//! Operator tool for schema migration and tenant provisioning.
//!
//! `migrate` needs the database owner; the other commands need a login role
//! that is a member of `pay_stellar_issuer`.

use anyhow::Context;
use clap::{Parser, Subcommand};
use fermah_pay_stellar_domain::Network;
use fermah_pay_stellar_gateway::issuance;
use sqlx::PgPool;
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../db/migrations");

#[derive(Parser)]
#[command(name = "fermah-pay-stellar-admin", version, about)]
struct Cli {
    /// PostgreSQL URL for the role the command needs.
    #[arg(long, env = "PAY_STELLAR_ADMIN_DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply pending schema migrations.
    Migrate,
    /// Register a product.
    CreateProduct {
        #[arg(long)]
        name: String,
    },
    /// Register a seller deployment of a product, pinned to one network.
    CreateDeployment {
        #[arg(long)]
        product_id: Uuid,
        #[arg(long)]
        name: String,
        #[arg(long)]
        network: Network,
    },
    /// Issue an API key for a deployment. The token is printed once and
    /// cannot be recovered afterwards.
    IssueApiKey {
        #[arg(long)]
        deployment_id: Uuid,
        #[arg(long)]
        label: String,
    },
    /// Revoke an API key.
    RevokeApiKey {
        #[arg(long)]
        key_id: Uuid,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let pool = PgPool::connect(&cli.database_url).await.context("connecting to PostgreSQL")?;
    let output = match cli.command {
        Command::Migrate => {
            MIGRATOR.run(&pool).await.context("applying migrations")?;
            serde_json::json!({ "migrated": true })
        }
        Command::CreateProduct { name } => {
            let id = issuance::create_product(&pool, &name).await?;
            serde_json::json!({ "product_id": id.to_string() })
        }
        Command::CreateDeployment { product_id, name, network } => {
            let id = issuance::create_seller_deployment(&pool, product_id, &name, network).await?;
            serde_json::json!({ "deployment_id": id.to_string(), "network": network.caip2() })
        }
        Command::IssueApiKey { deployment_id, label } => {
            let key = issuance::issue_api_key(&pool, deployment_id, &label).await?;
            serde_json::json!({ "key_id": key.id.to_string(), "token": key.token.as_str() })
        }
        Command::RevokeApiKey { key_id } => {
            let revoked = issuance::revoke_api_key(&pool, key_id).await?;
            serde_json::json!({ "key_id": key_id.to_string(), "revoked": revoked })
        }
    };
    println!("{output}");
    Ok(())
}
