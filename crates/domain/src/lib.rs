//! Values validated once at the system edge and trusted everywhere after.

#![forbid(unsafe_code)]

mod account;
mod external_ref;
mod idempotency_key;
mod network;

pub use account::{AccountAddress, AccountAddressError};
pub use external_ref::{ExternalRef, ExternalRefError};
pub use idempotency_key::{IdempotencyKey, IdempotencyKeyError};
pub use network::{Network, UnknownNetwork};
