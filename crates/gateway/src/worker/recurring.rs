//! Recurring charges: the buyer's signed mandates and revocations, sent like
//! deposits, and batches of the charges sellers ask for each period.
//!
//! A mandate or revocation is decided by its effect on the contract: the
//! buyer's mandate entry holds the mandate, or holds none. As with deposits,
//! a submission that did not succeed is not a verdict: the buyer's signed
//! entry can be included by anyone until it lapses. A successful one is
//! decided from a read at or after its ledger, so a node that trails the
//! network cannot make a fresh mandate look absent.
//!
//! A recurring charge holds nothing from the buyer's prepaid balance, so
//! settling it moves no balance: it records what the contract did. Like a
//! prepaid charge, it is decided from the batch's answer, then from the
//! attempt's record on the contract, and once that has lapsed from the
//! contract's `recurring` events.

use fermah_pay_stellar_chain::authorization::sign_entry_with;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{
    CHARGE_RECORD_GRACE, MAX_RECURRING_BATCH, MandateIntent, MandateTerms, PrepaidDeployment,
    RecurringChargeRequest, RecurringOutcome, recurring_outcomes, recurring_record, stored_mandate,
};
use fermah_pay_stellar_chain::stellar_xdr::{
    HostFunction, Limits, ReadXdr, ScAddress, ScVal, SorobanAddressCredentials,
    SorobanAuthorizationEntry, SorobanCredentials,
};
use fermah_pay_stellar_chain::transaction::account_id;
use fermah_pay_stellar_domain::AccountAddress;
use uuid::Uuid;

use super::{Snapshot, Worker, WorkerError, address, deployment, hash32, store};
use crate::events::{FoundRecurring, RecurringSearch, search_recurring};
use crate::submission::{Chain, Clock, EngineError, Kind, Resolution, State};

/// A mandate or revocation row and where its effect is read.
struct BuyerIntent {
    id: Uuid,
    expiration_ledger: i64,
    owner: AccountAddress,
    deployment: PrepaidDeployment,
}

/// The mandate the contract holds for `owner` in `snapshot`, if any.
fn held_mandate(snapshot: &Snapshot, intent: &BuyerIntent) -> Option<MandateTerms> {
    snapshot.entries.get(&intent.deployment.mandate_key(&intent.owner)).and_then(stored_mandate)
}

/// What a final batch means for one recurring charge it carried.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RecurringDecision {
    Charged,
    /// Nothing moved: the contract refused the charge, or it expired.
    Refused(RecurringOutcome),
    Quarantine {
        outcome: Option<RecurringOutcome>,
        reason: String,
    },
    /// Proven unprocessed and still within its last ledger.
    Requeue(String),
}

/// The decision an outcome the contract settled, or recorded, establishes;
/// `None` for `Duplicate`, which the attempt's record answers instead.
fn settled(outcome: RecurringOutcome) -> Option<RecurringDecision> {
    match outcome {
        RecurringOutcome::Charged => Some(RecurringDecision::Charged),
        RecurringOutcome::Duplicate => None,
        refused => Some(RecurringDecision::Refused(refused)),
    }
}

/// Counts a mandate or revocation this call moved to `state`.
fn closed(what: &'static str, state: &'static str, changed: u64) {
    if changed > 0 {
        metrics::counter!("pay_stellar_mandates_closed_total", "kind" => what, "state" => state)
            .increment(1);
    }
}

impl<C: Chain, K: Clock> Worker<C, K> {
    // ---- mandates ----------------------------------------------------------

    pub(super) async fn settle_mandate(
        &self,
        submission: Uuid,
        resolution: &Resolution,
    ) -> Result<(), WorkerError> {
        let Some(row) = sqlx::query!(
            r#"
            SELECT m.id, m.expiration_ledger, b.wallet_address,
                   l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.mandates m
            JOIN pay_stellar.buyers b ON b.id = m.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = m.seller_deployment_id AND l.network = m.network
            WHERE m.submission_id = $1 AND m.state = 'submitted'
            "#,
            submission,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("read submitted mandate"))?
        else {
            return Ok(());
        };
        let mandate = BuyerIntent {
            id: row.id,
            expiration_ledger: row.expiration_ledger,
            owner: address(&row.wallet_address)?,
            deployment: deployment(
                &row.contract_address,
                &row.usdc_address,
                &row.treasury_address,
            )?,
        };
        match resolution.state {
            State::Installed => Ok(()),
            State::Succeeded => {
                let included = i64::from(resolution.ledger.unwrap_or(i32::MAX));
                self.decide_mandates(&[mandate], Decide::Included(included)).await
            }
            State::Expired if self.latest_ledger().await? <= mandate.expiration_ledger => {
                self.resend("mandates", mandate.id).await
            }
            State::Expired | State::Failed | State::Quarantined => {
                let closing = if resolution.state == State::Failed { "failed" } else { "expired" };
                self.decide_mandates(&[mandate], Decide::Lapsing(closing)).await
            }
        }
    }

    /// Decides mandates from the buyer's mandate entry: active if it holds
    /// this mandate.
    async fn decide_mandates(
        &self,
        mandates: &[BuyerIntent],
        decide: Decide,
    ) -> Result<(), WorkerError> {
        let keys = mandates.iter().map(|m| m.deployment.mandate_key(&m.owner)).collect();
        let snapshot = self.existing(keys).await?;
        for mandate in mandates {
            let mandate_id = self.mandate_id(mandate.id).await?;
            match (held_mandate(&snapshot, mandate), decide) {
                (Some(held), _) if held.mandate_id == mandate_id => {
                    let starts_at = i64::try_from(held.start)
                        .map_err(|_| WorkerError::Corrupt("mandate start out of range"))?;
                    self.activate_mandate(mandate.id, starts_at).await?;
                }
                // Included, but the read is older than the inclusion.
                (_, Decide::Included(ledger)) if snapshot.ledger < ledger => {}
                // Included, and something replaced or revoked it since.
                (held, Decide::Included(_)) => {
                    let (state, reason) = match held {
                        Some(_) => ("replaced", "the contract holds a newer mandate of the buyer"),
                        None => ("revoked", "the contract holds no mandate of the buyer"),
                    };
                    self.close_mandate(mandate.id, state, reason).await?;
                }
                (_, Decide::Lapsing(closing)) if snapshot.ledger > mandate.expiration_ledger => {
                    self.close_mandate(
                        mandate.id,
                        closing,
                        "authorization lapsed; the contract never held it",
                    )
                    .await?;
                }
                (_, Decide::Lapsing(_)) => {}
            }
        }
        Ok(())
    }

    async fn mandate_id(&self, id: Uuid) -> Result<[u8; 32], WorkerError> {
        let raw =
            sqlx::query_scalar!("SELECT mandate_id FROM pay_stellar.mandates WHERE id = $1", id)
                .fetch_one(&self.pool)
                .await
                .map_err(store("read mandate id"))?;
        hash32(raw)
    }

    /// Makes the mandate the buyer's active one and closes the previous one
    /// as replaced, in one transaction: the contract holds one mandate per
    /// buyer, and so does the partial unique index.
    async fn activate_mandate(&self, id: Uuid, starts_at: i64) -> Result<(), WorkerError> {
        let mut tx = self.pool.begin().await.map_err(store("begin activating a mandate"))?;
        let open = sqlx::query_scalar!(
            r#"
            SELECT buyer_id FROM pay_stellar.mandates
            WHERE id = $1 AND state IN ('awaiting_signature', 'signed', 'submitted')
            FOR UPDATE
            "#,
            id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(store("lock mandate"))?;
        let Some(buyer) = open else { return Ok(()) };
        let replaced = sqlx::query!(
            r#"
            UPDATE pay_stellar.mandates
            SET state = 'replaced', closed_at = now(),
                last_error = 'a newer mandate of the buyer became active'
            WHERE buyer_id = $1 AND state = 'active' AND id <> $2
            "#,
            buyer,
            id,
        )
        .execute(&mut *tx)
        .await
        .map_err(store("replace previous mandate"))?;
        sqlx::query!(
            r#"
            UPDATE pay_stellar.mandates
            SET state = 'active', starts_at = $2, activated_at = now(), last_error = NULL
            WHERE id = $1
            "#,
            id,
            starts_at,
        )
        .execute(&mut *tx)
        .await
        .map_err(store("activate mandate"))?;
        tx.commit().await.map_err(store("commit activating a mandate"))?;
        closed("mandate", "replaced", replaced.rows_affected());
        closed("mandate", "active", 1);
        Ok(())
    }

    async fn close_mandate(
        &self,
        id: Uuid,
        state: &'static str,
        reason: &str,
    ) -> Result<(), WorkerError> {
        let changed = sqlx::query!(
            r#"
            UPDATE pay_stellar.mandates
            SET state = $2, last_error = $3, closed_at = now()
            WHERE id = $1 AND state IN ('awaiting_signature', 'signed', 'submitted', 'active')
            "#,
            id,
            state,
            reason,
        )
        .execute(&self.pool)
        .await
        .map_err(store("close mandate"))?;
        closed("mandate", state, changed.rows_affected());
        Ok(())
    }

    /// Puts a mandate or revocation whose transaction expired unincluded,
    /// and whose buyer signature is still good, back in line.
    async fn resend(&self, table: &'static str, id: Uuid) -> Result<(), WorkerError> {
        let reason = "not included before its transaction expired; sending again";
        let query = match table {
            "mandates" => sqlx::query!(
                r#"
                UPDATE pay_stellar.mandates SET state = 'signed', submission_id = NULL,
                    last_error = $2
                WHERE id = $1 AND state = 'submitted'
                "#,
                id,
                reason,
            ),
            _ => sqlx::query!(
                r#"
                UPDATE pay_stellar.revocations SET state = 'signed', submission_id = NULL,
                    last_error = $2
                WHERE id = $1 AND state = 'submitted'
                "#,
                id,
                reason,
            ),
        };
        query.execute(&self.pool).await.map_err(store("requeue buyer authorization"))?;
        Ok(())
    }

    /// Mandates whose buyer authorization lapsed without them being in
    /// flight. One never signed is closed at once; a signed one may have been
    /// included by the buyer, so the contract decides.
    pub(super) async fn conclude_lapsed_mandates(&self) -> Result<(), WorkerError> {
        let latest = self.latest_ledger().await?;
        let rows = sqlx::query!(
            r#"
            SELECT m.id, m.state, m.expiration_ledger, b.wallet_address,
                   l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.mandates m
            JOIN pay_stellar.buyers b ON b.id = m.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = m.seller_deployment_id AND l.network = m.network
            WHERE m.network = $1 AND l.operator_address = $2
              AND m.state IN ('awaiting_signature', 'signed') AND m.expiration_ledger < $3
            ORDER BY m.expiration_ledger
            LIMIT 100
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            latest,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("find lapsed mandates"))?;
        let mut signed = Vec::new();
        for row in rows {
            if row.state == "awaiting_signature" {
                self.close_mandate(row.id, "expired", "the buyer never signed it").await?;
                continue;
            }
            signed.push(BuyerIntent {
                id: row.id,
                expiration_ledger: row.expiration_ledger,
                owner: address(&row.wallet_address)?,
                deployment: deployment(
                    &row.contract_address,
                    &row.usdc_address,
                    &row.treasury_address,
                )?,
            });
        }
        self.decide_mandates(&signed, Decide::Lapsing("expired")).await
    }

    /// Active mandates past their last ledger, or whose last period has
    /// ended in ledger time: no charge for them can succeed any more.
    pub(super) async fn end_mandates(&self) -> Result<(), WorkerError> {
        let info = self.engine.chain().latest_ledger_info().await.map_err(WorkerError::Chain)?;
        let ended = sqlx::query!(
            r#"
            UPDATE pay_stellar.mandates m
            SET state = 'ended', closed_at = now(),
                last_error = 'past its last ledger or its last period'
            FROM pay_stellar.ledger_contracts l
            WHERE m.state = 'active' AND m.network = $1
              AND l.seller_deployment_id = m.seller_deployment_id AND l.network = m.network
              AND l.operator_address = $2
              AND (m.live_until < $3 OR m.starts_at + m.period_secs * m.cycles <= $4)
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            i64::from(info.sequence),
            info.close_time,
        )
        .execute(&self.pool)
        .await
        .map_err(store("end mandates"))?;
        closed("mandate", "ended", ended.rows_affected());
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(mandate_id, submission_id))]
    pub(super) async fn submit_mandate(&self) -> Result<Option<Uuid>, WorkerError> {
        let Some(row) = sqlx::query!(
            r#"
            SELECT m.id, m.mandate_id, m.amount, m.period_secs, m.cycles, m.live_until,
                   m.signed_authorization_xdr AS "signed_authorization_xdr!",
                   b.wallet_address, l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.mandates m
            JOIN pay_stellar.buyers b ON b.id = m.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = m.seller_deployment_id AND l.network = m.network
            WHERE m.network = $1 AND l.operator_address = $2 AND m.state = 'signed'
              AND m.id <> ALL($3)
            ORDER BY m.created_at
            LIMIT 1
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            &self.set_aside_ids(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("find signed mandate"))?
        else {
            return Ok(None);
        };
        let deployment =
            deployment(&row.contract_address, &row.usdc_address, &row.treasury_address)?;
        let corrupt = |what| WorkerError::Corrupt(what);
        let intent = MandateIntent {
            owner: address(&row.wallet_address)?,
            mandate_id: hash32(row.mandate_id)?,
            amount: i128::from(row.amount),
            period_secs: u64::try_from(row.period_secs).map_err(|_| corrupt("mandate period"))?,
            cycles: u32::try_from(row.cycles).map_err(|_| corrupt("mandate cycles"))?,
            live_until: u32::try_from(row.live_until)
                .map_err(|_| corrupt("mandate last ledger"))?,
        };
        let entry = SorobanAuthorizationEntry::from_xdr_base64(
            &row.signed_authorization_xdr,
            Limits::none(),
        )
        .map_err(|_| corrupt("stored signed entry does not decode"))?;
        // The API verified this entry against the same mandate; a mismatch
        // means the stored row changed, and nothing is sent for it.
        if entry.root_invocation != deployment.authorize_recurring_authorization(&intent) {
            tracing::error!(mandate_id = %row.id, "signed entry does not authorize this mandate");
            self.set_aside(row.id);
            return Ok(None);
        }
        let function = HostFunction::InvokeContract(deployment.authorize_recurring_call(&intent));
        self.send_buyer_intent(Kind::Mandate, "mandates", row.id, function, entry).await
    }

    /// Prepares, records and links one buyer-signed mandate or revocation.
    async fn send_buyer_intent(
        &self,
        kind: Kind,
        table: &'static str,
        id: Uuid,
        function: HostFunction,
        entry: SorobanAuthorizationEntry,
    ) -> Result<Option<Uuid>, WorkerError> {
        let prepared = match self.engine.prepare(kind, function, vec![entry]).await {
            Ok(prepared) => prepared,
            Err(EngineError::RestoreRequired(restore)) => return self.restore(&restore, id).await,
            Err(error @ EngineError::SimulationFailed(_)) => {
                tracing::warn!(id = %id, table, error = %error, "network refused it in simulation");
                self.note(table, id, &error.to_string()).await?;
                self.set_aside(id);
                return Ok(None);
            }
            Err(EngineError::SourceBusy { .. } | EngineError::NoFreeSource) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let span = tracing::Span::current();
        span.record("submission_id", tracing::field::display(prepared.id));
        let mut tx = self.pool.begin().await.map_err(store("begin buyer intent submission"))?;
        match self.engine.record(&mut tx, &prepared).await {
            Ok(_) => {}
            Err(EngineError::SourceBusy { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let linked = match table {
            "mandates" => sqlx::query!(
                r#"
                UPDATE pay_stellar.mandates SET state = 'submitted', submission_id = $2,
                    last_error = NULL
                WHERE id = $1 AND state = 'signed'
                "#,
                id,
                prepared.id,
            ),
            _ => sqlx::query!(
                r#"
                UPDATE pay_stellar.revocations SET state = 'submitted', submission_id = $2,
                    last_error = NULL
                WHERE id = $1 AND state = 'signed'
                "#,
                id,
                prepared.id,
            ),
        }
        .execute(&mut *tx)
        .await
        .map_err(store("link buyer intent"))?;
        if linked.rows_affected() != 1 {
            return Ok(None);
        }
        tx.commit().await.map_err(store("commit buyer intent submission"))?;
        Ok(Some(prepared.id))
    }

    async fn note(&self, table: &'static str, id: Uuid, error: &str) -> Result<(), WorkerError> {
        match table {
            "mandates" => sqlx::query!(
                "UPDATE pay_stellar.mandates SET last_error = $2 WHERE id = $1 AND state = 'signed'",
                id,
                error,
            ),
            _ => sqlx::query!(
                "UPDATE pay_stellar.revocations SET last_error = $2 WHERE id = $1 AND state = 'signed'",
                id,
                error,
            ),
        }
        .execute(&self.pool)
        .await
        .map_err(store("note buyer intent error"))?;
        Ok(())
    }

    // ---- revocations -------------------------------------------------------

    pub(super) async fn settle_revocation(
        &self,
        submission: Uuid,
        resolution: &Resolution,
    ) -> Result<(), WorkerError> {
        let Some(row) = sqlx::query!(
            r#"
            SELECT r.id, r.expiration_ledger, b.wallet_address,
                   l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.revocations r
            JOIN pay_stellar.buyers b ON b.id = r.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = r.seller_deployment_id AND l.network = r.network
            WHERE r.submission_id = $1 AND r.state = 'submitted'
            "#,
            submission,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("read submitted revocation"))?
        else {
            return Ok(());
        };
        let revocation = BuyerIntent {
            id: row.id,
            expiration_ledger: row.expiration_ledger,
            owner: address(&row.wallet_address)?,
            deployment: deployment(
                &row.contract_address,
                &row.usdc_address,
                &row.treasury_address,
            )?,
        };
        match resolution.state {
            State::Installed => Ok(()),
            State::Succeeded => {
                let included = i64::from(resolution.ledger.unwrap_or(i32::MAX));
                self.decide_revocations(&[revocation], Decide::Included(included)).await
            }
            State::Expired if self.latest_ledger().await? <= revocation.expiration_ledger => {
                self.resend("revocations", revocation.id).await
            }
            State::Expired | State::Failed | State::Quarantined => {
                let closing = if resolution.state == State::Failed { "failed" } else { "expired" };
                self.decide_revocations(&[revocation], Decide::Lapsing(closing)).await
            }
        }
    }

    /// Decides revocations from the buyer's mandate entry: confirmed once it
    /// holds no mandate. The buyer's active mandate, unless the contract
    /// still holds exactly it (authorized after this revocation), is then
    /// recorded as revoked in the same transaction.
    async fn decide_revocations(
        &self,
        revocations: &[BuyerIntent],
        decide: Decide,
    ) -> Result<(), WorkerError> {
        let keys = revocations.iter().map(|r| r.deployment.mandate_key(&r.owner)).collect();
        let snapshot = self.existing(keys).await?;
        for revocation in revocations {
            let held = held_mandate(&snapshot, revocation);
            match decide {
                Decide::Included(ledger) if snapshot.ledger < ledger => {}
                Decide::Included(_) => self.confirm_revocation(revocation.id, held).await?,
                Decide::Lapsing(_) if held.is_none() => {
                    self.confirm_revocation(revocation.id, None).await?;
                }
                Decide::Lapsing(closing) if snapshot.ledger > revocation.expiration_ledger => {
                    self.close_revocation(revocation.id, closing).await?;
                }
                Decide::Lapsing(_) => {}
            }
        }
        Ok(())
    }

    async fn confirm_revocation(
        &self,
        id: Uuid,
        held: Option<MandateTerms>,
    ) -> Result<(), WorkerError> {
        let held_id = held.map(|h| h.mandate_id.to_vec());
        let mut tx = self.pool.begin().await.map_err(store("begin confirming a revocation"))?;
        let buyer = sqlx::query_scalar!(
            r#"
            UPDATE pay_stellar.revocations SET state = 'confirmed', resolved_at = now()
            WHERE id = $1 AND state IN ('awaiting_signature', 'signed', 'submitted')
            RETURNING buyer_id
            "#,
            id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(store("confirm revocation"))?;
        let Some(buyer) = buyer else { return Ok(()) };
        let revoked = sqlx::query!(
            r#"
            UPDATE pay_stellar.mandates
            SET state = 'revoked', closed_at = now(), last_error = 'the buyer revoked it'
            WHERE buyer_id = $1 AND state = 'active'
              AND mandate_id IS DISTINCT FROM $2
            "#,
            buyer,
            held_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(store("revoke mandate"))?;
        tx.commit().await.map_err(store("commit confirming a revocation"))?;
        closed("revocation", "confirmed", 1);
        closed("mandate", "revoked", revoked.rows_affected());
        Ok(())
    }

    async fn close_revocation(&self, id: Uuid, state: &'static str) -> Result<(), WorkerError> {
        let changed = sqlx::query!(
            r#"
            UPDATE pay_stellar.revocations
            SET state = $2, resolved_at = now(),
                last_error = 'authorization lapsed; the contract still holds a mandate'
            WHERE id = $1 AND state IN ('awaiting_signature', 'signed', 'submitted')
            "#,
            id,
            state,
        )
        .execute(&self.pool)
        .await
        .map_err(store("close revocation"))?;
        closed("revocation", state, changed.rows_affected());
        Ok(())
    }

    pub(super) async fn conclude_lapsed_revocations(&self) -> Result<(), WorkerError> {
        let latest = self.latest_ledger().await?;
        let rows = sqlx::query!(
            r#"
            SELECT r.id, r.state, r.expiration_ledger, b.wallet_address,
                   l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.revocations r
            JOIN pay_stellar.buyers b ON b.id = r.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = r.seller_deployment_id AND l.network = r.network
            WHERE r.network = $1 AND l.operator_address = $2
              AND r.state IN ('awaiting_signature', 'signed') AND r.expiration_ledger < $3
            ORDER BY r.expiration_ledger
            LIMIT 100
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            latest,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("find lapsed revocations"))?;
        let mut signed = Vec::new();
        for row in rows {
            if row.state == "awaiting_signature" {
                self.close_revocation(row.id, "expired").await?;
                continue;
            }
            signed.push(BuyerIntent {
                id: row.id,
                expiration_ledger: row.expiration_ledger,
                owner: address(&row.wallet_address)?,
                deployment: deployment(
                    &row.contract_address,
                    &row.usdc_address,
                    &row.treasury_address,
                )?,
            });
        }
        self.decide_revocations(&signed, Decide::Lapsing("expired")).await
    }

    #[tracing::instrument(skip_all, fields(revocation_id, submission_id))]
    pub(super) async fn submit_revocation(&self) -> Result<Option<Uuid>, WorkerError> {
        let Some(row) = sqlx::query!(
            r#"
            SELECT r.id, r.signed_authorization_xdr AS "signed_authorization_xdr!",
                   b.wallet_address, l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.revocations r
            JOIN pay_stellar.buyers b ON b.id = r.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = r.seller_deployment_id AND l.network = r.network
            WHERE r.network = $1 AND l.operator_address = $2 AND r.state = 'signed'
              AND r.id <> ALL($3)
            ORDER BY r.created_at
            LIMIT 1
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            &self.set_aside_ids(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("find signed revocation"))?
        else {
            return Ok(None);
        };
        let deployment =
            deployment(&row.contract_address, &row.usdc_address, &row.treasury_address)?;
        let owner = address(&row.wallet_address)?;
        let entry = SorobanAuthorizationEntry::from_xdr_base64(
            &row.signed_authorization_xdr,
            Limits::none(),
        )
        .map_err(|_| WorkerError::Corrupt("stored signed entry does not decode"))?;
        if entry.root_invocation != deployment.revoke_recurring_authorization(&owner) {
            tracing::error!(revocation_id = %row.id, "signed entry does not authorize this revocation");
            self.set_aside(row.id);
            return Ok(None);
        }
        let function = HostFunction::InvokeContract(deployment.revoke_recurring_call(&owner));
        self.send_buyer_intent(Kind::Revocation, "revocations", row.id, function, entry).await
    }

    // ---- recurring charges -------------------------------------------------

    pub(super) async fn settle_recurring(
        &self,
        submission: Uuid,
        resolution: &Resolution,
    ) -> Result<(), WorkerError> {
        let rows = sqlx::query!(
            r#"
            SELECT r.id, r.charge_id, r.last_ledger, r.amount, r.batch_index AS "batch_index!",
                   b.wallet_address, l.contract_address, l.usdc_address, l.treasury_address,
                   (SELECT count(*) FROM pay_stellar.recurring_charges a
                    WHERE a.submission_id = r.submission_id) AS "batch_size!"
            FROM pay_stellar.recurring_charges r
            JOIN pay_stellar.buyers b ON b.id = r.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = r.seller_deployment_id AND l.network = r.network
            WHERE r.submission_id = $1 AND r.state = 'submitted'
            ORDER BY r.batch_index
            "#,
            submission,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("read submitted recurring charges"))?;
        let Some(first) = rows.first() else { return Ok(()) };
        let deployment =
            deployment(&first.contract_address, &first.usdc_address, &first.treasury_address)?;

        let mut decisions: Vec<(Uuid, RecurringDecision)> = Vec::new();
        let mut from_records = Vec::new();
        match resolution.state {
            State::Installed => return Ok(()),
            State::Succeeded => {
                match resolution.return_value.as_ref().and_then(recurring_outcomes) {
                    Some(outcomes)
                        if i64::try_from(outcomes.len()).ok() == Some(first.batch_size) =>
                    {
                        for row in &rows {
                            let outcome = usize::try_from(row.batch_index)
                                .ok()
                                .and_then(|i| outcomes.get(i).copied())
                                .ok_or(WorkerError::Corrupt("batch index outside the batch"))?;
                            match settled(outcome) {
                                Some(decision) => decisions.push((row.id, decision)),
                                None => from_records.push(row),
                            }
                        }
                    }
                    // Included and successful, but the answer is unreadable:
                    // every attempt it applied left a record.
                    _ => from_records.extend(rows.iter()),
                }
            }
            State::Failed | State::Expired | State::Quarantined => from_records.extend(rows.iter()),
        }

        if !from_records.is_empty() {
            let applied = resolution.state == State::Succeeded;
            let horizon = i64::from(self.engine.authorization_horizon(submission).await?);
            let owners = from_records
                .iter()
                .map(|row| Ok((address(&row.wallet_address)?, hash32(row.charge_id.clone())?)))
                .collect::<Result<Vec<_>, WorkerError>>()?;
            let keys = owners
                .iter()
                .map(|(owner, id)| deployment.recurring_record_key(owner, id))
                .collect();
            let snapshot = self.existing(keys).await?;
            for (row, (owner, id)) in from_records.iter().zip(&owners) {
                let record = snapshot.entries.get(&deployment.recurring_record_key(owner, id));
                let decision = match record {
                    Some(entry) => {
                        let outcome = recurring_record(entry)
                            .ok_or(WorkerError::Corrupt("recurring record does not decode"))?;
                        settled(outcome).ok_or(WorkerError::Corrupt("a record holds duplicate"))?
                    }
                    None if applied => RecurringDecision::Quarantine {
                        outcome: Some(RecurringOutcome::Duplicate),
                        reason: format!(
                            "submission {submission} answered duplicate or unreadably, and the contract holds no record at ledger {}",
                            snapshot.ledger
                        ),
                    },
                    None if snapshot.ledger <= horizon => continue,
                    None if snapshot.ledger <= row.last_ledger => {
                        RecurringDecision::Requeue(format!(
                            "submission {submission} did not apply it; no record at ledger {}",
                            snapshot.ledger
                        ))
                    }
                    None if snapshot.ledger <= row.last_ledger + i64::from(CHARGE_RECORD_GRACE) => {
                        RecurringDecision::Refused(RecurringOutcome::Expired)
                    }
                    None => match self
                        .decide_recurring_from_events(
                            submission,
                            &deployment,
                            owner,
                            id,
                            row.amount,
                            row.last_ledger,
                            snapshot.ledger,
                        )
                        .await?
                    {
                        Some(decision) => decision,
                        None => continue,
                    },
                };
                decisions.push((row.id, decision));
            }
        }
        self.apply_recurring_decisions(&decisions).await
    }

    /// Decides an attempt whose record has lapsed from the contract's
    /// `recurring` events between the ledger its batch was authorized at and
    /// its last ledger, as prepaid charges are.
    #[allow(clippy::too_many_arguments)]
    async fn decide_recurring_from_events(
        &self,
        submission: Uuid,
        deployment: &PrepaidDeployment,
        owner: &AccountAddress,
        charge_id: &[u8; 32],
        amount: i64,
        last_ledger: i64,
        read_at: i64,
    ) -> Result<Option<RecurringDecision>, WorkerError> {
        let lapsed = format!(
            "no record at ledger {read_at}, past the ledger its record would have lived to"
        );
        let quarantine = |reason: String| RecurringDecision::Quarantine { outcome: None, reason };
        let Some(from) = self.engine.authorized_from(submission).await? else {
            return Ok(Some(quarantine(format!(
                "{lapsed}; the ledger its batch was authorized at is not recorded, so its events cannot be searched"
            ))));
        };
        let to = u32::try_from(last_ledger)
            .map_err(|_| WorkerError::Corrupt("recurring last ledger out of range"))?;
        let search =
            search_recurring(self.engine.chain(), &deployment.contract, owner, charge_id, from, to)
                .await
                .map_err(WorkerError::Chain)?;
        let entries: Vec<FoundRecurring> = match search {
            RecurringSearch::Behind { .. } => return Ok(None),
            RecurringSearch::Pruned { oldest } => {
                return Ok(Some(quarantine(format!(
                    "{lapsed}; the node retains events only from ledger {oldest}, so ledgers {from} to {} cannot be searched",
                    oldest.saturating_sub(1)
                ))));
            }
            RecurringSearch::Unreadable { event } => {
                return Ok(Some(quarantine(format!(
                    "{lapsed}; recurring event {event} does not decode"
                ))));
            }
            RecurringSearch::Complete { entries, .. } => entries,
        };
        let Some(found) = entries.iter().find(|found| found.settles()) else {
            return Ok(Some(if entries.iter().any(FoundRecurring::is_duplicate) {
                RecurringDecision::Quarantine {
                    outcome: Some(RecurringOutcome::Duplicate),
                    reason: format!(
                        "{lapsed}; ledgers {from} to {to} show the attempt only as a duplicate"
                    ),
                }
            } else {
                RecurringDecision::Refused(RecurringOutcome::Expired)
            }));
        };
        if found.entry.amount != i128::from(amount) {
            return Ok(Some(quarantine(format!(
                "recurring event {} settled the attempt for {}, not {amount}",
                found.event, found.entry.amount
            ))));
        }
        Ok(Some(settled(found.entry.outcome).unwrap_or_else(|| {
            quarantine(format!("recurring event {} answered duplicate", found.event))
        })))
    }

    /// Admitted recurring charges past their last ledger were never sent
    /// (an admitted charge is not in flight) and never can be applied.
    pub(super) async fn expire_recurring(&self) -> Result<(), WorkerError> {
        let latest = self.latest_ledger().await?;
        sqlx::query!(
            r#"
            UPDATE pay_stellar.recurring_charges r
            SET state = 'refused', outcome = 'expired', settled_at = now(),
                last_error = 'not settled before its last ledger'
            FROM pay_stellar.ledger_contracts l
            WHERE r.state = 'admitted' AND r.last_ledger < $1 AND r.network = $2
              AND l.seller_deployment_id = r.seller_deployment_id AND l.network = r.network
              AND l.operator_address = $3
            "#,
            latest,
            self.network().caip2(),
            self.operator_address.as_str(),
        )
        .execute(&self.pool)
        .await
        .map_err(store("expire recurring charges"))?;
        Ok(())
    }

    /// Applies each decision in the statement that moves the charge out of
    /// `submitted`. A refusal that shows the mandate is gone, or over, closes
    /// the mandate row in the same transaction.
    async fn apply_recurring_decisions(
        &self,
        decisions: &[(Uuid, RecurringDecision)],
    ) -> Result<(), WorkerError> {
        let mut tx = self.pool.begin().await.map_err(store("begin recurring settlement"))?;
        let mut applied: Vec<&'static str> = Vec::new();
        for (id, decision) in decisions {
            let (result, changed) = match decision {
                RecurringDecision::Charged | RecurringDecision::Refused(_) => {
                    let (state, outcome) = match decision {
                        RecurringDecision::Refused(outcome) => ("refused", *outcome),
                        _ => ("charged", RecurringOutcome::Charged),
                    };
                    let mandate = sqlx::query_scalar!(
                        r#"
                        UPDATE pay_stellar.recurring_charges
                        SET state = $2, outcome = $3, settled_at = now()
                        WHERE id = $1 AND state = 'submitted'
                        RETURNING mandate_row_id
                        "#,
                        id,
                        state,
                        outcome.token(),
                    )
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(store("settle recurring charge"))?;
                    let ending = match outcome {
                        RecurringOutcome::MandateExpired => {
                            Some(("ended", "the contract answered mandate_expired"))
                        }
                        RecurringOutcome::NoMandate => {
                            Some(("revoked", "the contract no longer holds this mandate"))
                        }
                        _ => None,
                    };
                    if let (Some(mandate), Some((state, reason))) = (mandate, ending) {
                        sqlx::query!(
                            r#"
                            UPDATE pay_stellar.mandates
                            SET state = $2, closed_at = now(), last_error = $3
                            WHERE id = $1 AND state = 'active'
                            "#,
                            mandate,
                            state,
                            reason,
                        )
                        .execute(&mut *tx)
                        .await
                        .map_err(store("close mandate after a refusal"))?;
                    }
                    (outcome.token(), u64::from(mandate.is_some()))
                }
                RecurringDecision::Quarantine { outcome, reason } => {
                    tracing::error!(recurring_charge_id = %id, reason, "recurring charge quarantined");
                    let changed = sqlx::query!(
                        r#"
                        UPDATE pay_stellar.recurring_charges
                        SET state = 'quarantined', outcome = $2, last_error = $3,
                            settled_at = now()
                        WHERE id = $1 AND state = 'submitted'
                        "#,
                        id,
                        outcome.map(RecurringOutcome::token),
                        reason,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(store("quarantine recurring charge"))?
                    .rows_affected();
                    ("quarantined", changed)
                }
                RecurringDecision::Requeue(reason) => {
                    let changed = sqlx::query!(
                        r#"
                        UPDATE pay_stellar.recurring_charges
                        SET state = 'admitted', submission_id = NULL, batch_index = NULL,
                            last_error = $2
                        WHERE id = $1 AND state = 'submitted'
                        "#,
                        id,
                        reason,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(store("requeue recurring charge"))?
                    .rows_affected();
                    ("requeued", changed)
                }
            };
            if changed > 0 {
                applied.push(result);
            }
        }
        tx.commit().await.map_err(store("commit recurring settlement"))?;
        for result in applied {
            metrics::counter!("pay_stellar_recurring_settled_total", "result" => result)
                .increment(1);
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id, submission_id, charges))]
    pub(super) async fn submit_recurring(&self) -> Result<Option<Uuid>, WorkerError> {
        let latest = self.engine.chain().latest_ledger().await.map_err(WorkerError::Chain)?;
        let sendable =
            i64::from(latest.saturating_add(self.settings.operator_authorization_ledgers));
        let Some(target) = sqlx::query!(
            r#"
            SELECT l.seller_deployment_id, l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.ledger_contracts l
            JOIN LATERAL (
                SELECT min(r.created_at) AS oldest FROM pay_stellar.recurring_charges r
                WHERE r.seller_deployment_id = l.seller_deployment_id AND r.state = 'admitted'
                  AND r.last_ledger > $4
            ) o ON o.oldest IS NOT NULL
            WHERE l.network = $1 AND l.operator_address = $2
              AND l.seller_deployment_id <> ALL($3)
            ORDER BY o.oldest
            LIMIT 1
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            &self.set_aside_ids(),
            sendable,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("find deployment with admitted recurring charges"))?
        else {
            return Ok(None);
        };
        let deployment =
            deployment(&target.contract_address, &target.usdc_address, &target.treasury_address)?;
        let limit = i64::try_from(self.settings.max_batch.min(MAX_RECURRING_BATCH)).unwrap_or(1);
        let batch = sqlx::query!(
            r#"
            SELECT r.id, r.charge_id, r.cycle, r.amount, r.last_ledger, m.mandate_id,
                   b.wallet_address
            FROM pay_stellar.recurring_charges r
            JOIN pay_stellar.mandates m ON m.id = r.mandate_row_id
            JOIN pay_stellar.buyers b ON b.id = r.buyer_id
            WHERE r.seller_deployment_id = $1 AND r.state = 'admitted' AND r.last_ledger > $3
            ORDER BY r.created_at, r.id
            LIMIT $2
            "#,
            target.seller_deployment_id,
            limit,
            sendable,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("read admitted recurring charges"))?;
        if batch.is_empty() {
            return Ok(None);
        }
        let corrupt = |what| WorkerError::Corrupt(what);
        let requests = batch
            .iter()
            .map(|r| {
                Ok(RecurringChargeRequest {
                    owner: address(&r.wallet_address)?,
                    charge_id: hash32(r.charge_id.clone())?,
                    mandate_id: hash32(r.mandate_id.clone())?,
                    cycle: u32::try_from(r.cycle).map_err(|_| corrupt("recurring period"))?,
                    amount: i128::from(r.amount),
                    last_ledger: u32::try_from(r.last_ledger)
                        .map_err(|_| corrupt("recurring last ledger"))?,
                })
            })
            .collect::<Result<Vec<_>, WorkerError>>()?;
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).map_err(WorkerError::Randomness)?;
        let unsigned = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
                address: ScAddress::Account(account_id(&self.operator_address)),
                nonce: i64::from_le_bytes(nonce),
                signature_expiration_ledger: latest
                    .saturating_add(self.settings.operator_authorization_ledgers),
                signature: ScVal::Void,
            }),
            root_invocation: deployment.charge_recurring_batch_authorization(&requests),
        };
        let signed = sign_entry_with(&unsigned, network_id(self.network()), self.operator.as_ref())
            .await
            .inspect_err(|_| crate::submission::signing_failed("operator"))
            .map_err(WorkerError::Signing)?;
        let function =
            HostFunction::InvokeContract(deployment.charge_recurring_batch_call(&requests));
        let prepared = match self.engine.prepare(Kind::RecurringBatch, function, vec![signed]).await
        {
            Ok(prepared) => prepared,
            Err(EngineError::RestoreRequired(restore)) => {
                return self.restore(&restore, target.seller_deployment_id).await;
            }
            Err(error @ EngineError::SimulationFailed(_)) => {
                tracing::warn!(
                    seller_deployment_id = %target.seller_deployment_id,
                    error = %error,
                    "network refused the recurring batch in simulation"
                );
                self.set_aside(target.seller_deployment_id);
                return Ok(None);
            }
            Err(EngineError::SourceBusy { .. } | EngineError::NoFreeSource) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        // The operator signed after the network reached `latest`, so no
        // transaction can include this batch's authorization earlier.
        let prepared = prepared.authorized_from(latest);
        let ids: Vec<Uuid> = batch.iter().map(|r| r.id).collect();
        let span = tracing::Span::current();
        span.record("seller_deployment_id", tracing::field::display(target.seller_deployment_id));
        span.record("submission_id", tracing::field::display(prepared.id));
        span.record("charges", ids.len());
        let indexes: Vec<i16> =
            (0..batch.len()).map(|i| i16::try_from(i).unwrap_or(i16::MAX)).collect();
        let mut tx = self.pool.begin().await.map_err(store("begin recurring batch submission"))?;
        match self.engine.record(&mut tx, &prepared).await {
            Ok(_) => {}
            Err(EngineError::SourceBusy { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let linked = sqlx::query!(
            r#"
            UPDATE pay_stellar.recurring_charges r
            SET state = 'submitted', submission_id = $1, batch_index = u.batch_index,
                last_error = NULL
            FROM unnest($2::uuid[], $3::int2[]) AS u(id, batch_index)
            WHERE r.id = u.id AND r.state = 'admitted'
            "#,
            prepared.id,
            &ids,
            &indexes,
        )
        .execute(&mut *tx)
        .await
        .map_err(store("link recurring charges"))?;
        if linked.rows_affected() != u64::try_from(ids.len()).unwrap_or(u64::MAX) {
            return Ok(None);
        }
        tx.commit().await.map_err(store("commit recurring batch submission"))?;
        Ok(Some(prepared.id))
    }
}

/// How a mandate or revocation is decided from the contract's state.
#[derive(Clone, Copy, Debug)]
enum Decide {
    /// Included successfully at this ledger: decided by a read at or after
    /// it.
    Included(i64),
    /// Not included, or of unknown fate: decided once the buyer's
    /// authorization has lapsed, closing it in this state if the contract
    /// shows no effect.
    Lapsing(&'static str),
}
