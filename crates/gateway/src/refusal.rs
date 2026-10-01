//! Stable refusal reasons: the only error vocabulary callers may branch on.
//! Each is sent as the gRPC status message and documented in
//! `docs/api/`; internal error detail is logged, never returned.

use tonic::{Code, Status};

use crate::labels::labels;

labels! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Refusal as reason {
        Unauthenticated => "unauthenticated",
        InvalidExternalRef => "invalid_external_ref",
        InvalidWalletAddress => "invalid_wallet_address",
        UnsupportedWalletAddress => "unsupported_wallet_address",
        InvalidBuyerId => "invalid_buyer_id",
        MissingLookup => "missing_lookup",
        BuyerConflict => "buyer_conflict",
        BuyerNotFound => "buyer_not_found",
        InvalidAmount => "invalid_amount",
        InvalidIdempotencyKey => "invalid_idempotency_key",
        IdempotencyConflict => "idempotency_conflict",
        InsufficientBalance => "insufficient_balance",
        LedgerNotConfigured => "ledger_not_configured",
        InvalidDepositId => "invalid_deposit_id",
        DepositNotFound => "deposit_not_found",
        InvalidAuthorizationEntry => "invalid_authorization_entry",
        AuthorizationMismatch => "authorization_mismatch",
        InvalidSignature => "invalid_signature",
        /// A contract account's entry: the network refused the call with it
        /// when simulated.
        AuthorizationRefused => "authorization_refused",
        /// A contract account's call would cost the operator more than a
        /// buyer's transaction may.
        WalletTooCostly => "wallet_too_costly",
        DepositExpired => "deposit_expired",
        DepositAlreadySigned => "deposit_already_signed",
        InvalidChargeId => "invalid_charge_id",
        ChargeNotFound => "charge_not_found",
        InvalidWithdrawalId => "invalid_withdrawal_id",
        WithdrawalNotFound => "withdrawal_not_found",
        WithdrawalExpired => "withdrawal_expired",
        WithdrawalAlreadySigned => "withdrawal_already_signed",
        InvalidDestination => "invalid_destination",
        DestinationNotAllowed => "destination_not_allowed",
        WithdrawalBelowMinimum => "withdrawal_below_minimum",
        BuyerQuotaExceeded => "buyer_quota_exceeded",
        DepositQuotaExceeded => "deposit_quota_exceeded",
        WithdrawalQuotaExceeded => "withdrawal_quota_exceeded",
        InvalidMandateId => "invalid_mandate_id",
        MandateNotFound => "mandate_not_found",
        MandateAuthorizationExpired => "mandate_authorization_expired",
        MandateAlreadySigned => "mandate_already_signed",
        InvalidPeriod => "invalid_period",
        InvalidCycles => "invalid_cycles",
        MandateTooLong => "mandate_too_long",
        MandateQuotaExceeded => "mandate_quota_exceeded",
        DeploymentDepositQuotaExceeded => "deployment_deposit_quota_exceeded",
        DeploymentMandateQuotaExceeded => "deployment_mandate_quota_exceeded",
        InvalidRevocationId => "invalid_revocation_id",
        RevocationNotFound => "revocation_not_found",
        RevocationExpired => "revocation_expired",
        RevocationAlreadySigned => "revocation_already_signed",
        MandateNotActive => "mandate_not_active",
        MandateEnded => "mandate_ended",
        AboveMandate => "above_mandate",
        PeriodAlreadyCharged => "period_already_charged",
        InvalidRecurringChargeId => "invalid_recurring_charge_id",
        RecurringChargeNotFound => "recurring_charge_not_found",
        NetworkUnavailable => "network_unavailable",
        Internal => "internal",
    }
}

impl Refusal {
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
            | Self::AuthorizationRefused
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
            | Self::MandateQuotaExceeded
            | Self::DeploymentDepositQuotaExceeded
            | Self::DeploymentMandateQuotaExceeded => Code::ResourceExhausted,
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
            | Self::MandateAuthorizationExpired
            | Self::MandateAlreadySigned
            | Self::RevocationExpired
            | Self::RevocationAlreadySigned
            | Self::MandateNotActive
            | Self::MandateEnded
            | Self::DestinationNotAllowed
            | Self::WalletTooCostly => Code::FailedPrecondition,
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
