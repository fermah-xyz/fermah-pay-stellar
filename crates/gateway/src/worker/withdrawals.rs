//! Withdrawals: each attempt carries the buyer's stored authorization and a
//! fresh treasury authorization, and the outcome is read from the contract's
//! withdrawal marker, as a deposit's is from its deposit marker.
//!
//! Every inclusion needs the buyer's authorization, which lapses at the
//! withdrawal's expiration ledger. So the held amount is returned only once
//! a node has seen that ledger pass without the marker: from then on no
//! attempt, nor any authorization a simulation exposed, can move the USDC.
//! Attempts in between are safe to repeat: the marker and the buyer's nonce
//! let at most one of them succeed.

use fermah_pay_stellar_chain::authorization::sign_entry_with;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{PrepaidDeployment, WithdrawIntent};
use fermah_pay_stellar_chain::stellar_xdr::{
    HostFunction, Limits, ReadXdr, ScAddress, ScVal, SorobanAddressCredentials,
    SorobanAuthorizationEntry, SorobanCredentials,
};
use fermah_pay_stellar_chain::transaction::account_id;
use fermah_pay_stellar_domain::{AccountAddress, ChainAddress};
use uuid::Uuid;

use super::{Worker, WorkerError, address, deployment, hash32, store};
use crate::submission::{Chain, Clock, EngineError, Kind, Resolution, State};

struct WithdrawalRow {
    id: Uuid,
    withdrawal_id: [u8; 32],
    expiration_ledger: i64,
    owner: ChainAddress,
    deployment: PrepaidDeployment,
}

/// Counts a withdrawal this call moved to `state`.
fn withdrawal_closed(state: &'static str, changed: i64) {
    if changed > 0 {
        metrics::counter!("pay_stellar_withdrawals_closed_total", "state" => state).increment(1);
    }
}

impl<C: Chain, K: Clock> Worker<C, K> {
    pub(super) async fn settle_withdrawal(
        &self,
        submission: Uuid,
        resolution: &Resolution,
    ) -> Result<(), WorkerError> {
        let Some(row) = sqlx::query!(
            r#"
            SELECT w.id, w.withdrawal_id, w.expiration_ledger, b.wallet_address,
                   l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.withdrawals w
            JOIN pay_stellar.buyers b ON b.id = w.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = w.seller_deployment_id AND l.network = w.network
            WHERE w.submission_id = $1 AND w.state = 'submitted'
            "#,
            submission,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("read submitted withdrawal"))?
        else {
            return Ok(());
        };
        let withdrawal = WithdrawalRow {
            id: row.id,
            withdrawal_id: hash32(row.withdrawal_id)?,
            expiration_ledger: row.expiration_ledger,
            owner: address(&row.wallet_address)?,
            deployment: deployment(
                &row.contract_address,
                &row.usdc_address,
                row.treasury_address.as_deref(),
            )?,
        };
        match resolution.state {
            State::Installed => Ok(()),
            State::Succeeded => self.confirm_withdrawal(withdrawal.id).await,
            // Never included, and the buyer's signature is still good: it is
            // sent again with a fresh treasury authorization.
            State::Expired if self.latest_ledger().await? <= withdrawal.expiration_ledger => {
                self.retry_withdrawal(withdrawal.id).await
            }
            // Failed, expired, or of unknown fate: once the buyer's signature
            // has lapsed the marker decides; until then the withdrawal may
            // still be included elsewhere, and the decision waits.
            State::Expired | State::Failed | State::Quarantined => {
                let closing = if resolution.state == State::Failed { "failed" } else { "expired" };
                self.conclude_withdrawals(&[withdrawal], closing).await
            }
        }
    }

    /// Decides withdrawals from the contract's marker: confirmed if it
    /// exists, `closing` with the amount returned if it is absent at a
    /// ledger after the buyer's authorization lapsed.
    async fn conclude_withdrawals(
        &self,
        withdrawals: &[WithdrawalRow],
        closing: &'static str,
    ) -> Result<(), WorkerError> {
        let keys = withdrawals
            .iter()
            .map(|w| w.deployment.withdrawal_key(&w.owner, &w.withdrawal_id))
            .collect();
        let snapshot = self.existing(keys).await?;
        for withdrawal in withdrawals {
            let key =
                withdrawal.deployment.withdrawal_key(&withdrawal.owner, &withdrawal.withdrawal_id);
            if snapshot.entries.contains_key(&key) {
                self.confirm_withdrawal(withdrawal.id).await?;
            } else if snapshot.ledger > withdrawal.expiration_ledger {
                self.close_withdrawal(
                    withdrawal.id,
                    closing,
                    "authorization lapsed; the contract never processed it",
                )
                .await?;
            }
        }
        Ok(())
    }

    /// The held amount has left: confirming changes no balance.
    async fn confirm_withdrawal(&self, id: Uuid) -> Result<(), WorkerError> {
        let confirmed = sqlx::query!(
            r#"
            UPDATE pay_stellar.withdrawals
            SET state = 'confirmed', resolved_at = now()
            WHERE id = $1 AND state IN ('signed', 'submitted')
            "#,
            id,
        )
        .execute(&self.pool)
        .await
        .map_err(store("confirm withdrawal"))?;
        withdrawal_closed("confirmed", i64::from(confirmed.rows_affected() > 0));
        Ok(())
    }

    /// Closes the withdrawal and returns a held amount in the same
    /// statement, so it is returned exactly once. A withdrawal never signed
    /// held nothing. The buyer row is locked before the withdrawal row, the
    /// order in which the API locks them when it stores a signature and
    /// holds the amount, so the two cannot deadlock.
    async fn close_withdrawal(
        &self,
        id: Uuid,
        state: &'static str,
        reason: &str,
    ) -> Result<(), WorkerError> {
        let mut tx = self.pool.begin().await.map_err(store("begin closing a withdrawal"))?;
        sqlx::query!(
            r#"
            SELECT b.id FROM pay_stellar.buyers b
            JOIN pay_stellar.withdrawals w ON w.buyer_id = b.id
            WHERE w.id = $1
            FOR UPDATE OF b
            "#,
            id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(store("lock the withdrawing buyer"))?;
        let closed = sqlx::query_scalar!(
            r#"
            WITH closed AS (
                UPDATE pay_stellar.withdrawals
                SET state = $2, last_error = $3, resolved_at = now()
                WHERE id = $1 AND state IN ('awaiting_signature', 'signed', 'submitted')
                RETURNING buyer_id, amount, signed_at IS NOT NULL AS held
            ), returned AS (
                UPDATE pay_stellar.buyers b
                SET available = b.available + closed.amount
                FROM closed WHERE b.id = closed.buyer_id AND closed.held
            )
            SELECT count(*) AS "closed!" FROM closed
            "#,
            id,
            state,
            reason,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(store("close withdrawal"))?;
        tx.commit().await.map_err(store("commit closing a withdrawal"))?;
        withdrawal_closed(state, closed);
        Ok(())
    }

    async fn retry_withdrawal(&self, id: Uuid) -> Result<(), WorkerError> {
        sqlx::query!(
            r#"
            UPDATE pay_stellar.withdrawals
            SET state = 'signed', submission_id = NULL,
                last_error = 'not included before its transaction expired; sending again'
            WHERE id = $1 AND state = 'submitted'
            "#,
            id,
        )
        .execute(&self.pool)
        .await
        .map_err(store("requeue withdrawal"))?;
        Ok(())
    }

    /// Withdrawals whose buyer authorization has lapsed without them being
    /// in flight. One never signed is closed at once: the worker creates a
    /// treasury authorization only for a signed withdrawal, so without one
    /// nothing could include it. A signed one may have been included by an
    /// earlier attempt, so its marker decides.
    pub(super) async fn conclude_lapsed_withdrawals(&self) -> Result<(), WorkerError> {
        let latest = self.latest_ledger().await?;
        let rows = sqlx::query!(
            r#"
            SELECT w.id, w.state, w.withdrawal_id, w.expiration_ledger, b.wallet_address,
                   l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.withdrawals w
            JOIN pay_stellar.buyers b ON b.id = w.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = w.seller_deployment_id AND l.network = w.network
            WHERE w.network = $1 AND l.operator_address = $2
              AND w.state IN ('awaiting_signature', 'signed') AND w.expiration_ledger < $3
            ORDER BY w.expiration_ledger
            LIMIT 100
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            latest,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("find lapsed withdrawals"))?;
        let mut signed = Vec::new();
        for row in rows {
            if row.state == "awaiting_signature" {
                self.close_withdrawal(row.id, "expired", "the buyer never signed it").await?;
                continue;
            }
            signed.push(WithdrawalRow {
                id: row.id,
                withdrawal_id: hash32(row.withdrawal_id)?,
                expiration_ledger: row.expiration_ledger,
                owner: address(&row.wallet_address)?,
                deployment: deployment(
                    &row.contract_address,
                    &row.usdc_address,
                    row.treasury_address.as_deref(),
                )?,
            });
        }
        self.conclude_withdrawals(&signed, "expired").await
    }

    /// Sends the oldest signed withdrawal this worker can co-sign: of a
    /// vault, with the operator's authorization; of a prepaid ledger whose
    /// treasury key it holds, with the treasury's. The co-signature is valid
    /// for as long as the operator's authorization of a batch.
    #[tracing::instrument(skip_all, fields(withdrawal_id, submission_id))]
    pub(super) async fn submit_withdrawal(&self) -> Result<Option<Uuid>, WorkerError> {
        let treasury_address = self.treasury.as_ref().map(|treasury| treasury.address());
        let latest = self.engine.chain().latest_ledger().await.map_err(WorkerError::Chain)?;
        let Some(row) = sqlx::query!(
            r#"
            SELECT w.id, w.amount, w.withdrawal_id, w.destination_address,
                   w.signed_authorization_xdr AS "signed_authorization_xdr!",
                   b.wallet_address, l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.withdrawals w
            JOIN pay_stellar.buyers b ON b.id = w.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = w.seller_deployment_id AND l.network = w.network
            WHERE w.network = $1 AND l.operator_address = $2
              AND (l.custody = 'vault' OR l.treasury_address = $3)
              AND w.state = 'signed' AND w.id <> ALL($4) AND w.expiration_ledger > $5
            ORDER BY w.created_at
            LIMIT 1
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            treasury_address.as_ref().map(AccountAddress::as_str),
            &self.set_aside_ids(),
            i64::from(latest),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("find signed withdrawal"))?
        else {
            return Ok(None);
        };
        let deployment =
            deployment(&row.contract_address, &row.usdc_address, row.treasury_address.as_deref())?;
        let intent = WithdrawIntent {
            owner: address(&row.wallet_address)?,
            amount: i128::from(row.amount),
            destination: address(&row.destination_address)?,
            withdrawal_id: hash32(row.withdrawal_id)?,
        };
        let owner_entry = SorobanAuthorizationEntry::from_xdr_base64(
            &row.signed_authorization_xdr,
            Limits::none(),
        )
        .map_err(|_| WorkerError::Corrupt("stored signed entry does not decode"))?;
        // The API verified this entry against the same withdrawal; a
        // mismatch here means the stored row changed, and nothing is signed
        // or sent for it.
        if owner_entry.root_invocation != deployment.owner_withdraw_authorization(&intent) {
            tracing::error!(withdrawal_id = %row.id, "signed entry does not authorize this withdrawal");
            self.set_aside(row.id);
            return Ok(None);
        }
        let (cosigner, cosigner_address, role) = match (&self.treasury, deployment.treasury()) {
            (_, None) => (
                &self.operator,
                self.operator_address.clone(),
                crate::submission::SigningRole::Operator,
            ),
            (Some(treasury), Some(_)) => {
                (treasury, treasury.address(), crate::submission::SigningRole::Treasury)
            }
            (None, Some(_)) => {
                return Err(WorkerError::Corrupt("withdrawal without its co-signer"));
            }
        };
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).map_err(WorkerError::Randomness)?;
        let unsigned = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
                address: ScAddress::Account(account_id(&cosigner_address)),
                nonce: i64::from_le_bytes(nonce),
                signature_expiration_ledger: latest
                    .saturating_add(self.settings.operator_authorization_ledgers),
                signature: ScVal::Void,
            }),
            root_invocation: deployment.cosigner_withdraw_authorization(&intent),
        };
        let cosigner_entry =
            sign_entry_with(&unsigned, network_id(self.network()), cosigner.as_ref())
                .await
                .inspect_err(|_| crate::submission::signing_failed(role))
                .map_err(WorkerError::Signing)?;
        let function = HostFunction::InvokeContract(deployment.withdraw_call(&intent));
        let prepared = match self
            .engine
            .prepare(Kind::Withdrawal, function, vec![owner_entry, cosigner_entry])
            .await
        {
            Ok(prepared) => prepared,
            Err(EngineError::RestoreRequired(restore)) => {
                match self.buyer_restore_refusal(row.id, &restore) {
                    None => return self.restore(&restore, row.id).await,
                    Some(reason) => {
                        tracing::warn!(withdrawal_id = %row.id, reason, "its restore is not sent");
                        self.note_withdrawal(row.id, &reason).await?;
                        self.set_aside(row.id);
                        return Ok(None);
                    }
                }
            }
            // The treasury may be short of USDC until it is topped up, or
            // the destination may lack a trustline; the withdrawal is tried
            // again after the pause until the buyer's signature lapses.
            Err(
                error
                @ (EngineError::SimulationFailed(_) | EngineError::ResourceFeeAboveCap { .. }),
            ) => {
                tracing::warn!(withdrawal_id = %row.id, error = %error, "network refused the withdrawal in simulation, or it costs too much");
                self.note_withdrawal(row.id, &error.to_string()).await?;
                self.set_aside(row.id);
                return Ok(None);
            }
            Err(EngineError::SourceBusy { .. } | EngineError::NoFreeSource) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let span = tracing::Span::current();
        span.record("withdrawal_id", tracing::field::display(row.id));
        span.record("submission_id", tracing::field::display(prepared.id));

        let mut tx = self.pool.begin().await.map_err(store("begin withdrawal submission"))?;
        match self.engine.record(&mut tx, &prepared).await {
            Ok(_) => {}
            Err(EngineError::SourceBusy { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let linked = sqlx::query!(
            r#"
            UPDATE pay_stellar.withdrawals
            SET state = 'submitted', submission_id = $2, last_error = NULL
            WHERE id = $1 AND state = 'signed'
            "#,
            row.id,
            prepared.id,
        )
        .execute(&mut *tx)
        .await
        .map_err(store("link withdrawal"))?;
        if linked.rows_affected() != 1 {
            return Ok(None);
        }
        tx.commit().await.map_err(store("commit withdrawal submission"))?;
        Ok(Some(prepared.id))
    }

    async fn note_withdrawal(&self, id: Uuid, error: &str) -> Result<(), WorkerError> {
        sqlx::query!(
            "UPDATE pay_stellar.withdrawals SET last_error = $2 WHERE id = $1 AND state = 'signed'",
            id,
            error,
        )
        .execute(&self.pool)
        .await
        .map_err(store("note withdrawal error"))?;
        Ok(())
    }
}
