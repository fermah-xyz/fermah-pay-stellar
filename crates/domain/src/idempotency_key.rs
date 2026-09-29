use std::fmt;
use std::str::FromStr;

use crate::ExternalRef;

/// A caller-chosen key that makes a money-moving request safe to retry: the
/// first request with a key within a deployment acts, every later one returns
/// its result. Same alphabet as [`ExternalRef`], for the same reason.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("idempotency key must be 1-128 characters of [A-Za-z0-9._:@+-]")]
pub struct IdempotencyKeyError;

impl IdempotencyKey {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for IdempotencyKey {
    type Err = IdempotencyKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid: ExternalRef = s.parse().map_err(|_| IdempotencyKeyError)?;
        Ok(Self(valid.as_str().to_owned()))
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
