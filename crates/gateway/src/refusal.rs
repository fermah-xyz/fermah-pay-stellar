//! Stable refusal reasons: the only error vocabulary callers may branch on.
//! Each is sent as the gRPC status message and documented in
//! `docs/api/buyer.md`; internal error detail is logged, never returned.

use tonic::{Code, Status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Unauthenticated,
    InvalidExternalRef,
    InvalidWalletAddress,
    UnsupportedWalletAddress,
    InvalidBuyerId,
    MissingLookup,
    BuyerConflict,
    BuyerNotFound,
    Internal,
}

impl Refusal {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::InvalidExternalRef => "invalid_external_ref",
            Self::InvalidWalletAddress => "invalid_wallet_address",
            Self::UnsupportedWalletAddress => "unsupported_wallet_address",
            Self::InvalidBuyerId => "invalid_buyer_id",
            Self::MissingLookup => "missing_lookup",
            Self::BuyerConflict => "buyer_conflict",
            Self::BuyerNotFound => "buyer_not_found",
            Self::Internal => "internal",
        }
    }

    #[must_use]
    pub const fn code(self) -> Code {
        match self {
            Self::Unauthenticated => Code::Unauthenticated,
            Self::InvalidExternalRef
            | Self::InvalidWalletAddress
            | Self::UnsupportedWalletAddress
            | Self::InvalidBuyerId
            | Self::MissingLookup => Code::InvalidArgument,
            Self::BuyerConflict => Code::AlreadyExists,
            Self::BuyerNotFound => Code::NotFound,
            Self::Internal => Code::Internal,
        }
    }
}

impl From<Refusal> for Status {
    fn from(refusal: Refusal) -> Self {
        Self::new(refusal.code(), refusal.reason())
    }
}
