//! PostgreSQL access for deposits, charges, withdrawals and balances, under
//! the API role.
//! Every query binds the caller's deployment and network, so a row of another
//! deployment is indistinguishable from a missing one.

use fermah_pay_stellar_chain::prepaid::PrepaidDeployment;
use fermah_pay_stellar_domain::{AccountAddress, IdempotencyKey};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::scope::Scope;
use crate::store::{Store, StoreError};

const DEPOSIT_KEY: &str = "deposits_idempotency_key";
const CHARGE_KEY: &str = "charges_idempotency_key";
const WITHDRAWAL_KEY: &str = "withdrawals_idempotency_key";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepositState {
    AwaitingSignature,
    Signed,
    Submitted,
    Confirmed,
    Failed,
    Expired,
}

impl DepositState {
    fn parse(raw: &str) -> Result<Self, StoreError> {
        Ok(match raw {
            "awaiting_signature" => Self::AwaitingSignature,
            "signed" => Self::Signed,
            "submitted" => Self::Submitted,
            "confirmed" => Self::Confirmed,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            _ => return Err(StoreError::Corrupt("deposit state outside the CHECK constraint")),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WithdrawalState {
    AwaitingSignature,
    Signed,
    Submitted,
    Confirmed,
    Failed,
    Expired,
}

impl WithdrawalState {
    fn parse(raw: &str) -> Result<Self, StoreError> {
        Ok(match raw {
            "awaiting_signature" => Self::AwaitingSignature,
            "signed" => Self::Signed,
            "submitted" => Self::Submitted,
            "confirmed" => Self::Confirmed,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            _ => return Err(StoreError::Corrupt("withdrawal state outside the CHECK constraint")),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChargeState {
    Admitted,
    Submitted,
    Charged,
    Refused,
    Quarantined,
}

impl ChargeState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Submitted => "submitted",
            Self::Charged => "charged",
            Self::Refused => "refused",
            Self::Quarantined => "quarantined",
        }
    }

    fn parse(raw: &str) -> Result<Self, StoreError> {
        Ok(match raw {
            "admitted" => Self::Admitted,
            "submitted" => Self::Submitted,
            "charged" => Self::Charged,
            "refused" => Self::Refused,
            "quarantined" => Self::Quarantined,
            _ => return Err(StoreError::Corrupt("charge state outside the CHECK constraint")),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositRecord {
    pub id: Uuid,
    pub buyer_id: Uuid,
    pub wallet: AccountAddress,
    pub amount: i64,
    pub state: DepositState,
    pub authorization_xdr: String,
    pub signed_authorization_xdr: Option<String>,
    pub expiration_ledger: i64,
    pub transaction_hash: Option<Vec<u8>>,
    pub ledger: Option<i32>,
    pub created_at: OffsetDateTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChargeRecord {
    pub id: Uuid,
    pub buyer_id: Uuid,
    pub amount: i64,
    pub charge_id: Vec<u8>,
    pub last_ledger: i64,
    pub state: ChargeState,
    pub outcome: Option<String>,
    pub transaction_hash: Option<Vec<u8>>,
    pub ledger: Option<i32>,
    pub created_at: OffsetDateTime,
}

pub struct NewDeposit<'a> {
    pub buyer_id: Uuid,
    pub key: &'a IdempotencyKey,
    pub amount: i64,
    pub deposit_id: [u8; 32],
    pub authorization_xdr: &'a str,
    pub expiration_ledger: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WithdrawalRecord {
    pub id: Uuid,
    pub buyer_id: Uuid,
    pub wallet: AccountAddress,
    pub amount: i64,
    pub destination: AccountAddress,
    pub state: WithdrawalState,
    pub authorization_xdr: String,
    pub signed_authorization_xdr: Option<String>,
    pub expiration_ledger: i64,
    pub transaction_hash: Option<Vec<u8>>,
    pub ledger: Option<i32>,
    pub created_at: OffsetDateTime,
}

pub struct NewWithdrawal<'a> {
    pub buyer_id: Uuid,
    pub key: &'a IdempotencyKey,
    pub amount: i64,
    pub destination: &'a AccountAddress,
    pub withdrawal_id: [u8; 32],
    pub authorization_xdr: &'a str,
    pub expiration_ledger: u32,
}

/// What inserting a deposit or withdrawal did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insertion {
    Created(Uuid),
    /// Another row holds the idempotency key.
    KeyTaken,
    /// The buyer reached its quota for the day; nothing was inserted.
    QuotaExceeded,
}

/// What storing a withdrawal's signed entry did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WithdrawalSigning {
    /// Stored, and the amount held from the available balance.
    Held,
    /// The withdrawal no longer awaits a signature; a concurrent request
    /// may have stored one.
    NotOpen,
    /// The available balance does not cover the amount; nothing changed.
    InsufficientBalance,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    Admitted(ChargeRecord),
    /// The key was used before for this buyer and amount; nothing was debited.
    Replayed(ChargeRecord),
    /// The key was used before for another buyer or amount.
    Conflict,
    BuyerNotFound,
    InsufficientBalance,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Balance {
    pub available: i64,
    pub pending_charges: i64,
    pub pending_withdrawals: i64,
}

/// The on-chain identifier of the charge a seller names with `key`: the
/// same key always names the same charge, and keys are unique within a
/// deployment, whose contract scopes identifiers by buyer.
#[must_use]
pub fn charge_id(key: &IdempotencyKey) -> [u8; 32] {
    Sha256::digest(key.as_str().as_bytes()).into()
}

fn query(operation: &'static str) -> impl FnOnce(sqlx::Error) -> StoreError {
    move |source| StoreError::Query { operation, source }
}

fn is_violation_of(error: &sqlx::Error, constraint: &str) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.constraint() == Some(constraint))
}

fn address(raw: &str) -> Result<AccountAddress, StoreError> {
    raw.parse().map_err(|_| StoreError::Corrupt("address outside the CHECK constraint"))
}

fn contract_id(raw: &str) -> Result<[u8; 32], StoreError> {
    stellar_strkey::Contract::from_string(raw)
        .map(|contract| contract.0)
        .map_err(|_| StoreError::Corrupt("contract address outside the CHECK constraint"))
}

impl Store {
    /// The ledger contract the caller's deployment settles against.
    pub async fn ledger_binding(
        &self,
        scope: &Scope,
    ) -> Result<Option<PrepaidDeployment>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT contract_address, usdc_address, treasury_address
            FROM pay_stellar.ledger_contracts
            WHERE seller_deployment_id = $1 AND network = $2
            "#,
            scope.seller_deployment_id(),
            scope.network().caip2(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(query("read ledger binding"))?;
        row.map(|row| {
            Ok(PrepaidDeployment {
                contract: contract_id(&row.contract_address)?,
                usdc: contract_id(&row.usdc_address)?,
                treasury: address(&row.treasury_address)?,
            })
        })
        .transpose()
    }

    /// The caller's buyer holding `wallet`; a wallet belongs to at most one
    /// buyer of a deployment.
    pub async fn buyer_by_wallet(
        &self,
        scope: &Scope,
        wallet: &AccountAddress,
    ) -> Result<Option<Uuid>, StoreError> {
        sqlx::query_scalar!(
            r#"
            SELECT id FROM pay_stellar.buyers
            WHERE wallet_address = $1 AND product_id = $2 AND seller_deployment_id = $3
              AND network = $4
            "#,
            wallet.as_str(),
            scope.product_id(),
            scope.seller_deployment_id(),
            scope.network().caip2(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(query("find buyer by wallet"))
    }

    pub async fn buyer_wallet(
        &self,
        scope: &Scope,
        buyer_id: Uuid,
    ) -> Result<Option<AccountAddress>, StoreError> {
        let wallet = sqlx::query_scalar!(
            r#"
            SELECT wallet_address FROM pay_stellar.buyers
            WHERE id = $1 AND product_id = $2 AND seller_deployment_id = $3 AND network = $4
            "#,
            buyer_id,
            scope.product_id(),
            scope.seller_deployment_id(),
            scope.network().caip2(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(query("read buyer wallet"))?;
        wallet.as_deref().map(address).transpose()
    }

    /// Inserts a prepared deposit, unless the buyer prepared its quota of
    /// deposits in the last day. `KeyTaken` means the idempotency key is
    /// taken, possibly by a concurrent request; the caller reads that deposit.
    pub async fn insert_deposit(
        &self,
        scope: &Scope,
        deposit: &NewDeposit<'_>,
    ) -> Result<Insertion, StoreError> {
        let id = Uuid::now_v7();
        let mut tx = self.pool.begin().await.map_err(query("begin deposit"))?;
        if !self
            .within_quota(
                &mut tx,
                scope,
                deposit.buyer_id,
                "deposits",
                self.quotas.deposits_per_buyer,
            )
            .await?
        {
            return Ok(Insertion::QuotaExceeded);
        }
        let inserted = sqlx::query!(
            r#"
            INSERT INTO pay_stellar.deposits
                (id, buyer_id, seller_deployment_id, network, idempotency_key, amount, deposit_id,
                 authorization_xdr, expiration_ledger)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
            id,
            deposit.buyer_id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            deposit.key.as_str(),
            deposit.amount,
            deposit.deposit_id.as_slice(),
            deposit.authorization_xdr,
            i64::from(deposit.expiration_ledger),
        )
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(error) if is_violation_of(&error, DEPOSIT_KEY) => return Ok(Insertion::KeyTaken),
            Err(source) => return Err(StoreError::Query { operation: "insert deposit", source }),
        }
        tx.commit().await.map_err(query("commit deposit"))?;
        Ok(Insertion::Created(id))
    }

    /// Locks the buyer row, as charge admission does, and tells whether the
    /// buyer created fewer than `quota` rows of `table` in the last day. The
    /// lock makes the count and the insert that follows one step for the
    /// buyer.
    async fn within_quota(
        &self,
        tx: &mut sqlx::PgConnection,
        scope: &Scope,
        buyer_id: Uuid,
        table: &'static str,
        quota: u32,
    ) -> Result<bool, StoreError> {
        sqlx::query!(
            r#"
            SELECT id FROM pay_stellar.buyers
            WHERE id = $1 AND seller_deployment_id = $2 AND network = $3
            FOR UPDATE
            "#,
            buyer_id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(query("lock buyer"))?;
        let recent = match table {
            "deposits" => {
                sqlx::query_scalar!(
                    r#"
                SELECT count(*) AS "recent!" FROM pay_stellar.deposits
                WHERE buyer_id = $1 AND created_at > now() - interval '1 day'
                "#,
                    buyer_id,
                )
                .fetch_one(&mut *tx)
                .await
            }
            "withdrawals" => {
                sqlx::query_scalar!(
                    r#"
                SELECT count(*) AS "recent!" FROM pay_stellar.withdrawals
                WHERE buyer_id = $1 AND created_at > now() - interval '1 day'
                "#,
                    buyer_id,
                )
                .fetch_one(&mut *tx)
                .await
            }
            _ => return Err(StoreError::Corrupt("quota for an unknown table")),
        }
        .map_err(query("count recent requests"))?;
        Ok(recent < i64::from(quota))
    }

    pub async fn deposit(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<DepositRecord>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT d.id, d.buyer_id, b.wallet_address, d.amount, d.state, d.authorization_xdr,
                   d.signed_authorization_xdr, d.expiration_ledger, d.created_at,
                   s.outer_hash AS "outer_hash?", s.ledger AS "ledger?"
            FROM pay_stellar.deposits d
            JOIN pay_stellar.buyers b
              ON b.id = d.buyer_id AND b.seller_deployment_id = d.seller_deployment_id
             AND b.network = d.network
            LEFT JOIN pay_stellar.submissions s ON s.id = d.submission_id
            WHERE d.seller_deployment_id = $1 AND d.network = $2
              AND (d.id = $3 OR d.idempotency_key = $4)
            "#,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            id,
            key.map(IdempotencyKey::as_str),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(query("read deposit"))?;
        row.map(|row| {
            Ok(DepositRecord {
                id: row.id,
                buyer_id: row.buyer_id,
                wallet: address(&row.wallet_address)?,
                amount: row.amount,
                state: DepositState::parse(&row.state)?,
                authorization_xdr: row.authorization_xdr,
                signed_authorization_xdr: row.signed_authorization_xdr,
                expiration_ledger: row.expiration_ledger,
                transaction_hash: row.outer_hash,
                ledger: row.ledger,
                created_at: row.created_at,
            })
        })
        .transpose()
    }

    /// Stores the verified signed entry if the deposit still awaits one.
    /// Returns whether it did; a concurrent submission may have won.
    pub async fn sign_deposit(
        &self,
        scope: &Scope,
        id: Uuid,
        signed_authorization_xdr: &str,
    ) -> Result<bool, StoreError> {
        let updated = sqlx::query!(
            r#"
            UPDATE pay_stellar.deposits
            SET state = 'signed', signed_authorization_xdr = $4, signed_at = now()
            WHERE id = $1 AND seller_deployment_id = $2 AND network = $3
              AND state = 'awaiting_signature'
            "#,
            id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            signed_authorization_xdr,
        )
        .execute(&self.pool)
        .await
        .map_err(query("sign deposit"))?;
        Ok(updated.rows_affected() == 1)
    }

    /// Admits a charge in one database transaction: the buyer row is locked,
    /// so the balance check and the debit cannot interleave with another
    /// charge for the same buyer, and the idempotency key is checked after
    /// the lock, so a concurrent retry of the same request sees the first
    /// one's row instead of debiting twice. The charge's on-chain identifier
    /// is derived from the key, so every retry names the same charge.
    pub async fn admit_charge(
        &self,
        scope: &Scope,
        buyer_id: Uuid,
        amount: i64,
        key: &IdempotencyKey,
        last_ledger: u32,
    ) -> Result<Admission, StoreError> {
        match self.try_admit_charge(scope, buyer_id, amount, key, last_ledger).await {
            // Same key, another buyer: the two requests locked different rows
            // and raced to the key. Reading after the winner committed decides.
            Err(StoreError::Query { source, .. }) if is_violation_of(&source, CHARGE_KEY) => {
                Ok(self
                    .replay_charge(scope, buyer_id, amount, key)
                    .await?
                    .unwrap_or(Admission::Conflict))
            }
            other => other,
        }
    }

    async fn try_admit_charge(
        &self,
        scope: &Scope,
        buyer_id: Uuid,
        amount: i64,
        key: &IdempotencyKey,
        last_ledger: u32,
    ) -> Result<Admission, StoreError> {
        let mut tx = self.pool.begin().await.map_err(query("begin charge admission"))?;
        let buyer = sqlx::query!(
            r#"
            SELECT available FROM pay_stellar.buyers
            WHERE id = $1 AND product_id = $2 AND seller_deployment_id = $3 AND network = $4
            FOR UPDATE
            "#,
            buyer_id,
            scope.product_id(),
            scope.seller_deployment_id(),
            scope.network().caip2(),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(query("lock buyer"))?;
        if let Some(replayed) = self.replay_charge_on(&mut tx, scope, buyer_id, amount, key).await?
        {
            return Ok(replayed);
        }
        let Some(buyer) = buyer else { return Ok(Admission::BuyerNotFound) };
        if buyer.available < amount {
            return Ok(Admission::InsufficientBalance);
        }
        sqlx::query!(
            "UPDATE pay_stellar.buyers SET available = available - $2 WHERE id = $1",
            buyer_id,
            amount,
        )
        .execute(&mut *tx)
        .await
        .map_err(query("debit buyer"))?;
        let charge_id = charge_id(key);
        let row = sqlx::query!(
            r#"
            INSERT INTO pay_stellar.charges
                (id, buyer_id, seller_deployment_id, network, idempotency_key, amount, charge_id,
                 last_ledger)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            RETURNING id, created_at
            "#,
            Uuid::now_v7(),
            buyer_id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            key.as_str(),
            amount,
            charge_id.as_slice(),
            i64::from(last_ledger),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(query("insert charge"))?;
        tx.commit().await.map_err(query("commit charge admission"))?;
        Ok(Admission::Admitted(ChargeRecord {
            id: row.id,
            buyer_id,
            amount,
            charge_id: charge_id.to_vec(),
            last_ledger: i64::from(last_ledger),
            state: ChargeState::Admitted,
            outcome: None,
            transaction_hash: None,
            ledger: None,
            created_at: row.created_at,
        }))
    }

    /// The earlier charge under `key`, if any, judged against this request:
    /// answered from the stored row alone, so a retry needs no network.
    pub async fn replayed_charge(
        &self,
        scope: &Scope,
        buyer_id: Uuid,
        amount: i64,
        key: &IdempotencyKey,
    ) -> Result<Option<Admission>, StoreError> {
        self.replay_charge(scope, buyer_id, amount, key).await
    }

    async fn replay_charge(
        &self,
        scope: &Scope,
        buyer_id: Uuid,
        amount: i64,
        key: &IdempotencyKey,
    ) -> Result<Option<Admission>, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(query("acquire connection"))?;
        self.replay_charge_on(&mut conn, scope, buyer_id, amount, key).await
    }

    async fn replay_charge_on(
        &self,
        conn: &mut sqlx::PgConnection,
        scope: &Scope,
        buyer_id: Uuid,
        amount: i64,
        key: &IdempotencyKey,
    ) -> Result<Option<Admission>, StoreError> {
        let existing = self.charge_on(conn, scope, None, Some(key)).await?;
        Ok(existing.map(|charge| {
            if charge.buyer_id == buyer_id && charge.amount == amount {
                Admission::Replayed(charge)
            } else {
                Admission::Conflict
            }
        }))
    }

    pub async fn charge(
        &self,
        scope: &Scope,
        id: Uuid,
    ) -> Result<Option<ChargeRecord>, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(query("acquire connection"))?;
        self.charge_on(&mut conn, scope, Some(id), None).await
    }

    /// The caller's charge admitted under `key`.
    pub async fn charge_with_key(
        &self,
        scope: &Scope,
        key: &IdempotencyKey,
    ) -> Result<Option<ChargeRecord>, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(query("acquire connection"))?;
        self.charge_on(&mut conn, scope, None, Some(key)).await
    }

    async fn charge_on(
        &self,
        conn: &mut sqlx::PgConnection,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<ChargeRecord>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT c.id, c.buyer_id, c.amount, c.charge_id, c.last_ledger, c.state, c.outcome,
                   c.created_at,
                   s.outer_hash AS "outer_hash?", s.ledger AS "ledger?"
            FROM pay_stellar.charges c
            LEFT JOIN pay_stellar.submissions s ON s.id = c.submission_id
            WHERE c.seller_deployment_id = $1 AND c.network = $2
              AND (c.id = $3 OR c.idempotency_key = $4)
            "#,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            id,
            key.map(IdempotencyKey::as_str),
        )
        .fetch_optional(conn)
        .await
        .map_err(query("read charge"))?;
        row.map(|row| {
            Ok(ChargeRecord {
                id: row.id,
                buyer_id: row.buyer_id,
                amount: row.amount,
                charge_id: row.charge_id,
                last_ledger: row.last_ledger,
                state: ChargeState::parse(&row.state)?,
                outcome: row.outcome,
                transaction_hash: row.outer_hash,
                ledger: row.ledger,
                created_at: row.created_at,
            })
        })
        .transpose()
    }

    pub async fn balance(
        &self,
        scope: &Scope,
        buyer_id: Uuid,
    ) -> Result<Option<Balance>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT b.available,
                   COALESCE((SELECT SUM(c.amount) FROM pay_stellar.charges c
                             WHERE c.buyer_id = b.id AND c.state IN ('admitted', 'submitted')),
                            0)::BIGINT AS "pending!",
                   COALESCE((SELECT SUM(w.amount) FROM pay_stellar.withdrawals w
                             WHERE w.buyer_id = b.id AND w.state IN ('signed', 'submitted')),
                            0)::BIGINT AS "withdrawing!"
            FROM pay_stellar.buyers b
            WHERE b.id = $1 AND b.product_id = $2 AND b.seller_deployment_id = $3
              AND b.network = $4
            "#,
            buyer_id,
            scope.product_id(),
            scope.seller_deployment_id(),
            scope.network().caip2(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(query("read balance"))?;
        Ok(row.map(|row| Balance {
            available: row.available,
            pending_charges: row.pending,
            pending_withdrawals: row.withdrawing,
        }))
    }

    /// Inserts a prepared withdrawal, unless the buyer prepared its quota
    /// of withdrawals in the last day. `KeyTaken` means the idempotency key
    /// is taken, possibly by a concurrent request; the caller reads that one.
    pub async fn insert_withdrawal(
        &self,
        scope: &Scope,
        withdrawal: &NewWithdrawal<'_>,
    ) -> Result<Insertion, StoreError> {
        let id = Uuid::now_v7();
        let mut tx = self.pool.begin().await.map_err(query("begin withdrawal"))?;
        let quota = self.quotas.withdrawals_per_buyer;
        if !self.within_quota(&mut tx, scope, withdrawal.buyer_id, "withdrawals", quota).await? {
            return Ok(Insertion::QuotaExceeded);
        }
        let inserted = sqlx::query!(
            r#"
            INSERT INTO pay_stellar.withdrawals
                (id, buyer_id, seller_deployment_id, network, idempotency_key, amount,
                 destination_address, withdrawal_id, authorization_xdr, expiration_ledger)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
            id,
            withdrawal.buyer_id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            withdrawal.key.as_str(),
            withdrawal.amount,
            withdrawal.destination.as_str(),
            withdrawal.withdrawal_id.as_slice(),
            withdrawal.authorization_xdr,
            i64::from(withdrawal.expiration_ledger),
        )
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(error) if is_violation_of(&error, WITHDRAWAL_KEY) => {
                return Ok(Insertion::KeyTaken);
            }
            Err(source) => {
                return Err(StoreError::Query { operation: "insert withdrawal", source });
            }
        }
        tx.commit().await.map_err(query("commit withdrawal"))?;
        Ok(Insertion::Created(id))
    }

    pub async fn withdrawal(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<WithdrawalRecord>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT w.id, w.buyer_id, b.wallet_address, w.amount, w.destination_address, w.state,
                   w.authorization_xdr, w.signed_authorization_xdr, w.expiration_ledger,
                   w.created_at, s.outer_hash AS "outer_hash?", s.ledger AS "ledger?"
            FROM pay_stellar.withdrawals w
            JOIN pay_stellar.buyers b
              ON b.id = w.buyer_id AND b.seller_deployment_id = w.seller_deployment_id
             AND b.network = w.network
            LEFT JOIN pay_stellar.submissions s ON s.id = w.submission_id
            WHERE w.seller_deployment_id = $1 AND w.network = $2
              AND (w.id = $3 OR w.idempotency_key = $4)
            "#,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            id,
            key.map(IdempotencyKey::as_str),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(query("read withdrawal"))?;
        row.map(|row| {
            Ok(WithdrawalRecord {
                id: row.id,
                buyer_id: row.buyer_id,
                wallet: address(&row.wallet_address)?,
                amount: row.amount,
                destination: address(&row.destination_address)?,
                state: WithdrawalState::parse(&row.state)?,
                authorization_xdr: row.authorization_xdr,
                signed_authorization_xdr: row.signed_authorization_xdr,
                expiration_ledger: row.expiration_ledger,
                transaction_hash: row.outer_hash,
                ledger: row.ledger,
                created_at: row.created_at,
            })
        })
        .transpose()
    }

    /// Stores the verified signed entry and holds the amount from the
    /// buyer's available balance, in one database transaction with the
    /// buyer row locked first, as charge admission locks it: a charge
    /// admitted concurrently sees either the balance before the hold or
    /// after it, never both spending the same units.
    pub async fn sign_withdrawal(
        &self,
        scope: &Scope,
        id: Uuid,
        signed_authorization_xdr: &str,
    ) -> Result<WithdrawalSigning, StoreError> {
        let mut tx = self.pool.begin().await.map_err(query("begin withdrawal signing"))?;
        let buyer = sqlx::query!(
            r#"
            SELECT b.available, w.amount FROM pay_stellar.withdrawals w
            JOIN pay_stellar.buyers b ON b.id = w.buyer_id
            WHERE w.id = $1 AND w.seller_deployment_id = $2 AND w.network = $3
            FOR UPDATE OF b
            "#,
            id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(query("lock withdrawing buyer"))?;
        let Some(buyer) = buyer else { return Ok(WithdrawalSigning::NotOpen) };
        let signed = sqlx::query!(
            r#"
            UPDATE pay_stellar.withdrawals
            SET state = 'signed', signed_authorization_xdr = $2, signed_at = now()
            WHERE id = $1 AND state = 'awaiting_signature'
            "#,
            id,
            signed_authorization_xdr,
        )
        .execute(&mut *tx)
        .await
        .map_err(query("sign withdrawal"))?;
        if signed.rows_affected() != 1 {
            return Ok(WithdrawalSigning::NotOpen);
        }
        if buyer.available < buyer.amount {
            return Ok(WithdrawalSigning::InsufficientBalance);
        }
        sqlx::query!(
            r#"
            UPDATE pay_stellar.buyers b SET available = b.available - w.amount
            FROM pay_stellar.withdrawals w WHERE w.id = $1 AND b.id = w.buyer_id
            "#,
            id,
        )
        .execute(&mut *tx)
        .await
        .map_err(query("hold withdrawal"))?;
        tx.commit().await.map_err(query("commit withdrawal signing"))?;
        Ok(WithdrawalSigning::Held)
    }
}
