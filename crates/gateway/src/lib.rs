//! Fermah Pay Stellar gateway: the authenticated seller API over PostgreSQL.

#![forbid(unsafe_code)]

pub mod auth;
pub mod buyers;
pub mod config;
pub mod events;
pub mod issuance;
pub mod ledger;
pub mod observer;
pub mod quarantine;
pub mod refusal;
pub mod scope;
pub mod server;
pub mod shutdown;
pub mod startup;
pub mod store;
pub mod submission;
pub mod worker;
