//! What an observed event means against the gateway's records.
//!
//! Each judgment is a pure function of the event and the rows it matches, so
//! the rules can be read, and tested, apart from how they are stored.

use fermah_pay_stellar_chain::prepaid::{ChargeEntry, Outcome, Role};
use serde_json::{Value, json};

/// How urgently a finding needs a person.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Money may be wrong: on-chain state the records contradict.
    Critical,
    /// Something needs a person's attention, but no balance is wrong because
    /// of it on its own.
    Warning,
    Info,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

/// The kinds of finding; each is documented in
/// `docs/self-hosting/observer.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FindingKind {
    EventGap,
    UnrecognizedEvent,
    UnknownCharge,
    ChargeAmountMismatch,
    ChargeOutcomeMismatch,
    ChargeUnsettled,
    UnknownDeposit,
    DepositAmountMismatch,
    DepositOutcomeMismatch,
    DepositUnsettled,
    UnknownWithdrawal,
    WithdrawalMismatch,
    WithdrawalOutcomeMismatch,
    RoleChanged,
    AdminChange,
    BindingOutOfDate,
    TreasuryDeficit,
    TreasurySurplus,
    TreasuryDeauthorized,
    EventTotalsMismatch,
    LedgerTotalsMismatch,
}

impl FindingKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EventGap => "event_gap",
            Self::UnrecognizedEvent => "unrecognized_event",
            Self::UnknownCharge => "unknown_charge",
            Self::ChargeAmountMismatch => "charge_amount_mismatch",
            Self::ChargeOutcomeMismatch => "charge_outcome_mismatch",
            Self::ChargeUnsettled => "charge_unsettled",
            Self::UnknownDeposit => "unknown_deposit",
            Self::DepositAmountMismatch => "deposit_amount_mismatch",
            Self::DepositOutcomeMismatch => "deposit_outcome_mismatch",
            Self::DepositUnsettled => "deposit_unsettled",
            Self::UnknownWithdrawal => "unknown_withdrawal",
            Self::WithdrawalMismatch => "withdrawal_mismatch",
            Self::WithdrawalOutcomeMismatch => "withdrawal_outcome_mismatch",
            Self::RoleChanged => "role_changed",
            Self::AdminChange => "admin_change",
            Self::BindingOutOfDate => "binding_out_of_date",
            Self::TreasuryDeficit => "treasury_deficit",
            Self::TreasurySurplus => "treasury_surplus",
            Self::TreasuryDeauthorized => "treasury_deauthorized",
            Self::EventTotalsMismatch => "event_totals_mismatch",
            Self::LedgerTotalsMismatch => "ledger_totals_mismatch",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub kind: FindingKind,
    pub severity: Severity,
    pub detail: Value,
}

impl Finding {
    #[must_use]
    pub const fn new(kind: FindingKind, severity: Severity, detail: Value) -> Self {
        Self { kind, severity, detail }
    }
}

/// What an observed subject (a charge entry, a deposit, a role change)
/// amounts to at one check.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Matched,
    /// The records may still catch up; checked again later.
    Pending,
    Finding(Finding),
}

/// A charge row as the observer reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChargeRow {
    pub id: uuid::Uuid,
    pub amount: i64,
    pub state: String,
    pub outcome: Option<String>,
}

/// A deposit row as the observer reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositRow {
    pub id: uuid::Uuid,
    pub amount: i64,
    pub state: String,
}

/// A withdrawal row as the observer reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WithdrawalRow {
    pub id: uuid::Uuid,
    pub amount: i64,
    pub destination: String,
    pub state: String,
}

/// Whether the entry debited the buyer's balance on-chain.
const fn debited(outcome: Outcome) -> bool {
    matches!(outcome, Outcome::Charged)
}

/// Judges one entry of a `charges` event against the charge the gateway
/// recorded under the same account and identifier. `overdue` is whether the
/// settlement grace after the event's ledger has passed.
#[must_use]
pub fn judge_charge(entry: &ChargeEntry, row: Option<&ChargeRow>, overdue: bool) -> Verdict {
    let base = || {
        json!({
            "owner": entry.owner.to_string(),
            "charge_id": fermah_pay_stellar_chain::rpc::hex_lower(&entry.charge_id),
            "amount": entry.amount.to_string(),
            "outcome": entry.outcome.token(),
        })
    };
    let with_row = |row: &ChargeRow| {
        let mut detail = base();
        detail["charge"] = json!({
            "id": row.id.to_string(),
            "amount": row.amount.to_string(),
            "state": row.state,
            "outcome": row.outcome,
        });
        detail
    };
    let Some(row) = row else {
        // The operator key signed a charge the gateway never admitted. Only
        // a debit moved money; a refusal still shows the key was used.
        let severity = if debited(entry.outcome) { Severity::Critical } else { Severity::Warning };
        return Verdict::Finding(Finding::new(FindingKind::UnknownCharge, severity, base()));
    };
    if i128::from(row.amount) != entry.amount {
        return Verdict::Finding(Finding::new(
            FindingKind::ChargeAmountMismatch,
            Severity::Critical,
            with_row(row),
        ));
    }
    // A duplicate or expired entry debited nothing and recorded nothing: the
    // charge's own settlement, an earlier entry, decides its outcome.
    if matches!(entry.outcome, Outcome::Duplicate | Outcome::Expired) {
        return Verdict::Matched;
    }
    let mismatch = |severity| {
        Verdict::Finding(Finding::new(FindingKind::ChargeOutcomeMismatch, severity, with_row(row)))
    };
    match row.state.as_str() {
        "charged" if debited(entry.outcome) => Verdict::Matched,
        "charged" => mismatch(Severity::Critical),
        "refused" if debited(entry.outcome) => mismatch(Severity::Critical),
        "refused" if row.outcome.as_deref() == Some(entry.outcome.token()) => Verdict::Matched,
        // Neither side debited; only the recorded reason differs.
        "refused" => mismatch(Severity::Warning),
        // The worker quarantines an `unknown_account` answer for a person to
        // look at; that is the gateway recognizing this outcome.
        "quarantined"
            if entry.outcome == Outcome::UnknownAccount
                && row.outcome.as_deref() == Some(Outcome::UnknownAccount.token()) =>
        {
            Verdict::Matched
        }
        _ if overdue => Verdict::Finding(Finding::new(
            FindingKind::ChargeUnsettled,
            Severity::Warning,
            with_row(row),
        )),
        _ => Verdict::Pending,
    }
}

/// Judges a `deposit` event against the deposit the gateway recorded under
/// the same account and deposit id.
#[must_use]
pub fn judge_deposit(
    owner: &str,
    amount: i128,
    deposit_id: &[u8; 32],
    row: Option<&DepositRow>,
    overdue: bool,
) -> Verdict {
    let base = || {
        json!({
            "owner": owner,
            "deposit_id": fermah_pay_stellar_chain::rpc::hex_lower(deposit_id),
            "amount": amount.to_string(),
        })
    };
    let Some(row) = row else {
        // Anyone may deposit to the contract directly. The credit is real on
        // the contract but not part of the buyer's available balance.
        return Verdict::Finding(Finding::new(
            FindingKind::UnknownDeposit,
            Severity::Warning,
            base(),
        ));
    };
    let with_row = || {
        let mut detail = base();
        detail["deposit"] = json!({
            "id": row.id.to_string(),
            "amount": row.amount.to_string(),
            "state": row.state,
        });
        detail
    };
    if i128::from(row.amount) != amount {
        return Verdict::Finding(Finding::new(
            FindingKind::DepositAmountMismatch,
            Severity::Critical,
            with_row(),
        ));
    }
    match row.state.as_str() {
        "confirmed" => Verdict::Matched,
        // Final and never credited, although the contract credited it.
        "failed" | "expired" => Verdict::Finding(Finding::new(
            FindingKind::DepositOutcomeMismatch,
            Severity::Critical,
            with_row(),
        )),
        _ if overdue => Verdict::Finding(Finding::new(
            FindingKind::DepositUnsettled,
            Severity::Warning,
            with_row(),
        )),
        _ => Verdict::Pending,
    }
}

/// Judges a `withdraw` event. The row exists before any treasury
/// authorization is signed for it, so there is nothing to wait for: an
/// event without one, or that disagrees with it, is final.
#[must_use]
pub fn judge_withdrawal(
    owner: &str,
    destination: &str,
    amount: i128,
    withdrawal_id: &[u8; 32],
    row: Option<&WithdrawalRow>,
) -> Verdict {
    let mut detail = json!({
        "owner": owner,
        "destination": destination,
        "withdrawal_id": fermah_pay_stellar_chain::rpc::hex_lower(withdrawal_id),
        "amount": amount.to_string(),
    });
    let Some(row) = row else {
        // The treasury authorized a withdrawal the gateway never prepared:
        // its key was used outside the worker.
        return Verdict::Finding(Finding::new(
            FindingKind::UnknownWithdrawal,
            Severity::Warning,
            detail,
        ));
    };
    detail["withdrawal"] = json!({
        "id": row.id.to_string(),
        "amount": row.amount.to_string(),
        "destination": row.destination,
        "state": row.state,
    });
    if i128::from(row.amount) != amount || row.destination != destination {
        return Verdict::Finding(Finding::new(
            FindingKind::WithdrawalMismatch,
            Severity::Critical,
            detail,
        ));
    }
    match row.state.as_str() {
        "signed" | "submitted" | "confirmed" => Verdict::Matched,
        // Never held, or held and returned, although the USDC left.
        _ => Verdict::Finding(Finding::new(
            FindingKind::WithdrawalOutcomeMismatch,
            Severity::Critical,
            detail,
        )),
    }
}

/// Judges a `role` event. The binding names the operator and the treasury;
/// nothing in the gateway's records can confirm an admin or seller rotation
/// was intended, so a person must.
#[must_use]
pub fn judge_role(
    role: Role,
    previous: &str,
    current: &str,
    bound: Option<&str>,
    superseded: bool,
    overdue: bool,
) -> Verdict {
    let detail = || json!({ "role": role.token(), "previous": previous, "current": current });
    match role {
        Role::Admin | Role::Seller => {
            Verdict::Finding(Finding::new(FindingKind::RoleChanged, Severity::Warning, detail()))
        }
        Role::Operator | Role::Treasury if bound == Some(current) || superseded => Verdict::Matched,
        Role::Operator | Role::Treasury if overdue => {
            let mut detail = detail();
            detail["bound"] = json!(bound);
            Verdict::Finding(Finding::new(FindingKind::BindingOutOfDate, Severity::Warning, detail))
        }
        Role::Operator | Role::Treasury => Verdict::Pending,
    }
}

#[cfg(test)]
mod tests {
    use fermah_pay_stellar_chain::prepaid::ChainAddress;
    use fermah_pay_stellar_domain::AccountAddress;

    use super::*;

    fn entry(amount: i128, outcome: Outcome) -> ChargeEntry {
        ChargeEntry {
            owner: ChainAddress::Account(AccountAddress::from_public_key([1; 32])),
            charge_id: [2; 32],
            amount,
            outcome,
        }
    }

    fn row(amount: i64, state: &str, outcome: Option<&str>) -> ChargeRow {
        ChargeRow {
            id: uuid::Uuid::nil(),
            amount,
            state: state.to_owned(),
            outcome: outcome.map(str::to_owned),
        }
    }

    fn kind(verdict: &Verdict) -> Option<(FindingKind, Severity)> {
        match verdict {
            Verdict::Finding(finding) => Some((finding.kind, finding.severity)),
            _ => None,
        }
    }

    #[test]
    fn test_charge_outcomes_against_final_rows() {
        use Outcome::*;
        type Case = (Outcome, &'static str, Option<&'static str>, Option<(FindingKind, Severity)>);
        let cases: [Case; 9] = [
            (Charged, "charged", Some("charged"), None),
            (
                Charged,
                "refused",
                Some("insufficient_balance"),
                Some((FindingKind::ChargeOutcomeMismatch, Severity::Critical)),
            ),
            (InsufficientBalance, "refused", Some("insufficient_balance"), None),
            (AboveLimit, "refused", Some("above_limit"), None),
            (
                AboveLimit,
                "refused",
                Some("expired"),
                Some((FindingKind::ChargeOutcomeMismatch, Severity::Warning)),
            ),
            (
                InsufficientBalance,
                "charged",
                Some("charged"),
                Some((FindingKind::ChargeOutcomeMismatch, Severity::Critical)),
            ),
            (UnknownAccount, "quarantined", Some("unknown_account"), None),
            (
                UnknownAccount,
                "charged",
                Some("charged"),
                Some((FindingKind::ChargeOutcomeMismatch, Severity::Critical)),
            ),
            (
                UnknownAccount,
                "refused",
                Some("expired"),
                Some((FindingKind::ChargeOutcomeMismatch, Severity::Warning)),
            ),
        ];
        for (outcome, state, recorded, expected) in cases {
            let verdict = judge_charge(&entry(30, outcome), Some(&row(30, state, recorded)), false);
            assert_eq!(kind(&verdict), expected, "{outcome:?} against {state}/{recorded:?}");
            if expected.is_none() {
                assert_eq!(verdict, Verdict::Matched);
            }
        }
    }

    #[test]
    fn test_duplicate_and_expired_entries_are_not_debits() {
        for outcome in [Outcome::Duplicate, Outcome::Expired] {
            for (state, recorded) in [("charged", Some("charged")), ("refused", Some("expired"))] {
                let verdict =
                    judge_charge(&entry(30, outcome), Some(&row(30, state, recorded)), false);
                assert_eq!(verdict, Verdict::Matched, "{outcome:?} against {state}");
            }
        }
    }

    #[test]
    fn test_unsettled_charge_waits_until_overdue() {
        for state in ["admitted", "submitted", "quarantined"] {
            let unsettled = row(30, state, None);
            assert_eq!(
                judge_charge(&entry(30, Outcome::Charged), Some(&unsettled), false),
                Verdict::Pending
            );
            assert_eq!(
                kind(&judge_charge(&entry(30, Outcome::Charged), Some(&unsettled), true)),
                Some((FindingKind::ChargeUnsettled, Severity::Warning))
            );
        }
    }

    #[test]
    fn test_unknown_charge_is_critical_only_when_it_debited() {
        assert_eq!(
            kind(&judge_charge(&entry(30, Outcome::Charged), None, false)),
            Some((FindingKind::UnknownCharge, Severity::Critical))
        );
        assert_eq!(
            kind(&judge_charge(&entry(30, Outcome::Expired), None, false)),
            Some((FindingKind::UnknownCharge, Severity::Warning))
        );
    }

    #[test]
    fn test_amount_mismatch_wins_over_every_outcome() {
        for outcome in [Outcome::Charged, Outcome::Duplicate] {
            assert_eq!(
                kind(&judge_charge(
                    &entry(31, outcome),
                    Some(&row(30, "charged", Some("charged"))),
                    false
                )),
                Some((FindingKind::ChargeAmountMismatch, Severity::Critical))
            );
        }
    }

    #[test]
    fn test_deposit_states() {
        let deposit = |state: &str| DepositRow {
            id: uuid::Uuid::nil(),
            amount: 100,
            state: state.to_owned(),
        };
        let judge =
            |row: Option<&DepositRow>, overdue| judge_deposit("G", 100, &[3; 32], row, overdue);
        assert_eq!(judge(Some(&deposit("confirmed")), false), Verdict::Matched);
        assert_eq!(judge(Some(&deposit("submitted")), false), Verdict::Pending);
        assert_eq!(
            kind(&judge(Some(&deposit("signed")), true)),
            Some((FindingKind::DepositUnsettled, Severity::Warning))
        );
        assert_eq!(
            kind(&judge(Some(&deposit("expired")), false)),
            Some((FindingKind::DepositOutcomeMismatch, Severity::Critical))
        );
        assert_eq!(
            kind(&judge(None, false)),
            Some((FindingKind::UnknownDeposit, Severity::Warning))
        );
        let other = DepositRow { amount: 99, ..deposit("confirmed") };
        assert_eq!(
            kind(&judge(Some(&other), false)),
            Some((FindingKind::DepositAmountMismatch, Severity::Critical))
        );
    }

    #[test]
    fn test_roles() {
        assert_eq!(judge_role(Role::Operator, "A", "B", Some("B"), false, true), Verdict::Matched);
        assert_eq!(judge_role(Role::Operator, "A", "B", Some("A"), true, true), Verdict::Matched);
        assert_eq!(judge_role(Role::Treasury, "A", "B", Some("A"), false, false), Verdict::Pending);
        assert_eq!(
            kind(&judge_role(Role::Treasury, "A", "B", Some("A"), false, true)),
            Some((FindingKind::BindingOutOfDate, Severity::Warning))
        );
        assert_eq!(
            kind(&judge_role(Role::Admin, "A", "B", None, false, false)),
            Some((FindingKind::RoleChanged, Severity::Warning))
        );
    }
}
