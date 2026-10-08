//! Vault deployments: the contract's limit and exit events applied to the
//! buyers' rows; the buyers' limit changes and exit requests, sent like
//! revocations; and exits sent once they unlock.
//!
//! A buyer's row holds only what the events showed. Admission adds the
//! buyer's requests still open on top (see `ledger::store::reservations`),
//! so a request stays open until the events that carry its effect have been
//! applied: a request is confirmed only once this worker has read the
//! contract's events past the ledger that included it, and closed as failed
//! or expired only once it has read past the ledger its authorization
//! lapsed at, by which any use of it by someone else has been applied too.

use fermah_pay_stellar_chain::prepaid::{
    LedgerEvent, PrepaidDeployment, ledger_event, vault_account,
};
use fermah_pay_stellar_chain::rpc::{EventCursor, EventPage, EventsFrom};
use fermah_pay_stellar_chain::stellar_xdr::{
    HostFunction, Limits, ReadXdr, SorobanAuthorizationEntry,
};
use fermah_pay_stellar_domain::ChainAddress;
use uuid::Uuid;

use super::{Worker, WorkerError, address, deployment, store};
use crate::submission::{Chain, Clock, EngineError, Kind, Resolution, State};

/// Events read per `getEvents` call while catching up.
const PAGE: u32 = 1_000;

impl<C: Chain, K: Clock> Worker<C, K> {
    /// Keeps each vault's buyers current with the contract: reads the
    /// account entry of each buyer not read yet, then applies every new limit
    /// and exit event page by page, each page's effects and the new position
    /// committing together, so no event applies twice or is skipped. A vault
    /// that cannot be read is reported and left behind, which refuses its
    /// charges once it lags too far; the other vaults and the rest of the
    /// round go on.
    pub(super) async fn ingest_vault_events(&self) -> Result<(), WorkerError> {
        let vaults = sqlx::query!(
            r#"
            SELECT l.seller_deployment_id, l.network, l.contract_address, l.usdc_address
            FROM pay_stellar.ledger_contracts l
            WHERE l.network = $1 AND l.operator_address = $2 AND l.custody = 'vault'
            ORDER BY l.seller_deployment_id
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("read vault deployments"))?;
        for vault in vaults {
            let id = vault.seller_deployment_id;
            let label = id.to_string();
            let served = deployment(&vault.contract_address, &vault.usdc_address, None)?;
            let read = async {
                let latest =
                    self.engine.chain().latest_ledger().await.map_err(WorkerError::Chain)?;
                let position = self.vault_position(id, &vault.network, latest).await?;
                self.sync_vault_buyers(id, &served).await?;
                self.apply_vault_events(id, served.contract, position).await?;
                Ok::<_, WorkerError>(latest)
            };
            match read.await {
                Ok(latest) => {
                    let through = sqlx::query_scalar!(
                        "SELECT ledger FROM pay_stellar.vault_event_cursors WHERE seller_deployment_id = $1",
                        id,
                    )
                    .fetch_one(&self.pool)
                    .await
                    .map_err(store("read the vault event position"))?;
                    #[allow(clippy::cast_precision_loss)]
                    metrics::gauge!("pay_stellar_vault_events_behind_ledgers", "deployment" => label)
                        .set((i64::from(latest) - through).max(0) as f64);
                }
                Err(error) => {
                    metrics::counter!("pay_stellar_vault_read_failures_total", "deployment" => label)
                        .increment(1);
                    tracing::error!(seller_deployment_id = %id, error = %error, source = ?std::error::Error::source(&error), "the vault's events were not applied; its charges are refused once the reading falls behind");
                }
            }
        }
        Ok(())
    }

    /// Where the vault's events were applied up to; a vault never read
    /// starts after `latest`, its buyers' earlier state being read from
    /// their account entries instead.
    async fn vault_position(
        &self,
        id: Uuid,
        network: &str,
        latest: u32,
    ) -> Result<EventCursor, WorkerError> {
        let start = EventCursor::end_of_ledger(latest);
        sqlx::query!(
            r#"
            INSERT INTO pay_stellar.vault_event_cursors
                (seller_deployment_id, network, cursor, ledger)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (seller_deployment_id) DO NOTHING
            "#,
            id,
            network,
            start.to_string(),
            i64::from(latest),
        )
        .execute(&self.pool)
        .await
        .map_err(store("start reading vault events"))?;
        let cursor = sqlx::query_scalar!(
            "SELECT cursor FROM pay_stellar.vault_event_cursors WHERE seller_deployment_id = $1",
            id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(store("read the vault event position"))?;
        EventCursor::parse(&cursor)
            .ok_or(WorkerError::Corrupt("vault event cursor outside the CHECK constraint"))
    }

    /// Reads the limit and exit of buyers whose account entries were never
    /// read, as of the ledger the node answers at, which is at or past every
    /// event applied so far. Events up to that ledger are then skipped for
    /// them: the entry already reflects them.
    async fn sync_vault_buyers(
        &self,
        id: Uuid,
        served: &PrepaidDeployment,
    ) -> Result<(), WorkerError> {
        let buyers = sqlx::query!(
            r#"
            SELECT id, wallet_address FROM pay_stellar.buyers
            WHERE seller_deployment_id = $1 AND vault_synced_ledger IS NULL
            ORDER BY id
            LIMIT 64
            "#,
            id,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("find vault buyers to read"))?;
        if buyers.is_empty() {
            return Ok(());
        }
        let owners = buyers
            .iter()
            .map(|buyer| address::<ChainAddress>(&buyer.wallet_address))
            .collect::<Result<Vec<_>, _>>()?;
        let keys: Vec<_> = owners.iter().map(|owner| served.account_key(owner)).collect();
        let read = self.engine.chain().ledger_entries(&keys).await.map_err(WorkerError::Chain)?;
        let mut tx = self.pool.begin().await.map_err(store("begin reading vault buyers"))?;
        // An answer from a node behind the events already applied would
        // undo them; the next round asks again.
        let through = sqlx::query_scalar!(
            "SELECT ledger FROM pay_stellar.vault_event_cursors WHERE seller_deployment_id = $1 FOR SHARE",
            id,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(store("read the vault event position"))?;
        let at = i64::from(read.latest_ledger);
        if at < through {
            return Ok(());
        }
        for (buyer, key) in buyers.iter().zip(&keys) {
            let account = read
                .entries
                .iter()
                .find(|record| record.key == *key)
                .map(|record| {
                    vault_account(&record.data)
                        .ok_or(WorkerError::Corrupt("a vault account entry does not decode"))
                })
                .transpose()?;
            let (cap, pending, exit) = account.map_or((0, None, None), |account| {
                (
                    saturate(account.cap),
                    account.pending_cap.map(|(cap, at)| (saturate(cap), i64::from(at))),
                    account.exit.map(|(amount, _, unlock)| (saturate(amount), i64::from(unlock))),
                )
            });
            sqlx::query!(
                r#"
                UPDATE pay_stellar.buyers
                SET cap = $2, pending_cap = $3, pending_cap_at = $4, exit_amount = $5,
                    exit_unlock_at = $6, vault_synced_ledger = $7
                WHERE id = $1 AND vault_synced_ledger IS NULL
                "#,
                buyer.id,
                cap,
                pending.map(|(cap, _)| cap),
                pending.map(|(_, at)| at),
                exit.map(|(amount, _)| amount),
                exit.map(|(_, unlock)| unlock),
                at,
            )
            .execute(&mut *tx)
            .await
            .map_err(store("store a vault buyer's account"))?;
        }
        tx.commit().await.map_err(store("commit reading vault buyers"))
    }

    async fn apply_vault_events(
        &self,
        id: Uuid,
        contract: [u8; 32],
        mut position: EventCursor,
    ) -> Result<(), WorkerError> {
        loop {
            let page = self
                .engine
                .chain()
                .events(&contract, &EventsFrom::Cursor(position), PAGE)
                .await
                .map_err(WorkerError::Chain)?;
            // Events the node no longer holds cannot be applied: reading on
            // from its oldest ledger would skip them as if read.
            if position.first_unread_ledger() < page.oldest_ledger {
                return Err(WorkerError::VaultEventsLost {
                    from: position.first_unread_ledger(),
                    oldest: page.oldest_ledger,
                });
            }
            validate(contract, position, &page)?;
            if page.cursor <= position {
                return Ok(());
            }
            let mut tx = self.pool.begin().await.map_err(store("begin applying vault events"))?;
            for event in page.events.iter().filter(|event| event.in_successful_contract_call) {
                let Some(decoded) = ledger_event(&event.topics, &event.value) else { continue };
                apply(&mut tx, id, event.ledger, &decoded).await?;
            }
            let read_through = page.cursor.first_unread_ledger().saturating_sub(1);
            let moved = sqlx::query!(
                r#"
                UPDATE pay_stellar.vault_event_cursors
                SET cursor = $3, ledger = $4, updated_at = now()
                WHERE seller_deployment_id = $1 AND cursor = $2
                "#,
                id,
                position.to_string(),
                page.cursor.to_string(),
                i64::from(read_through.max(1)),
            )
            .execute(&mut *tx)
            .await
            .map_err(store("move the vault event cursor"))?;
            // Another worker moved it first: its effects are already in.
            if moved.rows_affected() != 1 {
                return Ok(());
            }
            tx.commit().await.map_err(store("commit applying vault events"))?;
            position = page.cursor;
            if u32::try_from(page.events.len()).unwrap_or(u32::MAX) < PAGE {
                return Ok(());
            }
        }
    }

    /// Sends the oldest signed limit change or exit request of a vault this
    /// worker serves, as signed.
    #[tracing::instrument(skip_all, fields(vault_request_id, submission_id))]
    pub(super) async fn submit_vault_request(&self) -> Result<Option<Uuid>, WorkerError> {
        let Some(row) = sqlx::query!(
            r#"
            SELECT r.id, r.kind, r.cap, r.amount, r.destination,
                   r.signed_authorization_xdr AS "signed_authorization_xdr!",
                   b.wallet_address, l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.vault_requests r
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
        .map_err(store("find signed vault request"))?
        else {
            return Ok(None);
        };
        let deployment =
            deployment(&row.contract_address, &row.usdc_address, row.treasury_address.as_deref())?;
        let owner: ChainAddress = address(&row.wallet_address)?;
        let (kind, call, tree) = match (row.kind.as_str(), row.cap, row.amount, row.destination) {
            ("set_cap", Some(cap), None, None) => (
                Kind::SetCap,
                deployment.set_cap_call(&owner, i128::from(cap)),
                deployment.set_cap_authorization(&owner, i128::from(cap)),
            ),
            ("request_exit", None, Some(amount), Some(destination)) => {
                let destination: ChainAddress = address(&destination)?;
                (
                    Kind::RequestExit,
                    deployment.request_exit_call(&owner, i128::from(amount), &destination),
                    deployment.request_exit_authorization(&owner, i128::from(amount), &destination),
                )
            }
            _ => return Err(WorkerError::Corrupt("vault request outside the CHECK constraint")),
        };
        let entry = SorobanAuthorizationEntry::from_xdr_base64(
            &row.signed_authorization_xdr,
            Limits::none(),
        )
        .map_err(|_| WorkerError::Corrupt("stored signed entry does not decode"))?;
        if entry.root_invocation != tree {
            tracing::error!(vault_request_id = %row.id, "signed entry does not authorize this request");
            self.set_aside(row.id);
            return Ok(None);
        }
        self.send_buyer_intent(
            kind,
            "vault_requests",
            row.id,
            HostFunction::InvokeContract(call),
            entry,
        )
        .await
    }

    /// A request's transaction is final. Only an envelope that expired while
    /// the buyer's signature is still good is acted on here: it is sent
    /// again. Everything else waits for the events (see
    /// [`Self::conclude_vault_requests`]).
    pub(super) async fn settle_vault_request(
        &self,
        submission: Uuid,
        resolution: &Resolution,
    ) -> Result<(), WorkerError> {
        if resolution.state != State::Expired {
            return Ok(());
        }
        let Some(row) = sqlx::query!(
            r#"
            SELECT id, expiration_ledger FROM pay_stellar.vault_requests
            WHERE submission_id = $1 AND state = 'submitted'
            "#,
            submission,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("read submitted vault request"))?
        else {
            return Ok(());
        };
        if self.latest_ledger().await? <= row.expiration_ledger {
            self.resend("vault_requests", row.id).await?;
        }
        Ok(())
    }

    /// Resolves the requests whose effect, or the lack of one, the applied
    /// events now show.
    pub(super) async fn conclude_vault_requests(&self) -> Result<(), WorkerError> {
        let rows = sqlx::query!(
            r#"
            SELECT r.id, r.state, r.expiration_ledger, s.state AS "submission_state?",
                   s.ledger AS "included?", c.ledger AS "read_through?"
            FROM pay_stellar.vault_requests r
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = r.seller_deployment_id AND l.network = r.network
            LEFT JOIN pay_stellar.submissions s ON s.id = r.submission_id
            LEFT JOIN pay_stellar.vault_event_cursors c
              ON c.seller_deployment_id = r.seller_deployment_id
            WHERE r.network = $1 AND l.operator_address = $2
              AND r.state IN ('awaiting_signature', 'signed', 'submitted')
            ORDER BY r.created_at
            LIMIT 200
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("find open vault requests"))?;
        for row in rows {
            let Some(read_through) = row.read_through else { continue };
            let lapsed = read_through > row.expiration_ledger;
            let closing = match (row.state.as_str(), row.submission_state.as_deref(), row.included)
            {
                ("submitted", Some("succeeded"), Some(included))
                    if read_through >= i64::from(included) =>
                {
                    Some(("confirmed", None))
                }
                ("submitted", Some("failed"), _) if lapsed => {
                    Some(("failed", Some("included as failed")))
                }
                ("submitted", Some("expired" | "quarantined"), _) if lapsed => {
                    Some(("expired", Some("authorization lapsed")))
                }
                ("awaiting_signature" | "signed", _, _) if lapsed => {
                    Some(("expired", Some("authorization lapsed")))
                }
                _ => None,
            };
            let Some((state, error)) = closing else { continue };
            sqlx::query!(
                r#"
                UPDATE pay_stellar.vault_requests
                SET state = $2, resolved_at = now(), last_error = COALESCE($3, last_error)
                WHERE id = $1 AND state IN ('awaiting_signature', 'signed', 'submitted')
                "#,
                row.id,
                state,
                error,
            )
            .execute(&self.pool)
            .await
            .map_err(store("close vault request"))?;
            metrics::counter!("pay_stellar_vault_requests_closed_total", "state" => state)
                .increment(1);
        }
        Ok(())
    }

    /// Sends the exit of a buyer whose request has unlocked, which pays the
    /// destination the buyer signed. Nobody needs to authorize it: the
    /// operator sends it so the buyer does not have to.
    #[tracing::instrument(skip_all, fields(buyer_id, submission_id))]
    pub(super) async fn submit_exit(&self) -> Result<Option<Uuid>, WorkerError> {
        let latest = self.latest_ledger().await?;
        let Some(row) = sqlx::query!(
            r#"
            SELECT b.id, b.wallet_address, l.contract_address, l.usdc_address
            FROM pay_stellar.buyers b
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = b.seller_deployment_id AND l.network = b.network
            WHERE b.network = $1 AND l.operator_address = $2 AND l.custody = 'vault'
              AND b.exit_unlock_at <= $3 AND b.id <> ALL($4)
            ORDER BY b.exit_unlock_at
            LIMIT 1
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            latest,
            &self.set_aside_ids(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("find unlocked exit"))?
        else {
            return Ok(None);
        };
        // Not sent again before its event has been applied.
        self.set_aside(row.id);
        let deployment = deployment(&row.contract_address, &row.usdc_address, None)?;
        let owner: ChainAddress = address(&row.wallet_address)?;
        let function = HostFunction::InvokeContract(deployment.exit_call(&owner));
        let prepared = match self.engine.prepare(Kind::Exit, function, Vec::new()).await {
            Ok(prepared) => prepared,
            Err(
                error
                @ (EngineError::SimulationFailed(_) | EngineError::ResourceFeeAboveCap { .. }),
            ) => {
                tracing::info!(buyer_id = %row.id, error = %error, "the exit is not payable now");
                return Ok(None);
            }
            Err(EngineError::SourceBusy { .. } | EngineError::NoFreeSource) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut tx = self.pool.begin().await.map_err(store("begin exit submission"))?;
        match self.engine.record(&mut tx, &prepared).await {
            Ok(_) => {}
            Err(EngineError::SourceBusy { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        tx.commit().await.map_err(store("commit exit submission"))?;
        tracing::Span::current().record("submission_id", tracing::field::display(prepared.id));
        Ok(Some(prepared.id))
    }
}

/// Refuses a page that does not continue from `position`, in order, with
/// events of `contract` only.
fn validate(
    contract: [u8; 32],
    position: EventCursor,
    page: &EventPage,
) -> Result<(), WorkerError> {
    let mut last = position;
    for event in &page.events {
        if event.contract != contract {
            return Err(WorkerError::Corrupt("the node returned another contract's event"));
        }
        if event.id <= last {
            return Err(WorkerError::Corrupt("the node returned events out of order"));
        }
        last = event.id;
    }
    if page.cursor < last && page.cursor > position {
        return Err(WorkerError::Corrupt("the node's cursor is behind its events"));
    }
    Ok(())
}

/// Applies one event of a vault to its buyer's row; events of other kinds,
/// and owners that are no buyer of this deployment, change nothing. A
/// buyer's limit and exit change only for events after the ledger its
/// account entry was read at, which already reflects the earlier ones.
async fn apply(
    tx: &mut sqlx::PgConnection,
    deployment: Uuid,
    ledger: u32,
    event: &LedgerEvent,
) -> Result<(), WorkerError> {
    let ledger = i64::from(ledger);
    match event {
        LedgerEvent::CapRaised { owner, cap } => {
            sqlx::query!(
                r#"
                UPDATE pay_stellar.buyers
                SET cap = $3, pending_cap = NULL, pending_cap_at = NULL
                WHERE seller_deployment_id = $1 AND wallet_address = $2
                  AND (vault_synced_ledger IS NULL OR vault_synced_ledger < $4)
                "#,
                deployment,
                owner.to_string(),
                saturate(*cap),
                ledger,
            )
            .execute(&mut *tx)
            .await
            .map_err(store("apply a raised limit"))?;
        }
        LedgerEvent::CapLowered { owner, cap, effective_at } => {
            // A pending lower limit that had taken effect by this event is
            // the one in force the new one is lower than.
            sqlx::query!(
                r#"
                UPDATE pay_stellar.buyers
                SET cap = CASE WHEN pending_cap_at <= $5 THEN pending_cap ELSE cap END,
                    pending_cap = $3, pending_cap_at = $4
                WHERE seller_deployment_id = $1 AND wallet_address = $2
                  AND (vault_synced_ledger IS NULL OR vault_synced_ledger < $5)
                "#,
                deployment,
                owner.to_string(),
                saturate(*cap),
                i64::from(*effective_at),
                ledger,
            )
            .execute(&mut *tx)
            .await
            .map_err(store("apply a lowered limit"))?;
        }
        LedgerEvent::ExitRequested { owner, amount, unlock_at, .. } => {
            sqlx::query!(
                r#"
                UPDATE pay_stellar.buyers SET exit_amount = $3, exit_unlock_at = $4
                WHERE seller_deployment_id = $1 AND wallet_address = $2
                  AND (vault_synced_ledger IS NULL OR vault_synced_ledger < $5)
                "#,
                deployment,
                owner.to_string(),
                saturate(*amount),
                i64::from(*unlock_at),
                ledger,
            )
            .execute(&mut *tx)
            .await
            .map_err(store("apply an exit request"))?;
        }
        LedgerEvent::Exited { owner, amount, .. } => {
            // What the exit paid left the vault whenever this event is read,
            // so it always comes off `available`. Beyond `available`, it took
            // what the buyer's open charges and held withdrawals were held
            // from: those can no longer settle, and their refunds bring the
            // balance back to what the contract holds for the buyer.
            let paid = saturate(*amount);
            let short = sqlx::query_scalar!(
                r#"
                UPDATE pay_stellar.buyers b
                SET available = GREATEST(b.available - $3, CASE
                        WHEN b.vault_synced_ledger IS NULL THEN 0
                        ELSE -((SELECT COALESCE(SUM(c.amount), 0) FROM pay_stellar.charges c
                                WHERE c.buyer_id = b.id
                                  AND c.state IN ('admitted', 'submitted', 'quarantined'))
                             + (SELECT COALESCE(SUM(w.amount), 0) FROM pay_stellar.withdrawals w
                                WHERE w.buyer_id = b.id AND w.state IN ('signed', 'submitted')))
                    END),
                    exit_amount = CASE
                        WHEN b.vault_synced_ledger IS NULL OR b.vault_synced_ledger < $4 THEN NULL
                        ELSE b.exit_amount END,
                    exit_unlock_at = CASE
                        WHEN b.vault_synced_ledger IS NULL OR b.vault_synced_ledger < $4 THEN NULL
                        ELSE b.exit_unlock_at END
                WHERE b.seller_deployment_id = $1 AND b.wallet_address = $2
                RETURNING b.available < 0 AS "short!"
                "#,
                deployment,
                owner.to_string(),
                paid,
                ledger,
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(store("apply an exit"))?;
            if short == Some(true) {
                tracing::warn!(seller_deployment_id = %deployment, owner = %owner, paid, "an exit took balance held for open charges or withdrawals");
            }
        }
        _ => {}
    }
    Ok(())
}

/// A vault amount as stored. The contract bounds transfers to `i64`, but not
/// a limit: one beyond it limits nothing an `i64` balance can reach.
fn saturate(amount: i128) -> i64 {
    i64::try_from(amount).unwrap_or(if amount < 0 { 0 } else { i64::MAX })
}
