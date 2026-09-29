//! Stable refusal reasons: the only error vocabulary callers may branch on.
//! Each is sent as the gRPC status message and documented in
//! `docs/api/`; internal error detail is logged, never returned.

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
    InvalidAmount,
    InvalidIdempotencyKey,
    IdempotencyConflict,
    InsufficientBalance,
    LedgerNotConfigured,
    InvalidDepositId,
    DepositNotFound,
    InvalidAuthorizationEntry,
    AuthorizationMismatch,
    InvalidSignature,
    DepositExpired,
    DepositAlreadySigned,
    InvalidChargeId,
    ChargeNotFound,
    NetworkUnavailable,
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
            Self::InvalidAmount => "invalid_amount",
            Self::InvalidIdempotencyKey => "invalid_idempotency_key",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::InsufficientBalance => "insufficient_balance",
            Self::LedgerNotConfigured => "ledger_not_configured",
            Self::InvalidDepositId => "invalid_deposit_id",
            Self::DepositNotFound => "deposit_not_found",
            Self::InvalidAuthorizationEntry => "invalid_authorization_entry",
            Self::AuthorizationMismatch => "authorization_mismatch",
            Self::InvalidSignature => "invalid_signature",
            Self::DepositExpired => "deposit_expired",
            Self::DepositAlreadySigned => "deposit_already_signed",
            Self::InvalidChargeId => "invalid_charge_id",
            Self::ChargeNotFound => "charge_not_found",
            Self::NetworkUnavailable => "network_unavailable",
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
            | Self::MissingLookup
            | Self::InvalidAmount
            | Self::InvalidIdempotencyKey
            | Self::InvalidDepositId
            | Self::InvalidAuthorizationEntry
            | Self::AuthorizationMismatch
            | Self::InvalidSignature
            | Self::InvalidChargeId => Code::InvalidArgument,
            Self::BuyerConflict | Self::IdempotencyConflict => Code::AlreadyExists,
            Self::BuyerNotFound | Self::DepositNotFound | Self::ChargeNotFound => Code::NotFound,
            Self::InsufficientBalance
            | Self::LedgerNotConfigured
            | Self::DepositExpired
            | Self::DepositAlreadySigned => Code::FailedPrecondition,
            Self::NetworkUnavailable => Code::Unavailable,
            Self::Internal => Code::Internal,
        }
    }
}

impl From<Refusal> for Status {
    fn from(refusal: Refusal) -> Self {
        Self::new(refusal.code(), refusal.reason())
    }
}
