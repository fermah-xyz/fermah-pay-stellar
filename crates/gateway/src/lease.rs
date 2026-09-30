//! Leadership among processes sharing one database. Several workers, or
//! several observers, may run with the same configuration; the one holding
//! the lease acts, and the others stand by until it is released or lapses.
//!
//! The lease prevents wasted and conflicting work (two workers building
//! envelopes for the same charges, two observers counting the same
//! discrepancy twice). It is not what keeps balances correct: a process
//! that loses its lease in the middle of a round finishes that round, and
//! the database's own guards (one envelope in flight per source, state
//! changes that happen once) make that harmless.

use std::future::Future;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::watch;
use tokio::time::Instant;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct Lease {
    pool: PgPool,
    name: String,
    holder: Uuid,
    ttl: Duration,
}

impl Lease {
    /// A lease named `name`, held for `ttl` at a time by this process under
    /// a fresh holder identifier.
    #[must_use]
    pub fn new(pool: PgPool, name: String, ttl: Duration) -> Self {
        Self { pool, name, holder: Uuid::now_v7(), ttl }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn holder(&self) -> Uuid {
        self.holder
    }

    /// Takes the lease if it is free or has lapsed, or renews it if this
    /// process holds it, for another `ttl` from the database's clock.
    /// `false` while another process holds it.
    pub async fn try_hold(&self) -> Result<bool, sqlx::Error> {
        let held = sqlx::query_scalar!(
            r#"
            INSERT INTO pay_stellar.leases (name, holder, acquired_at, expires_at)
            VALUES ($1, $2, now(), now() + make_interval(secs => $3))
            ON CONFLICT (name) DO UPDATE
            SET holder = EXCLUDED.holder,
                acquired_at = CASE WHEN leases.holder = EXCLUDED.holder
                                   THEN leases.acquired_at ELSE now() END,
                expires_at = EXCLUDED.expires_at
            WHERE leases.holder = EXCLUDED.holder OR leases.expires_at <= now()
            RETURNING holder
            "#,
            self.name,
            self.holder,
            self.ttl.as_secs_f64(),
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(held.is_some())
    }

    /// Gives the lease up, if this process holds it, so a standby takes
    /// over without waiting for it to lapse.
    pub async fn release(&self) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "DELETE FROM pay_stellar.leases WHERE name = $1 AND holder = $2",
            self.name,
            self.holder,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Runs `work` whenever this process holds `lease`, until `shutdown`
/// resolves. `work` receives a future that resolves when it must stop,
/// because of shutdown or because the lease was lost; it should return soon
/// after, but a round it had started may finish. The lease is renewed every
/// third of its life while `work` runs, and counted lost when a renewal is
/// refused or once a full life has passed since the last renewal the
/// database confirmed, measured from before that renewal was sent: the
/// database's expiry is never earlier. On shutdown the lease is released
/// after `work` returns.
pub async fn lead<F, Fut>(
    lease: &Lease,
    role: &'static str,
    shutdown: impl Future<Output = ()>,
    mut work: F,
) where
    F: FnMut(Stop) -> Fut,
    Fut: Future<Output = ()>,
{
    let every = lease.ttl / 3;
    let held = metrics::gauge!("pay_stellar_lease_held", "role" => role);
    tokio::pin!(shutdown);
    loop {
        held.set(0.0);
        let confirmed = loop {
            let sent = Instant::now();
            match lease.try_hold().await {
                Ok(true) => break sent,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(lease = lease.name(), error = %error, "reading the lease failed");
                }
            }
            tokio::select! {
                () = &mut shutdown => return,
                () = tokio::time::sleep(every) => {}
            }
        };
        held.set(1.0);
        tracing::info!(lease = lease.name(), holder = %lease.holder(), "lease acquired; this process acts");
        let (stop, stopped) = watch::channel(false);
        let run = work(Stop(stopped));
        tokio::pin!(run);
        // Renews alongside `work` rather than between its steps, and ends
        // when the lease is lost: refused, or not confirmed before the
        // deadline, which the database's expiry can only follow.
        let renewals = async {
            let mut confirmed = confirmed;
            loop {
                let deadline = confirmed + lease.ttl;
                tokio::select! {
                    () = tokio::time::sleep_until(deadline) => return,
                    () = tokio::time::sleep(every) => {}
                }
                let sent = Instant::now();
                match tokio::time::timeout_at(deadline, lease.try_hold()).await {
                    Ok(Ok(true)) => confirmed = sent,
                    Ok(Ok(false)) | Err(_) => return,
                    Ok(Err(error)) => {
                        tracing::warn!(lease = lease.name(), error = %error, "renewing the lease failed");
                    }
                }
            }
        };
        tokio::pin!(renewals);
        let mut shutting_down = false;
        let mut lost = false;
        loop {
            tokio::select! {
                () = &mut run => break,
                () = &mut shutdown, if !shutting_down => {
                    shutting_down = true;
                    let _ = stop.send(true);
                }
                () = &mut renewals, if !lost => {
                    lost = true;
                    held.set(0.0);
                    tracing::warn!(lease = lease.name(), "lease lost; stopping until it is free again");
                    let _ = stop.send(true);
                }
            }
        }
        held.set(0.0);
        // Released whether `work` stopped for shutdown or ended on its own,
        // so a standby takes over without waiting for the lease to lapse.
        if !lost && let Err(error) = lease.release().await {
            tracing::warn!(lease = lease.name(), error = %error, "releasing the lease failed; it lapses on its own");
        }
        if shutting_down || !lost {
            return;
        }
    }
}

/// Resolves when the work holding a lease must stop.
pub struct Stop(watch::Receiver<bool>);

impl Stop {
    pub async fn wait(mut self) {
        // A dropped sender also means stop.
        let _ = self.0.wait_for(|stop| *stop).await;
    }
}
