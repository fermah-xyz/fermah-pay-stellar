//! Upgrading a database that already holds charges to the charge-identifier
//! schema: the migration must carry final charges across and refuse while
//! any charge is still in flight.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const MIGRATIONS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../db/migrations");
const BUYER: &str = "GAAQEAYEAUDAOCAJBIFQYDIOB4IBCEQTCQKRMFYYDENBWHA5DYPSBVPU";
const SOURCE: &str = "GAFQWDAOCQPBCFSDKNOQZDBHDMSPUHZAEJPDMKZBA5KZXVBR5LLXDZ5T";

/// The migrations before the charge-identifier one, in a directory of their
/// own, so the database can be brought to the schema an installation had.
fn migrations_before_0007() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pay-stellar-upgrade-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    for entry in std::fs::read_dir(MIGRATIONS).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if name.as_str() < "0007" {
            std::fs::copy(&path, dir.join(name)).unwrap();
        }
    }
    dir
}

async fn migrate(pool: &PgPool, dir: &Path) -> Result<(), sqlx::migrate::MigrateError> {
    Migrator::new(dir).await.unwrap().run(pool).await
}

async fn pool(opts: PgPoolOptions, connect: PgConnectOptions) -> PgPool {
    let pool = opts.connect_with(connect).await.unwrap();
    let before = migrations_before_0007();
    migrate(&pool, &before).await.unwrap();
    std::fs::remove_dir_all(before).unwrap();
    pool
}

/// A deployment with one buyer and one succeeded batch, as the previous
/// schema stored them. Returns the buyer, deployment and submission ids.
async fn seed(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let product: Uuid = sqlx::query_scalar(
        "INSERT INTO pay_stellar.products (id, name) VALUES (gen_random_uuid(), 'shop') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let deployment: Uuid = sqlx::query_scalar(
        "INSERT INTO pay_stellar.seller_deployments (id, product_id, name, network)
         VALUES (gen_random_uuid(), $1, 'main', 'stellar:testnet') RETURNING id",
    )
    .bind(product)
    .fetch_one(pool)
    .await
    .unwrap();
    let buyer: Uuid = sqlx::query_scalar(
        "INSERT INTO pay_stellar.buyers
             (id, product_id, seller_deployment_id, network, external_ref, wallet_address)
         VALUES (gen_random_uuid(), $1, $2, 'stellar:testnet', 'buyer-1', $3) RETURNING id",
    )
    .bind(product)
    .bind(deployment)
    .bind(BUYER)
    .fetch_one(pool)
    .await
    .unwrap();
    let submission = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO pay_stellar.submissions
             (id, network, kind, state, source_address, fee_source_address, sequence,
              valid_until, inner_hash, outer_hash, envelope_xdr, ledger, result_xdr, resolved_at)
         VALUES ($1, 'stellar:testnet', 'charge_batch', 'succeeded', $2, $2, 1, now(),
                 $3, $4, 'AAAA', 10, 'AAAA', now())",
    )
    .bind(submission)
    .bind(SOURCE)
    .bind(vec![1_u8; 32])
    .bind(vec![2_u8; 32])
    .execute(pool)
    .await
    .unwrap();
    (buyer, deployment, submission)
}

async fn insert_charge(
    pool: &PgPool,
    (buyer, deployment, submission): (Uuid, Uuid, Uuid),
    key: &str,
    sequence: i64,
    state: &str,
    outcome: Option<&str>,
    batch_index: i16,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO pay_stellar.charges
             (id, buyer_id, seller_deployment_id, network, idempotency_key, amount, sequence,
              state, outcome, submission_id, batch_index, settled_at)
         VALUES ($1, $2, $3, 'stellar:testnet', $4, 10, $5, $6, $7, $8, $9,
                 CASE WHEN $6 IN ('charged', 'refused', 'quarantined') THEN now() END)",
    )
    .bind(id)
    .bind(buyer)
    .bind(deployment)
    .bind(key)
    .bind(sequence)
    .bind(state)
    .bind(outcome)
    .bind(submission)
    .bind(batch_index)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Final charges from a 100-charge batch, as the previous batch bound
/// allowed.
async fn final_history(pool: &PgPool) -> ((Uuid, Uuid, Uuid), Uuid, Uuid) {
    let parent = seed(pool).await;
    let charged = insert_charge(pool, parent, "key-1", 1, "charged", Some("charged"), 99).await;
    let refused =
        insert_charge(pool, parent, "key-2", 2, "refused", Some("insufficient_balance"), 0).await;
    (parent, charged, refused)
}

#[sqlx::test(migrations = false)]
async fn test_upgrade_carries_final_charges_across(opts: PgPoolOptions, connect: PgConnectOptions) {
    let pool = pool(opts, connect).await;
    let (parent, charged, refused) = final_history(&pool).await;

    migrate(&pool, Path::new(MIGRATIONS)).await.unwrap();

    for (id, key, index) in [(charged, "key-1", 99_i16), (refused, "key-2", 0)] {
        let row = sqlx::query(
            "SELECT charge_id, last_ledger, batch_index FROM pay_stellar.charges WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<Vec<u8>, _>("charge_id"), Sha256::digest(key.as_bytes()).to_vec());
        assert_eq!(row.get::<i64, _>("last_ledger"), 1);
        assert_eq!(row.get::<i16, _>("batch_index"), index);
    }
    // Final charges are immutable again once the backfill is done.
    let error = sqlx::query("UPDATE pay_stellar.charges SET last_error = 'x' WHERE id = $1")
        .bind(charged)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("is already charged"), "{error}");
    // The new batch bound holds for new rows.
    let (buyer, deployment, submission) = parent;
    let insert = |index: i16| {
        sqlx::query(
            "INSERT INTO pay_stellar.charges
                 (id, buyer_id, seller_deployment_id, network, idempotency_key, amount, charge_id,
                  last_ledger, state, submission_id, batch_index)
             VALUES (gen_random_uuid(), $1, $2, 'stellar:testnet', $3, 10, $4, 100, 'submitted',
                     $5, $6)",
        )
        .bind(buyer)
        .bind(deployment)
        .bind(format!("new-{index}"))
        .bind(Sha256::digest(format!("new-{index}").as_bytes()).to_vec())
        .bind(submission)
        .bind(index)
        .execute(&pool)
    };
    insert(97).await.unwrap();
    let error = insert(98).await.unwrap_err();
    assert!(error.to_string().contains("charges_batch_index_check"), "{error}");
}

#[sqlx::test(migrations = false)]
async fn test_upgrade_refuses_while_a_charge_is_in_flight(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let pool = pool(opts, connect).await;
    let (parent, _, _) = final_history(&pool).await;
    // The one property that differs from the upgrade above.
    insert_charge(&pool, parent, "key-3", 3, "submitted", None, 1).await;

    let error = migrate(&pool, Path::new(MIGRATIONS)).await.unwrap_err();
    assert!(error.to_string().contains("settle or resolve every charge"), "{error}");
    // Nothing of the migration remains.
    let sequence_column: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = 'pay_stellar' AND table_name = 'charges'
                          AND column_name = 'sequence')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(sequence_column);
}
