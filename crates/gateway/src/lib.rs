//! Fermah Pay Stellar gateway: the authenticated seller API over PostgreSQL.

#![forbid(unsafe_code)]

pub mod auth;
pub mod buyers;
pub mod config;
pub mod issuance;
pub mod refusal;
pub mod scope;
pub mod server;
pub mod store;
pub mod submission;
