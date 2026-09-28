//! Values validated once at the system edge and trusted everywhere after.

#![forbid(unsafe_code)]

mod account;
mod external_ref;
mod network;

pub use account::{AccountAddress, AccountAddressError};
pub use external_ref::{ExternalRef, ExternalRefError};
pub use network::{Network, UnknownNetwork};
