//! Reconciliation of the treasury's USDC, the contract's totals, the
//! observed event stream and the database.
//!
//! The contract's configuration and totals live in its instance entry, and
//! the treasury's USDC in its trustline entry. Both are read in one
//! `getLedgerEntries` call, which the RPC answers from a single ledger, so
//! the chain side of every comparison describes one ledger `L`. The event
//! sums are taken over events up to and including `L`, once every event of
//! `L` has been read.
//!
//! The database is read in one repeatable-read transaction just after, so it
//! may be a moment ahead of `L`, and a submission may settle in between: two
//! readings can disagree for a moment although nothing is wrong. A
//! discrepancy is therefore recorded only once it has persisted through
//! `confirmations` consecutive checks.
//!
//! What the database must match, from its state machine, with `W` and `R`
//! the buyer and revenue withdrawals observed up to `L` (the gateway keeps
//! no record of withdrawals), and the observed deposits and charges that no
//! database row explains (`D_out`, `C_out`, each also a finding of its own):
//!
//! - a buyer's `available` is its confirmed deposits minus every charge not
//!   refused, so the contract's liabilities lie between
//!   `sum(available) + D_out - C_out - W` and that plus the charges not yet
//!   final (`admitted`, `submitted`, `quarantined`: each may or may not have
//!   debited on-chain) plus the deposits not yet final (`awaiting_signature`,
//!   `signed`, `submitted`: each may already be credited on-chain);
//! - the revenue lies between `sum(charged) + C_out - R` and that plus the
//!   charges not yet final.

use serde_json::json;
use uuid::Uuid;

use fermah_pay_stellar_chain::prepaid::{InstanceState, PrepaidDeployment, instance_state};
use fermah_pay_stellar_chain::stellar_xdr::{LedgerEntryData, TrustLineFlags};
use fermah_pay_stellar_chain::usdc::{asset_contract_id, circle_usdc, trustline_key};
use fermah_pay_stellar_domain::AccountAddress;

use super::{
    ChainReader, Deployment, Finding, FindingKind, Observer, ObserverError, Position, Recorded,
    Severity, insert_finding, log, store, u32_of,
};
use crate::submission::Clock;

/// The chain side of one check, read at one ledger.
struct Reading {
    ledger: u32,
    treasury: AccountAddress,
    /// Balance and flags of the treasury's USDC trustline; `None` when it has
    /// none, which holds nothing and can receive nothing.
    trustline: Option<(i64, u32)>,
    state: InstanceState,
}

/// The database and event-stream side of one check.
struct Books {
    covered: bool,
    observed_from_ledger: i64,
    gaps: i64,
    unrecognized: i64,
    available: i128,
    charged: i128,
    pending_charges: i128,
    pending_deposits: i128,
    deposited_on_chain: i128,
    charged_on_chain: i128,
    withdrawn: i128,
    revenue_withdrawn: i128,
    outside_deposits: i128,
    outside_charges: i128,
}

fn amount(text: &str) -> Result<i128, ObserverError> {
    text.parse().map_err(|_| ObserverError::Corrupt("sum outside i128"))
}

impl<R: ChainReader, K: Clock> Observer<R, K> {
    /// Checks every deployment once; returns the findings recorded.
    pub async fn reconcile(&self) -> Result<Vec<Recorded>, ObserverError> {
        let mut recorded = Vec::new();
        let mut first_error = None;
        for deployment in self.deployments().await? {
            match self.reconcile_one(&deployment).await {
                Ok(mut found) => recorded.append(&mut found),
                Err(error) => {
                    tracing::error!(seller_deployment_id = %deployment.id, error = %error, source = ?std::error::Error::source(&error), "reconciliation failed");
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(recorded), Err)
    }

    async fn reconcile_one(&self, deployment: &Deployment) -> Result<Vec<Recorded>, ObserverError> {
        let reading = self.read_chain(deployment).await?;
        // Events up to the reading's ledger must all be stored before the
        // event sums can describe that ledger.
        self.catch_up(deployment).await?;
        let books = self.books(deployment.id, reading.ledger).await?;
        let mut findings = Vec::new();

        let totals = reading.state.totals;
        let owed = totals.liabilities + totals.revenue;
        let held = i128::from(reading.trustline.map_or(0, |(balance, _)| balance));
        let solvency = json!({
            "ledger": reading.ledger,
            "treasury": reading.treasury.as_str(),
            "treasury_usdc": held.to_string(),
            "trustline": reading.trustline.is_some(),
            "liabilities": totals.liabilities.to_string(),
            "revenue": totals.revenue.to_string(),
            "owed": owed.to_string(),
            "difference": (held - owed).to_string(),
        });
        if self.persisted(deployment.id, FindingKind::TreasuryDeficit, held < owed) {
            findings.push(Finding::new(
                FindingKind::TreasuryDeficit,
                Severity::Critical,
                solvency.clone(),
            ));
        }
        if self.persisted(deployment.id, FindingKind::TreasurySurplus, held > owed) {
            findings.push(Finding::new(FindingKind::TreasurySurplus, Severity::Info, solvency));
        }
        // USDC's issuer can revoke a trustline's authorization. The balance
        // then still covers what is owed, but the asset contract refuses
        // every transfer into or out of the treasury: no deposit or
        // withdrawal can succeed.
        let authorized = reading
            .trustline
            .is_some_and(|(_, flags)| flags & TrustLineFlags::AuthorizedFlag as u32 != 0);
        if self.persisted(deployment.id, FindingKind::TreasuryDeauthorized, !authorized) {
            findings.push(Finding::new(
                FindingKind::TreasuryDeauthorized,
                Severity::Critical,
                json!({
                    "ledger": reading.ledger,
                    "treasury": reading.treasury.as_str(),
                    "trustline": reading.trustline.is_some(),
                    "flags": reading.trustline.map(|(_, flags)| flags),
                }),
            ));
        }

        // Without every event up to the reading's ledger, the sums below
        // would describe another ledger; the streaks are left as they are.
        if books.covered {
            let coverage = json!({
                "observed_from_ledger": books.observed_from_ledger,
                "event_gaps": books.gaps,
                "unrecognized_events": books.unrecognized,
            });
            let expected_liabilities =
                books.deposited_on_chain - books.charged_on_chain - books.withdrawn;
            let expected_revenue = books.charged_on_chain - books.revenue_withdrawn;
            let events_differ =
                (expected_liabilities, expected_revenue) != (totals.liabilities, totals.revenue);
            if self.persisted(deployment.id, FindingKind::EventTotalsMismatch, events_differ) {
                findings.push(Finding::new(
                    FindingKind::EventTotalsMismatch,
                    Severity::Warning,
                    json!({
                        "ledger": reading.ledger,
                        "contract": { "liabilities": totals.liabilities.to_string(),
                                      "revenue": totals.revenue.to_string() },
                        "events": { "liabilities": expected_liabilities.to_string(),
                                    "revenue": expected_revenue.to_string(),
                                    "deposited": books.deposited_on_chain.to_string(),
                                    "charged": books.charged_on_chain.to_string(),
                                    "withdrawn": books.withdrawn.to_string(),
                                    "revenue_withdrawn": books.revenue_withdrawn.to_string() },
                        "coverage": coverage,
                    }),
                ));
            }

            let liabilities_low =
                books.available + books.outside_deposits - books.outside_charges - books.withdrawn;
            let liabilities_high = liabilities_low + books.pending_charges + books.pending_deposits;
            let revenue_low = books.charged + books.outside_charges - books.revenue_withdrawn;
            let revenue_high = revenue_low + books.pending_charges;
            let outside = !(liabilities_low..=liabilities_high).contains(&totals.liabilities)
                || !(revenue_low..=revenue_high).contains(&totals.revenue);
            if self.persisted(deployment.id, FindingKind::LedgerTotalsMismatch, outside) {
                findings.push(Finding::new(
                    FindingKind::LedgerTotalsMismatch,
                    Severity::Warning,
                    json!({
                        "ledger": reading.ledger,
                        "contract": { "liabilities": totals.liabilities.to_string(),
                                      "revenue": totals.revenue.to_string() },
                        "expected": {
                            "liabilities": [liabilities_low.to_string(), liabilities_high.to_string()],
                            "revenue": [revenue_low.to_string(), revenue_high.to_string()],
                        },
                        "database": { "available": books.available.to_string(),
                                      "charged": books.charged.to_string(),
                                      "pending_charges": books.pending_charges.to_string(),
                                      "pending_deposits": books.pending_deposits.to_string() },
                        "events": { "withdrawn": books.withdrawn.to_string(),
                                    "revenue_withdrawn": books.revenue_withdrawn.to_string(),
                                    "outside_deposits": books.outside_deposits.to_string(),
                                    "outside_charges": books.outside_charges.to_string() },
                        "coverage": coverage,
                    }),
                ));
            }
        }

        if findings.is_empty() {
            return Ok(Vec::new());
        }
        let recorded: Vec<Recorded> = findings
            .into_iter()
            .map(|finding| Recorded { deployment: deployment.id, event: None, finding })
            .collect();
        let mut tx = self.pool.begin().await.map_err(store("begin reconciliation"))?;
        for finding in &recorded {
            insert_finding(&mut tx, finding).await?;
        }
        tx.commit().await.map_err(store("commit reconciliation"))?;
        self.mark_recorded(deployment.id, recorded.iter().map(|r| r.finding.kind));
        recorded.iter().for_each(log);
        Ok(recorded)
    }

    /// Reads the instance entry and the treasury's trustline in one call.
    /// The trustline read is the treasury the same read names; after a
    /// rotation the binding's may be stale, so it is read again.
    async fn read_chain(&self, deployment: &Deployment) -> Result<Reading, ObserverError> {
        let usdc = circle_usdc(self.network);
        let probe = PrepaidDeployment {
            contract: deployment.contract,
            usdc: asset_contract_id(&usdc, self.network),
            treasury: deployment.treasury.clone(),
        };
        let mut treasury = deployment.treasury.clone();
        for _ in 0..3 {
            let line = trustline_key(&treasury, &usdc);
            let read = self
                .chain
                .ledger_entries(&[probe.instance_key(), line.clone()])
                .await
                .map_err(ObserverError::Chain)?;
            let state = read
                .entries
                .iter()
                .find(|record| record.key == probe.instance_key())
                .and_then(|record| instance_state(&record.data))
                .ok_or(ObserverError::UnreadableContract)?;
            if state.config.usdc != probe.usdc {
                return Err(ObserverError::UnreadableContract);
            }
            if state.config.treasury != treasury {
                treasury = state.config.treasury.clone();
                continue;
            }
            let trustline =
                read.entries.iter().find(|record| record.key == line).and_then(|record| {
                    match &record.data {
                        LedgerEntryData::Trustline(entry) => Some((entry.balance, entry.flags)),
                        _ => None,
                    }
                });
            return Ok(Reading { ledger: read.latest_ledger, treasury, trustline, state });
        }
        Err(ObserverError::Corrupt("the treasury changed on every read"))
    }

    async fn books(&self, deployment: Uuid, ledger: u32) -> Result<Books, ObserverError> {
        let mut tx = self.pool.begin().await.map_err(store("begin books"))?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(store("isolate books"))?;
        let row = sqlx::query!(
            r#"
            SELECT
                o.start_ledger, o.cursor, o.observed_from_ledger,
                (SELECT count(*) FROM pay_stellar.reconciliation_findings f
                 WHERE f.seller_deployment_id = $1 AND f.kind = 'event_gap') AS "gaps!",
                (SELECT count(*) FROM pay_stellar.chain_events e
                 WHERE e.seller_deployment_id = $1 AND e.ledger <= $2
                   AND e.kind = 'unrecognized') AS "unrecognized!",
                (SELECT COALESCE(sum(available), 0) FROM pay_stellar.buyers
                 WHERE seller_deployment_id = $1)::text AS "available!",
                (SELECT COALESCE(sum(amount), 0) FROM pay_stellar.charges
                 WHERE seller_deployment_id = $1 AND state = 'charged')::text AS "charged!",
                (SELECT COALESCE(sum(amount), 0) FROM pay_stellar.charges
                 WHERE seller_deployment_id = $1
                   AND state IN ('admitted', 'submitted', 'quarantined'))::text
                    AS "pending_charges!",
                (SELECT COALESCE(sum(amount), 0) FROM pay_stellar.deposits
                 WHERE seller_deployment_id = $1
                   AND state IN ('awaiting_signature', 'signed', 'submitted'))::text
                    AS "pending_deposits!",
                (SELECT COALESCE(sum(amount), 0) FROM pay_stellar.chain_events
                 WHERE seller_deployment_id = $1 AND ledger <= $2 AND kind = 'deposit')::text
                    AS "deposited_on_chain!",
                (SELECT COALESCE(sum(amount), 0) FROM pay_stellar.chain_events
                 WHERE seller_deployment_id = $1 AND ledger <= $2 AND kind = 'withdrawal')::text
                    AS "withdrawn!",
                (SELECT COALESCE(sum(amount), 0) FROM pay_stellar.chain_events
                 WHERE seller_deployment_id = $1 AND ledger <= $2
                   AND kind = 'revenue_withdrawal')::text AS "revenue_withdrawn!",
                (SELECT COALESCE(sum(ce.amount), 0)
                 FROM pay_stellar.chain_charge_entries ce
                 JOIN pay_stellar.chain_events e ON e.id = ce.chain_event_id
                 WHERE e.seller_deployment_id = $1 AND e.ledger <= $2
                   AND ce.outcome = 'charged')::text AS "charged_on_chain!",
                (SELECT COALESCE(sum(e.amount), 0) FROM pay_stellar.chain_events e
                 WHERE e.seller_deployment_id = $1 AND e.ledger <= $2 AND e.kind = 'deposit'
                   AND NOT EXISTS (
                       SELECT 1 FROM pay_stellar.deposits d
                       JOIN pay_stellar.buyers b ON b.id = d.buyer_id
                       WHERE d.seller_deployment_id = e.seller_deployment_id
                         AND b.wallet_address = e.owner AND d.deposit_id = e.reference))::text
                    AS "outside_deposits!",
                (SELECT COALESCE(sum(ce.amount), 0)
                 FROM pay_stellar.chain_charge_entries ce
                 JOIN pay_stellar.chain_events e ON e.id = ce.chain_event_id
                 WHERE e.seller_deployment_id = $1 AND e.ledger <= $2
                   AND ce.outcome = 'charged'
                   AND NOT EXISTS (
                       SELECT 1 FROM pay_stellar.charges c
                       JOIN pay_stellar.buyers b ON b.id = c.buyer_id
                       WHERE c.seller_deployment_id = e.seller_deployment_id
                         AND b.wallet_address = ce.owner AND c.charge_id = ce.charge_id))::text
                    AS "outside_charges!"
            FROM pay_stellar.observer_cursors o
            WHERE o.seller_deployment_id = $1
            "#,
            deployment,
            i64::from(ledger),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(store("read books"))?;
        tx.commit().await.map_err(store("end books"))?;
        let position = Position {
            start_ledger: u32_of(row.start_ledger)?,
            cursor: row
                .cursor
                .as_deref()
                .map(|c| {
                    fermah_pay_stellar_chain::rpc::EventCursor::parse(c)
                        .ok_or(ObserverError::Corrupt("stored cursor"))
                })
                .transpose()?,
        };
        Ok(Books {
            covered: position.covers(ledger),
            observed_from_ledger: row.observed_from_ledger,
            gaps: row.gaps,
            unrecognized: row.unrecognized,
            available: amount(&row.available)?,
            charged: amount(&row.charged)?,
            pending_charges: amount(&row.pending_charges)?,
            pending_deposits: amount(&row.pending_deposits)?,
            deposited_on_chain: amount(&row.deposited_on_chain)?,
            charged_on_chain: amount(&row.charged_on_chain)?,
            withdrawn: amount(&row.withdrawn)?,
            revenue_withdrawn: amount(&row.revenue_withdrawn)?,
            outside_deposits: amount(&row.outside_deposits)?,
            outside_charges: amount(&row.outside_charges)?,
        })
    }
}
