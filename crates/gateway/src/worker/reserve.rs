//! The treasury as a hot wallet in front of a cold reserve. Withdrawals are
//! paid from the treasury, whose key the worker holds; what it holds above a
//! ceiling is moved to a reserve account whose key the worker never sees,
//! so a stolen treasury key reaches at most the ceiling. Topping the
//! treasury up from the reserve takes the reserve's own signers.

use std::time::Duration;

use fermah_pay_stellar_chain::authorization::sign_entry_with;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::stellar_xdr::{
    HostFunction, LedgerEntryData, ScAddress, ScVal, SorobanAddressCredentials,
    SorobanAuthorizationEntry, SorobanCredentials,
};
use fermah_pay_stellar_chain::transaction::account_id;
use fermah_pay_stellar_chain::usdc::{circle_usdc, trustline_key};
use fermah_pay_stellar_domain::AccountAddress;
use uuid::Uuid;

use super::{Worker, WorkerError, deployment, store};
use crate::submission::{Chain, Clock, EngineError, Kind};

/// How often the treasury's balance is read against the reserve settings.
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// Where the treasury's surplus goes, and the balances that decide when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reserve {
    /// The reserve account. It needs a USDC trustline, and its own signers
    /// (several, for a reserve) to spend from it.
    cold: AccountAddress,
    floor: i64,
    target: i64,
    ceiling: i64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("reserve settings need 0 <= floor <= target < ceiling, in USDC base units")]
pub struct ReserveSettingsError;

impl Reserve {
    /// Above `ceiling` the treasury is swept down to `target`, or to what
    /// held withdrawals still need if that is more; below `floor` it needs
    /// topping up, which is reported and left to the reserve's signers.
    pub fn new(
        cold: AccountAddress,
        floor: i64,
        target: i64,
        ceiling: i64,
    ) -> Result<Self, ReserveSettingsError> {
        if floor < 0 || floor > target || target >= ceiling {
            return Err(ReserveSettingsError);
        }
        Ok(Self { cold, floor, target, ceiling })
    }

    #[must_use]
    pub const fn cold(&self) -> &AccountAddress {
        &self.cold
    }
}

impl<C: Chain, K: Clock> Worker<C, K> {
    /// Sends the treasury's surplus above the ceiling to the reserve, at most
    /// once per check interval. One sweep is in flight at a time, and a new
    /// one waits until the previous one's treasury authorization has lapsed,
    /// so a copy of it included late cannot add to the next.
    pub(super) async fn sweep(&self) -> Result<Option<Uuid>, WorkerError> {
        let (Some(treasury), Some(reserve)) = (&self.treasury, &self.reserve) else {
            return Ok(None);
        };
        let now = self.engine.clock().now();
        let every = time::Duration::try_from(CHECK_EVERY).unwrap_or(time::Duration::MINUTE);
        {
            let mut checked = self
                .reserve_checked_at
                .lock()
                .map_err(|_| WorkerError::Corrupt("reserve check time poisoned"))?;
            if checked.is_some_and(|at| now < at + every) {
                return Ok(None);
            }
            *checked = Some(now);
        }
        let treasury_address = treasury.address();
        let Some(bound) = sqlx::query!(
            r#"
            SELECT contract_address, usdc_address, treasury_address
            FROM pay_stellar.ledger_contracts
            WHERE network = $1 AND operator_address = $2 AND treasury_address = $3
            LIMIT 1
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            treasury_address.as_str(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("find the treasury's deployment"))?
        else {
            return Ok(None);
        };
        let deployment =
            deployment(&bound.contract_address, &bound.usdc_address, &bound.treasury_address)?;

        let line = trustline_key(&treasury_address, &circle_usdc(self.network()));
        let snapshot = self.existing(vec![line.clone()]).await?;
        let Some(LedgerEntryData::Trustline(held_line)) = snapshot.entries.get(&line) else {
            tracing::warn!(treasury = %treasury_address, "the treasury has no USDC trustline");
            return Ok(None);
        };
        let balance = held_line.balance;
        #[allow(clippy::cast_precision_loss)]
        {
            metrics::gauge!("pay_stellar_hot_treasury_usdc").set(balance as f64);
            metrics::gauge!("pay_stellar_hot_treasury_floor_usdc").set(reserve.floor as f64);
        }
        if balance < reserve.floor {
            tracing::warn!(
                treasury = %treasury_address,
                balance,
                floor = reserve.floor,
                reserve = %reserve.cold,
                "the treasury is below its floor; top it up from the reserve"
            );
        }
        if balance <= reserve.ceiling {
            return Ok(None);
        }

        let last = sqlx::query!(
            r#"
            SELECT id, state FROM pay_stellar.submissions
            WHERE network = $1 AND kind = 'sweep'
            ORDER BY id DESC
            LIMIT 1
            "#,
            self.network().caip2(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("read the latest sweep"))?;
        if let Some(last) = last {
            if last.state == "installed" {
                return Ok(None);
            }
            let horizon = self.engine.authorization_horizon(last.id).await?;
            if snapshot.ledger <= i64::from(horizon) {
                return Ok(None);
            }
        }
        // What held withdrawals still need, and what withdrawals paid after
        // the ledger the balance was read at took out of it: that balance
        // still counts them, although they have left.
        let withdrawals = sqlx::query!(
            r#"
            SELECT
                COALESCE(sum(w.amount) FILTER (WHERE w.state IN ('signed', 'submitted')), 0)::bigint
                    AS "held!",
                COALESCE(sum(w.amount) FILTER (
                    WHERE w.state = 'confirmed' AND w.resolved_at > now() - interval '1 hour'
                      AND (s.ledger IS NULL OR s.ledger::bigint > $3)), 0)::bigint
                    AS "left_since!"
            FROM pay_stellar.withdrawals w
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = w.seller_deployment_id AND l.network = w.network
            LEFT JOIN pay_stellar.submissions s ON s.id = w.submission_id
            WHERE w.network = $1 AND l.treasury_address = $2
            "#,
            self.network().caip2(),
            treasury_address.as_str(),
            snapshot.ledger,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(store("read held withdrawals"))?;
        let amount = balance - withdrawals.left_since - reserve.target.max(withdrawals.held);
        if amount <= 0 {
            return Ok(None);
        }

        let latest = u32::try_from(snapshot.ledger)
            .map_err(|_| WorkerError::Corrupt("ledger beyond u32"))?;
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).map_err(WorkerError::Randomness)?;
        let unsigned = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
                address: ScAddress::Account(account_id(&treasury_address)),
                nonce: i64::from_le_bytes(nonce),
                signature_expiration_ledger: latest
                    .saturating_add(self.settings.operator_authorization_ledgers),
                signature: ScVal::Void,
            }),
            root_invocation: deployment
                .treasury_transfer_authorization(&reserve.cold, i128::from(amount)),
        };
        let entry = sign_entry_with(&unsigned, network_id(self.network()), treasury.as_ref())
            .await
            .inspect_err(|_| crate::submission::signing_failed("treasury"))
            .map_err(WorkerError::Signing)?;
        let function = HostFunction::InvokeContract(
            deployment.treasury_transfer_call(&reserve.cold, i128::from(amount)),
        );
        let prepared = match self.engine.prepare(Kind::Sweep, function, vec![entry]).await {
            Ok(prepared) => prepared,
            Err(error @ EngineError::SimulationFailed(_)) => {
                tracing::error!(
                    reserve = %reserve.cold,
                    error = %error,
                    "the network refused the sweep; check the reserve's USDC trustline"
                );
                return Ok(None);
            }
            Err(EngineError::SourceBusy { .. } | EngineError::NoFreeSource) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut conn = self.pool.acquire().await.map_err(store("acquire connection"))?;
        match self.engine.record(&mut conn, &prepared).await {
            Ok(_) => {}
            Err(EngineError::SourceBusy { .. } | EngineError::SweepInFlight) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        tracing::info!(
            submission_id = %prepared.id,
            treasury = %treasury_address,
            reserve = %reserve.cold,
            amount,
            balance,
            kept = balance - withdrawals.left_since - amount,
            "sweeping the treasury's surplus to the reserve"
        );
        Ok(Some(prepared.id))
    }
}
