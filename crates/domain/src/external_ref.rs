use std::fmt;
use std::str::FromStr;

/// The seller's own identifier for a buyer, unique within one seller
/// deployment. It is the natural idempotency key of buyer creation, so its
/// alphabet excludes whitespace and control characters that would let two
/// visually identical references coexist.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExternalRef(String);

pub const EXTERNAL_REF_MAX_LEN: usize = 128;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("external reference must be 1-128 characters of [A-Za-z0-9._:@+-]")]
pub struct ExternalRefError;

impl ExternalRef {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ExternalRef {
    type Err = ExternalRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let allowed = |c: char| c.is_ascii_alphanumeric() || "._:@+-".contains(c);
        if s.is_empty() || s.len() > EXTERNAL_REF_MAX_LEN || !s.chars().all(allowed) {
            return Err(ExternalRefError);
        }
        Ok(Self(s.to_owned()))
    }
}

impl fmt::Display for ExternalRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_accepts_boundary_length() {
        let max = "a".repeat(EXTERNAL_REF_MAX_LEN);
        assert_eq!(max.parse::<ExternalRef>().map(|r| r.0), Ok(max));
    }

    #[test]
    fn test_parse_rejects_one_past_boundary_length() {
        let over = "a".repeat(EXTERNAL_REF_MAX_LEN + 1);
        assert_eq!(over.parse::<ExternalRef>(), Err(ExternalRefError));
    }

    #[test]
    fn test_parse_rejects_empty() {
        assert_eq!("".parse::<ExternalRef>(), Err(ExternalRefError));
    }

    #[test]
    fn test_parse_rejects_whitespace_and_non_ascii() {
        for raw in ["user 1", "user\t1", "user\n", "usér", "user/1"] {
            assert_eq!(raw.parse::<ExternalRef>(), Err(ExternalRefError), "{raw:?}");
        }
    }

    #[test]
    fn test_parse_accepts_full_alphabet() {
        assert!("Az09._:@+-".parse::<ExternalRef>().is_ok());
    }
}
