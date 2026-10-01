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
    InvalidWithdrawalId,
    WithdrawalNotFound,
    WithdrawalExpired,
    WithdrawalAlreadySigned,
    InvalidDestination,
    WithdrawalBelowMinimum,
    BuyerQuotaExceeded,
    DepositQuotaExceeded,
    WithdrawalQuotaExceeded,
    InvalidMandateId,
    MandateNotFound,
    MandateExpired,
    MandateAlreadySigned,
    InvalidPeriod,
    InvalidCycles,
    MandateTooLong,
    MandateQuotaExceeded,
    InvalidRevocationId,
    RevocationNotFound,
    RevocationExpired,
    RevocationAlreadySigned,
    MandateNotActive,
    MandateEnded,
    AboveMandate,
    PeriodAlreadyCharged,
    InvalidRecurringChargeId,
    RecurringChargeNotFound,
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
            Self::InvalidWithdrawalId => "invalid_withdrawal_id",
            Self::WithdrawalNotFound => "withdrawal_not_found",
            Self::WithdrawalExpired => "withdrawal_expired",
            Self::WithdrawalAlreadySigned => "withdrawal_already_signed",
            Self::InvalidDestination => "invalid_destination",
            Self::WithdrawalBelowMinimum => "withdrawal_below_minimum",
            Self::BuyerQuotaExceeded => "buyer_quota_exceeded",
            Self::DepositQuotaExceeded => "deposit_quota_exceeded",
            Self::WithdrawalQuotaExceeded => "withdrawal_quota_exceeded",
            Self::InvalidMandateId => "invalid_mandate_id",
            Self::MandateNotFound => "mandate_not_found",
            Self::MandateExpired => "mandate_expired",
            Self::MandateAlreadySigned => "mandate_already_signed",
            Self::InvalidPeriod => "invalid_period",
            Self::InvalidCycles => "invalid_cycles",
            Self::MandateTooLong => "mandate_too_long",
            Self::MandateQuotaExceeded => "mandate_quota_exceeded",
            Self::InvalidRevocationId => "invalid_revocation_id",
            Self::RevocationNotFound => "revocation_not_found",
            Self::RevocationExpired => "revocation_expired",
            Self::RevocationAlreadySigned => "revocation_already_signed",
            Self::MandateNotActive => "mandate_not_active",
            Self::MandateEnded => "mandate_ended",
            Self::AboveMandate => "above_mandate",
            Self::PeriodAlreadyCharged => "period_already_charged",
            Self::InvalidRecurringChargeId => "invalid_recurring_charge_id",
            Self::RecurringChargeNotFound => "recurring_charge_not_found",
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
            | Self::InvalidChargeId
            | Self::InvalidWithdrawalId
            | Self::InvalidDestination
            | Self::WithdrawalBelowMinimum
            | Self::InvalidMandateId
            | Self::InvalidPeriod
            | Self::InvalidCycles
            | Self::MandateTooLong
            | Self::InvalidRevocationId
            | Self::AboveMandate
            | Self::InvalidRecurringChargeId => Code::InvalidArgument,
            Self::BuyerQuotaExceeded
            | Self::DepositQuotaExceeded
            | Self::WithdrawalQuotaExceeded
            | Self::MandateQuotaExceeded => Code::ResourceExhausted,
            Self::BuyerConflict | Self::IdempotencyConflict | Self::PeriodAlreadyCharged => {
                Code::AlreadyExists
            }
            Self::BuyerNotFound
            | Self::DepositNotFound
            | Self::ChargeNotFound
            | Self::WithdrawalNotFound
            | Self::MandateNotFound
            | Self::RevocationNotFound
            | Self::RecurringChargeNotFound => Code::NotFound,
            Self::InsufficientBalance
            | Self::LedgerNotConfigured
            | Self::DepositExpired
            | Self::DepositAlreadySigned
            | Self::WithdrawalExpired
            | Self::WithdrawalAlreadySigned
            | Self::MandateExpired
            | Self::MandateAlreadySigned
            | Self::RevocationExpired
            | Self::RevocationAlreadySigned
            | Self::MandateNotActive
            | Self::MandateEnded => Code::FailedPrecondition,
            Self::NetworkUnavailable => Code::Unavailable,
            Self::Internal => Code::Internal,
        }
    }
}

impl From<Refusal> for Status {
    fn from(refusal: Refusal) -> Self {
        metrics::counter!("pay_stellar_api_refusals_total", "reason" => refusal.reason())
            .increment(1);
        Self::new(refusal.code(), refusal.reason())
    }
}
