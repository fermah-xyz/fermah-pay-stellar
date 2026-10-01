//! Recurring charges against the real host: the buyer's single signed
//! authorization (mandate plus USDC approval), period accounting from ledger
//! time, the refusals, revocation, and the allowance as the USDC contract
//! itself reports it.

use fermah_pay_stellar_chain::prepaid::{
    ChainAddress, LedgerEvent, MandateIntent, MandateTerms, RecurringChargeRequest, RecurringEntry,
    allowance_of, ledger_event, recurring_outcomes, recurring_record, stored_mandate,
};
use soroban_sdk::testutils::storage::Persistent as _;

use super::*;

const MONTH: u64 = 30 * 86_400;
const START: u64 = 1_000_000;
/// Ledgers a test mandate lives.
const LIFETIME: u32 = 1_000;

fn new_world() -> World {
    let w = world();
    w.env.ledger().set_timestamp(START);
    w
}

fn mandate(w: &World, buyer: &Party, id: u8, amount: i128, cycles: u32) -> MandateIntent {
    MandateIntent {
        owner: buyer.key.address(),
        mandate_id: id32(id),
        amount,
        period_secs: MONTH,
        cycles,
        live_until: w.env.ledger().sequence() + LIFETIME,
    }
}

fn mandate_args(w: &World, intent: &MandateIntent, owner: &Address) -> soroban_sdk::Vec<Val> {
    (
        owner.clone(),
        BytesN::from_array(&w.env, &intent.mandate_id),
        intent.amount,
        intent.period_secs,
        intent.cycles,
        intent.live_until,
    )
        .into_val(&w.env)
}

pub(super) fn authorize(
    w: &World,
    buyer: &Party,
    intent: &MandateIntent,
) -> Result<(), soroban_sdk::Error> {
    let auth = w.signed(buyer, w.deployment().authorize_recurring_authorization(intent));
    w.invoke("authorize_recurring", mandate_args(w, intent, &buyer.address), &[auth])
}

pub(super) fn revoke(w: &World, buyer: &Party) -> Result<(), soroban_sdk::Error> {
    let auth = w.signed(buyer, w.deployment().revoke_recurring_authorization(&buyer.key.address()));
    w.invoke("revoke_recurring", (buyer.address.clone(),).into_val(&w.env), &[auth])
}

/// Attempt `attempt` at charging period `cycle` of mandate `mandate_id`,
/// settleable for the next 100 ledgers.
fn attempt(
    w: &World,
    buyer: &Party,
    attempt: u64,
    mandate_id: u8,
    cycle: u32,
    amount: i128,
) -> RecurringChargeRequest {
    RecurringChargeRequest {
        owner: buyer.key.address(),
        charge_id: charge_id(attempt),
        mandate_id: id32(mandate_id),
        cycle,
        amount,
        last_ledger: w.env.ledger().sequence() + 100,
    }
}

fn contract_charge(w: &World, request: &RecurringChargeRequest) -> RecurringCharge {
    RecurringCharge {
        owner: Address::from_str(&w.env, request.owner.as_str()),
        charge_id: BytesN::from_array(&w.env, &request.charge_id),
        mandate_id: BytesN::from_array(&w.env, &request.mandate_id),
        cycle: request.cycle,
        amount: request.amount,
        last_ledger: request.last_ledger,
    }
}

fn charge_by(
    w: &World,
    signer: &Party,
    charges: &[RecurringChargeRequest],
) -> Result<std::vec::Vec<RecurringOutcome>, soroban_sdk::Error> {
    let auth = w.signed(signer, w.deployment().charge_recurring_batch_authorization(charges));
    let mut entries = soroban_sdk::Vec::new(&w.env);
    for c in charges {
        entries.push_back(contract_charge(w, c));
    }
    let outcomes: soroban_sdk::Vec<RecurringOutcome> =
        w.invoke("charge_recurring_batch", (entries,).into_val(&w.env), &[auth])?;
    Ok(outcomes.iter().collect())
}

pub(super) fn charge(
    w: &World,
    charges: &[RecurringChargeRequest],
) -> Result<std::vec::Vec<RecurringOutcome>, soroban_sdk::Error> {
    charge_by(w, &w.operator, charges)
}

fn one(w: &World, request: RecurringChargeRequest) -> RecurringOutcome {
    charge(w, &[request]).unwrap()[0]
}

pub(super) fn allowance(w: &World, buyer: &Party) -> i128 {
    token::Client::new(&w.env, &w.usdc).allowance(&buyer.address, &w.contract)
}

fn months_later(w: &World, months: u64) {
    w.env.ledger().set_timestamp(START + months * MONTH);
}

fn stored(w: &World, buyer: &Party) -> Mandate {
    w.client().get_mandate(&buyer.address).expect("a mandate")
}

// ---- the monthly flow ------------------------------------------------------

#[test]
fn test_a_mandate_lets_the_seller_charge_once_a_month_without_the_buyer_signing_again() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, 2 * USDC, 3)).unwrap();
    assert_eq!(allowance(&w, &buyer), 6 * USDC);

    assert_eq!(one(&w, attempt(&w, &buyer, 1, 1, 0, 2 * USDC)), RecurringOutcome::Charged);
    months_later(&w, 1);
    assert_eq!(one(&w, attempt(&w, &buyer, 2, 1, 1, 2 * USDC)), RecurringOutcome::Charged);

    // The wallet paid the treasury directly, as revenue: no prepaid account
    // exists and no liability was recorded.
    assert_eq!(
        (
            w.usdc_balance(&buyer),
            w.usdc_balance(&w.treasury),
            allowance(&w, &buyer),
            w.client().get_totals(),
            w.client().get_account(&buyer.address),
            stored(&w, &buyer).next_cycle,
        ),
        (6 * USDC, 4 * USDC, 2 * USDC, Totals { liabilities: 0, revenue: 4 * USDC }, None, 2)
    );
}

#[test]
fn test_each_period_is_charged_at_most_once_and_never_early_or_late() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, USDC, 3)).unwrap();
    assert_eq!(one(&w, attempt(&w, &buyer, 1, 1, 0, USDC)), RecurringOutcome::Charged);
    // Again for the same period, under a new attempt identifier.
    assert_eq!(one(&w, attempt(&w, &buyer, 2, 1, 0, USDC)), RecurringOutcome::AlreadyCharged);
    // The next period one second before it starts.
    w.env.ledger().set_timestamp(START + MONTH - 1);
    assert_eq!(one(&w, attempt(&w, &buyer, 3, 1, 1, USDC)), RecurringOutcome::NotDue);
    // The next period is skipped entirely; once the one after starts, the
    // skipped one is not charged late.
    months_later(&w, 2);
    assert_eq!(one(&w, attempt(&w, &buyer, 4, 1, 1, USDC)), RecurringOutcome::PeriodOver);
    assert_eq!(one(&w, attempt(&w, &buyer, 5, 1, 2, USDC)), RecurringOutcome::Charged);
    assert_eq!(w.usdc_balance(&buyer), 8 * USDC);
}

#[test]
fn test_the_same_attempt_settles_once_and_records_its_outcome() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, USDC, 3)).unwrap();
    let request = attempt(&w, &buyer, 1, 1, 0, USDC);
    assert_eq!(
        charge(&w, &[request.clone(), request.clone()]).unwrap(),
        [RecurringOutcome::Charged, RecurringOutcome::Duplicate]
    );
    // A refused attempt is recorded too, so resubmitting it changes nothing.
    let early = attempt(&w, &buyer, 2, 1, 1, USDC);
    assert_eq!(
        charge(&w, &[early.clone(), early]).unwrap(),
        [RecurringOutcome::NotDue, RecurringOutcome::Duplicate]
    );
    assert_eq!(w.usdc_balance(&buyer), 9 * USDC);
}

#[test]
fn test_no_charge_after_the_mandate_expires() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    let intent = mandate(&w, &buyer, 1, USDC, 12);
    authorize(&w, &buyer, &intent).unwrap();
    w.env.ledger().set_sequence_number(intent.live_until + 1);
    months_later(&w, 1);
    assert_eq!(one(&w, attempt(&w, &buyer, 1, 1, 1, USDC)), RecurringOutcome::MandateExpired);
    // The USDC contract itself no longer lets this contract spend anything.
    assert_eq!((allowance(&w, &buyer), w.usdc_balance(&buyer)), (0, 10 * USDC));
}

#[test]
fn test_no_charge_after_the_last_period() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, USDC, 2)).unwrap();
    months_later(&w, 2);
    assert_eq!(one(&w, attempt(&w, &buyer, 1, 1, 2, USDC)), RecurringOutcome::MandateExpired);
    // Naming an earlier period does not help once every period is over.
    assert_eq!(one(&w, attempt(&w, &buyer, 2, 1, 1, USDC)), RecurringOutcome::MandateExpired);
    assert_eq!(w.usdc_balance(&buyer), 10 * USDC);
}

// ---- refusals ----------------------------------------------------------------

#[test]
fn test_a_charge_is_bounded_by_the_mandate_the_largest_charge_and_the_seller_daily_limit() {
    let w = new_world();
    let buyer = w.party(100 * USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, MAX_CHARGE, 3)).unwrap();
    assert_eq!(
        one(&w, attempt(&w, &buyer, 1, 1, 0, MAX_CHARGE + 1)),
        RecurringOutcome::AboveMandate
    );
    // The admin lowers the largest charge below the mandate's amount.
    let lower = Limits { min_deposit: MIN_DEPOSIT, max_charge: MAX_CHARGE - 1 };
    admin_call(&w, &w.admin, "set_limits", (lower,).into_val(&w.env)).unwrap();
    assert_eq!(one(&w, attempt(&w, &buyer, 2, 1, 0, MAX_CHARGE)), RecurringOutcome::AboveLimit);
    set_daily(&w, USDC, 3 * USDC).unwrap();
    // The buyer's own daily limit counts prepaid charges only; the seller's
    // counts both kinds.
    funded(&w, 2, 10 * USDC);
    w.charge_batch(&[w.charge(2, 1, 2 * USDC / 2)]).unwrap();
    assert_eq!(
        one(&w, attempt(&w, &buyer, 3, 1, 0, 2 * USDC + 1)),
        RecurringOutcome::AboveDailyLimit
    );
    assert_eq!(one(&w, attempt(&w, &buyer, 4, 1, 0, 2 * USDC)), RecurringOutcome::Charged);
    assert_eq!(w.usdc_balance(&buyer), 98 * USDC);
}

#[test]
fn test_a_short_wallet_leaves_the_period_chargeable_once_topped_up() {
    let w = new_world();
    let buyer = w.party(USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, 2 * USDC, 3)).unwrap();
    assert_eq!(one(&w, attempt(&w, &buyer, 1, 1, 0, 2 * USDC)), RecurringOutcome::WalletShort);
    assert_eq!(
        (w.usdc_balance(&buyer), allowance(&w, &buyer), stored(&w, &buyer).next_cycle),
        (USDC, 6 * USDC, 0)
    );
    set_trustline(&w.env, &buyer.key, &w.asset, 5 * USDC as i64);
    assert_eq!(one(&w, attempt(&w, &buyer, 2, 1, 0, 2 * USDC)), RecurringOutcome::Charged);
    assert_eq!(w.usdc_balance(&buyer), 3 * USDC);
}

#[test]
fn test_a_buyer_lowering_the_allowance_in_the_usdc_contract_stops_charges() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    let intent = mandate(&w, &buyer, 1, 2 * USDC, 3);
    authorize(&w, &buyer, &intent).unwrap();
    // Straight in the USDC contract, from any wallet, without this contract.
    w.env.mock_all_auths();
    token::Client::new(&w.env, &w.usdc).approve(
        &buyer.address,
        &w.contract,
        &USDC,
        &intent.live_until,
    );
    w.env.set_auths(&[]);
    assert_eq!(one(&w, attempt(&w, &buyer, 1, 1, 0, 2 * USDC)), RecurringOutcome::AllowanceShort);
    assert_eq!((w.usdc_balance(&buyer), stored(&w, &buyer).next_cycle), (10 * USDC, 0));
}

#[test]
fn test_a_frozen_wallet_is_a_refused_transfer() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, USDC, 3)).unwrap();
    // The issuer revoked the buyer's authorization to hold USDC.
    let xdr::Asset::CreditAlphanum4(alpha) = &w.asset else { panic!("test asset is alphanum4") };
    let line = xdr::TrustLineAsset::CreditAlphanum4(alpha.clone());
    let key = Rc::new(xdr::LedgerKey::Trustline(xdr::LedgerKeyTrustLine {
        account_id: account_xdr_id(&buyer.key),
        asset: line.clone(),
    }));
    let frozen = Rc::new(xdr::LedgerEntry {
        last_modified_ledger_seq: 0,
        data: xdr::LedgerEntryData::Trustline(xdr::TrustLineEntry {
            account_id: account_xdr_id(&buyer.key),
            asset: line,
            balance: 10 * USDC as i64,
            limit: i64::MAX,
            flags: 0,
            ext: xdr::TrustLineEntryExt::V0,
        }),
        ext: xdr::LedgerEntryExt::V0,
    });
    w.env.host().add_ledger_entry(&key, &frozen, None).unwrap();
    assert_eq!(one(&w, attempt(&w, &buyer, 1, 1, 0, USDC)), RecurringOutcome::TransferRefused);
    assert_eq!(stored(&w, &buyer).next_cycle, 0);
}

#[test]
fn test_a_charge_past_its_last_ledger_is_expired_and_not_recorded() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, USDC, 3)).unwrap();
    let late = attempt(&w, &buyer, 1, 1, 0, USDC);
    w.env.ledger().set_sequence_number(late.last_ledger + 1);
    assert_eq!(one(&w, late.clone()), RecurringOutcome::Expired);
    let retried = RecurringChargeRequest { last_ledger: w.env.ledger().sequence() + 10, ..late };
    assert_eq!(one(&w, retried), RecurringOutcome::Charged);
}

// ---- replacing and revoking ------------------------------------------------

#[test]
fn test_a_new_mandate_replaces_the_old_one_and_its_allowance() {
    let w = new_world();
    let buyer = w.party(100 * USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, 5 * USDC, 12)).unwrap();
    assert_eq!(one(&w, attempt(&w, &buyer, 1, 1, 0, 5 * USDC)), RecurringOutcome::Charged);
    w.env.ledger().set_timestamp(START + MONTH / 2);
    authorize(&w, &buyer, &mandate(&w, &buyer, 2, USDC, 2)).unwrap();
    // What was left of the first mandate's 60 USDC is gone: the allowance is
    // the new mandate's alone, and charges for the old mandate are refused.
    assert_eq!(allowance(&w, &buyer), 2 * USDC);
    months_later(&w, 1);
    assert_eq!(one(&w, attempt(&w, &buyer, 2, 1, 1, 5 * USDC)), RecurringOutcome::NoMandate);
    // The new mandate's periods count from when it was authorized, half a
    // month ago: its first period is still running.
    assert_eq!(one(&w, attempt(&w, &buyer, 3, 2, 1, USDC)), RecurringOutcome::NotDue);
    assert_eq!(one(&w, attempt(&w, &buyer, 4, 2, 0, USDC)), RecurringOutcome::Charged);
    assert_eq!((w.usdc_balance(&buyer), allowance(&w, &buyer)), (94 * USDC, USDC));
}

#[test]
fn test_revocation_ends_the_mandate_and_zeroes_the_allowance_even_while_paused() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, USDC, 3)).unwrap();
    admin_call(&w, &w.admin, "pause", no_args(&w)).unwrap();
    revoke(&w, &buyer).unwrap();
    admin_call(&w, &w.admin, "unpause", no_args(&w)).unwrap();
    assert_eq!((allowance(&w, &buyer), w.client().get_mandate(&buyer.address)), (0, None));
    assert_eq!(one(&w, attempt(&w, &buyer, 1, 1, 0, USDC)), RecurringOutcome::NoMandate);
    assert_eq!(w.usdc_balance(&buyer), 10 * USDC);
    // Revoking with no mandate is harmless.
    revoke(&w, &buyer).unwrap();
}

#[test]
fn test_recurring_events_describe_each_step() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    let intent = mandate(&w, &buyer, 1, USDC, 3);
    authorize(&w, &buyer, &intent).unwrap();
    // Read before any other call: the host keeps only the last call's events.
    let events = w.env.events().all().filter_by_contract(&w.contract);
    let authorized =
        MandateAuthorized { owner: buyer.address.clone(), mandate: stored(&w, &buyer) };
    assert_eq!(events, [authorized.to_xdr(&w.env, &w.contract)]);
    let request = attempt(&w, &buyer, 7, 1, 0, USDC);
    one(&w, request.clone());
    let settled = RecurringCharges {
        settled: soroban_sdk::vec![
            &w.env,
            RecurringSettled(
                buyer.address.clone(),
                BytesN::from_array(&w.env, &request.charge_id),
                BytesN::from_array(&w.env, &intent.mandate_id),
                0,
                USDC,
                RecurringOutcome::Charged,
            )
        ],
    };
    assert_eq!(
        w.env.events().all().filter_by_contract(&w.contract),
        [settled.to_xdr(&w.env, &w.contract)]
    );
    revoke(&w, &buyer).unwrap();
    let revoked = MandateRevoked {
        owner: buyer.address.clone(),
        mandate_id: Some(BytesN::from_array(&w.env, &intent.mandate_id)),
    };
    assert_eq!(
        w.env.events().all().filter_by_contract(&w.contract),
        [revoked.to_xdr(&w.env, &w.contract)]
    );
}

// ---- storage layout the gateway reads -------------------------------------

#[test]
fn test_mandate_and_attempt_keys_match_the_contract_layout() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, USDC, 3)).unwrap();
    one(&w, attempt(&w, &buyer, 1, 1, 0, USDC));
    let snapshot = w.env.to_ledger_snapshot();
    let present = |key: &xdr::LedgerKey| snapshot.ledger_entries.iter().any(|(k, _)| **k == *key);
    let deployment = w.deployment();
    let owner = buyer.key.address();
    assert!(present(&deployment.mandate_key(&owner)), "mandate at the derived key");
    assert!(
        present(&deployment.recurring_record_key(&owner, &charge_id(1))),
        "attempt record at the derived key"
    );
    // A prepaid charge record with the same identifier is a different entry.
    assert!(!present(&deployment.charge_record_key(&owner, &charge_id(1))));
    // The USDC contract's allowance, as the mandate left it after one charge.
    let allowance = snapshot
        .ledger_entries
        .iter()
        .find(|(k, _)| **k == deployment.allowance_key(&owner))
        .and_then(|(_, (entry, _))| allowance_of(&entry.data));
    assert_eq!(allowance, Some((2 * USDC, w.env.ledger().sequence() + LIFETIME)));
}

/// The gateway reads mandates, attempt records, outcomes and events with its
/// own decoders; they are pinned here against what the host records.
#[test]
fn test_gateway_decodes_mandates_records_outcomes_and_events() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    let intent = mandate(&w, &buyer, 1, USDC, 3);
    let owner = ChainAddress::Account(buyer.key.address());
    let decoded = || {
        w.env
            .events()
            .all()
            .filter_by_contract(&w.contract)
            .events()
            .iter()
            .map(|event| {
                let xdr::ContractEventBody::V0(body) = &event.body;
                ledger_event(&body.topics, &body.data).expect("a decodable event")
            })
            .collect::<std::vec::Vec<_>>()
    };
    authorize(&w, &buyer, &intent).unwrap();
    let terms = MandateTerms {
        mandate_id: intent.mandate_id,
        amount: USDC,
        period_secs: MONTH,
        start: START,
        cycles: 3,
        live_until: intent.live_until,
        next_cycle: 0,
    };
    assert_eq!(
        decoded(),
        [LedgerEvent::MandateAuthorized { owner: owner.clone(), mandate: terms.clone() }]
    );

    let charged = attempt(&w, &buyer, 1, 1, 0, USDC);
    let early = attempt(&w, &buyer, 2, 1, 1, USDC);
    let auth = w.signed(
        &w.operator,
        w.deployment().charge_recurring_batch_authorization(&[charged.clone(), early.clone()]),
    );
    let mut entries = soroban_sdk::Vec::new(&w.env);
    entries.push_back(contract_charge(&w, &charged));
    entries.push_back(contract_charge(&w, &early));
    let returned: Val =
        w.invoke("charge_recurring_batch", (entries,).into_val(&w.env), &[auth]).unwrap();
    let returned: xdr::ScVal = returned.try_into_val(&w.env).unwrap();
    use fermah_pay_stellar_chain::prepaid::RecurringOutcome as Decoded;
    assert_eq!(recurring_outcomes(&returned), Some(std::vec![Decoded::Charged, Decoded::NotDue]));
    let entry = |request: &RecurringChargeRequest, cycle, outcome| RecurringEntry {
        owner: owner.clone(),
        charge_id: request.charge_id,
        mandate_id: intent.mandate_id,
        cycle,
        amount: USDC,
        outcome,
    };
    assert_eq!(
        decoded(),
        [LedgerEvent::Recurring(std::vec![
            entry(&charged, 0, Decoded::Charged),
            entry(&early, 1, Decoded::NotDue),
        ])]
    );

    let snapshot = w.env.to_ledger_snapshot();
    let read = |key: &xdr::LedgerKey| {
        snapshot.ledger_entries.iter().find(|(k, _)| **k == *key).map(|(_, (e, _))| e.data.clone())
    };
    let deployment = w.deployment();
    let address = buyer.key.address();
    assert_eq!(
        read(&deployment.mandate_key(&address)).as_ref().and_then(stored_mandate),
        Some(MandateTerms { next_cycle: 1, ..terms })
    );
    let record = |id| {
        read(&deployment.recurring_record_key(&address, &charge_id(id)))
            .as_ref()
            .and_then(recurring_record)
    };
    assert_eq!((record(1), record(2)), (Some(Decoded::Charged), Some(Decoded::NotDue)));

    revoke(&w, &buyer).unwrap();
    assert_eq!(
        decoded(),
        [LedgerEvent::MandateRevoked { owner: owner.clone(), mandate_id: Some(intent.mandate_id) }]
    );
    revoke(&w, &buyer).unwrap();
    assert_eq!(decoded(), [LedgerEvent::MandateRevoked { owner, mandate_id: None }]);
}

// ---- authorization ---------------------------------------------------------

#[test]
fn test_a_mandate_needs_the_buyer_to_sign_the_exact_approval() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    let intent = mandate(&w, &buyer, 1, USDC, 3);
    // The buyer signed a mandate of one period, but the call asks for three:
    // the approval the contract makes is not the one the buyer signed.
    let signed_for = MandateIntent { cycles: 1, ..intent.clone() };
    let auth = w.signed(&buyer, w.deployment().authorize_recurring_authorization(&signed_for));
    let result: Result<(), _> =
        w.invoke("authorize_recurring", mandate_args(&w, &intent, &buyer.address), &[auth]);
    assert_eq!(result, Err(auth_failure()));
    // Someone else cannot authorize a mandate on the buyer's wallet.
    let other = w.party(0);
    let auth = w.signed(&other, w.deployment().authorize_recurring_authorization(&intent));
    let result: Result<(), _> =
        w.invoke("authorize_recurring", mandate_args(&w, &intent, &buyer.address), &[auth]);
    assert_eq!(result, Err(auth_failure()));
    assert_eq!((w.client().get_mandate(&buyer.address), allowance(&w, &buyer)), (None, 0));
}

#[test]
fn test_only_the_operator_settles_recurring_charges() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, USDC, 3)).unwrap();
    let request = attempt(&w, &buyer, 1, 1, 0, USDC);
    for signer in [&w.seller, &buyer, &w.admin] {
        assert_eq!(charge_by(&w, signer, std::slice::from_ref(&request)), Err(auth_failure()));
    }
    assert_eq!(w.usdc_balance(&buyer), 10 * USDC);
}

#[test]
fn test_unusable_mandates_and_batches_are_rejected() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    let valid = mandate(&w, &buyer, 1, USDC, 3);
    let invalid = [
        (MandateIntent { amount: 0, ..valid.clone() }, Error::InvalidAmount),
        (MandateIntent { period_secs: 0, ..valid.clone() }, Error::InvalidMandate),
        (MandateIntent { cycles: 0, ..valid.clone() }, Error::InvalidMandate),
        (
            MandateIntent { live_until: w.env.ledger().sequence() - 1, ..valid.clone() },
            Error::InvalidMandate,
        ),
    ];
    for (intent, error) in invalid {
        assert_eq!(authorize(&w, &buyer, &intent), Err(contract_error(error)), "{intent:?}");
    }
    admin_call(&w, &w.admin, "pause", no_args(&w)).unwrap();
    assert_eq!(authorize(&w, &buyer, &valid), Err(contract_error(Error::Paused)));
    admin_call(&w, &w.admin, "unpause", no_args(&w)).unwrap();
    authorize(&w, &buyer, &valid).unwrap();

    assert_eq!(charge(&w, &[]), Err(contract_error(Error::EmptyBatch)));
    let too_many: std::vec::Vec<_> =
        (0..=u64::from(MAX_RECURRING_BATCH)).map(|i| attempt(&w, &buyer, i, 1, 0, USDC)).collect();
    assert_eq!(charge(&w, &too_many), Err(contract_error(Error::BatchTooLarge)));
    let zero = attempt(&w, &buyer, 1, 1, 0, 0);
    assert_eq!(charge(&w, &[zero]), Err(contract_error(Error::InvalidAmount)));
    admin_call(&w, &w.admin, "pause", no_args(&w)).unwrap();
    assert_eq!(
        charge(&w, &[attempt(&w, &buyer, 2, 1, 0, USDC)]),
        Err(contract_error(Error::Paused))
    );
}

// ---- resource ceiling: MAX_RECURRING_BATCH buyers in one invocation --------

/// Charges period 0 of `n` distinct buyers' mandates in one invocation of the
/// Wasm contract and returns the resources it used.
fn measure_recurring_batch(n: u32) -> (std::vec::Vec<RecurringOutcome>, Measured) {
    let code = wasm_under_test();
    let w = world_with(Some(&code));
    w.env.ledger().set_timestamp(START);
    let mut charges = std::vec::Vec::new();
    for i in 0..n {
        let buyer = w.party(10 * USDC);
        authorize(&w, &buyer, &mandate(&w, &buyer, 1, USDC, 12)).unwrap();
        charges.push(attempt(&w, &buyer, u64::from(i), 1, 0, USDC));
    }
    // The test host records every authorization tree for inspection under
    // its own budget, which a large batch of mandates' arguments exhausts.
    // The network's limits are checked against the measurement instead.
    w.env.cost_estimate().budget().reset_unlimited();
    let outcomes = charge(&w, &charges).unwrap();
    let r = w.env.cost_estimate().resources();
    let measured = Measured {
        instructions: r.instructions,
        mem_bytes: r.mem_bytes,
        write_entries: r.write_entries,
        write_bytes: r.write_bytes,
        disk_read_entries: r.disk_read_entries,
        memory_read_entries: r.memory_read_entries,
        events_bytes: r.contract_events_size_bytes,
        fee_stroops: w.env.cost_estimate().fee().total,
        outcomes: std::vec::Vec::new(),
    };
    std::println!(
        "recurring entries={n} instructions={} mem_bytes={} write_entries={} write_bytes={} footprint={} events_bytes={} estimated_fee_stroops={}",
        measured.instructions,
        measured.mem_bytes,
        measured.write_entries,
        measured.write_bytes,
        measured.memory_read_entries + measured.disk_read_entries,
        measured.events_bytes,
        measured.fee_stroops
    );
    (outcomes, measured)
}

#[test]
#[ignore = "needs the contract Wasm: set PREPAID_WASM (see `just contract-resources`)"]
fn test_full_recurring_batch_fits_one_transaction() {
    let n = std::env::var("RECURRING_BATCH_PROBE")
        .map_or(MAX_RECURRING_BATCH, |probe| probe.parse().unwrap());
    let (outcomes, measured) = measure_recurring_batch(n);
    assert_eq!(outcomes, std::vec![RecurringOutcome::Charged; n as usize]);
    assert_within_limits(&measured);
}

// ---- mandate terms the contract refuses or keeps ---------------------------

#[test]
fn test_a_mandate_above_the_largest_charge_is_refused() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    // Never chargeable in full: every period would be refused as above the
    // limit while the buyer's whole approval stayed open.
    let above = mandate(&w, &buyer, 1, MAX_CHARGE + 1, 3);
    assert_eq!(authorize(&w, &buyer, &above), Err(contract_error(Error::InvalidMandate)));
    assert_eq!(allowance(&w, &buyer), 0);
    authorize(&w, &buyer, &mandate(&w, &buyer, 1, MAX_CHARGE, 3)).unwrap();
}

#[test]
fn test_the_current_mandate_cannot_be_authorized_again_to_restart_its_periods() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    let first = mandate(&w, &buyer, 1, USDC, 3);
    authorize(&w, &buyer, &first).unwrap();
    assert_eq!(one(&w, attempt(&w, &buyer, 1, 1, 0, USDC)), RecurringOutcome::Charged);
    // The same identifier again would start its periods over.
    assert_eq!(authorize(&w, &buyer, &first), Err(contract_error(Error::InvalidMandate)));
    assert_eq!(one(&w, attempt(&w, &buyer, 2, 1, 0, USDC)), RecurringOutcome::AlreadyCharged);
    // A new identifier replaces it, as a new mandate.
    authorize(&w, &buyer, &mandate(&w, &buyer, 2, USDC, 3)).unwrap();
    assert_eq!(one(&w, attempt(&w, &buyer, 3, 2, 0, USDC)), RecurringOutcome::Charged);
}

#[test]
fn test_a_mandate_lives_until_its_last_ledger_without_being_charged() {
    let w = new_world();
    let buyer = w.party(10 * USDC);
    // Longer than an entry written by any call lives: a monthly mandate is
    // charged about once per that span.
    let lasting = MandateIntent {
        live_until: w.env.ledger().sequence() + 1_000_000,
        ..mandate(&w, &buyer, 1, USDC, 6)
    };
    authorize(&w, &buyer, &lasting).unwrap();
    let left = w.env.as_contract(&w.contract, || {
        w.env.storage().persistent().get_ttl(&Key::Mandate(buyer.address.clone()))
    });
    assert!(
        left >= lasting.live_until - w.env.ledger().sequence(),
        "the mandate lives {left} ledgers"
    );
}
