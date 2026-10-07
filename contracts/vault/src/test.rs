//! Expected values are derived from the rules in the crate documentation,
//! not from the contract's code. Authorizations are mocked, and each test of
//! who must authorize a call reads the authorizations the host required.

extern crate std;

use std::vec::Vec as StdVec;

use soroban_sdk::testutils::storage::{Persistent as _, Temporary as _};
use soroban_sdk::testutils::{Address as _, Deployer as _, Ledger as _};
use soroban_sdk::{Address, BytesN, Env, String as SorobanString, token, vec};

use super::*;

mod chain;
mod properties;

const USDC: i128 = 10_000_000;
const MAX_CHARGE: i128 = 100 * USDC;

struct World {
    env: Env,
    vault: Address,
    usdc: Address,
    admin: Address,
    operator: Address,
    seller: Address,
}

fn world() -> World {
    world_with(None)
}

/// A world whose vault runs as `wasm` in the Soroban VM, which resource
/// measurements need.
fn world_with(wasm: Option<&[u8]>) -> World {
    let env = Env::new_with_config(soroban_sdk::testutils::EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    env.ledger().set_sequence_number(10_000);
    env.mock_all_auths();
    let issuer = Address::generate(&env);
    let usdc = env.register_stellar_asset_contract_v2(issuer).address();
    let (admin, operator, seller) =
        (Address::generate(&env), Address::generate(&env), Address::generate(&env));
    let args = (
        admin.clone(),
        operator.clone(),
        seller.clone(),
        usdc.clone(),
        Limits { min_deposit: USDC / 10, max_charge: MAX_CHARGE },
    );
    let vault = match wasm {
        None => env.register(PrepaidVault, args),
        Some(code) => env.register(code, args),
    };
    World { env, vault, usdc, admin, operator, seller }
}

impl World {
    fn client(&self) -> PrepaidVaultClient<'_> {
        PrepaidVaultClient::new(&self.env, &self.vault)
    }

    fn token(&self) -> token::Client<'_> {
        token::Client::new(&self.env, &self.usdc)
    }

    /// A buyer whose wallet holds `usdc`.
    fn buyer(&self, usdc: i128) -> Address {
        let buyer = Address::generate(&self.env);
        if usdc > 0 {
            token::StellarAssetClient::new(&self.env, &self.usdc).mint(&buyer, &usdc);
        }
        buyer
    }

    /// A buyer who deposited `amount` with a daily limit of `cap`.
    fn funded(&self, amount: i128, cap: i128) -> Address {
        let buyer = self.buyer(amount);
        self.client().deposit(&buyer, &amount, &self.id(1), &Some(cap));
        buyer
    }

    fn id(&self, n: u8) -> BytesN<32> {
        BytesN::from_array(&self.env, &[n; 32])
    }

    fn now(&self) -> u32 {
        self.env.ledger().sequence()
    }

    fn advance(&self, ledgers: u32) {
        self.env.ledger().set_sequence_number(self.now() + ledgers);
    }

    fn next_day(&self) {
        let now = self.env.ledger().timestamp();
        self.env.ledger().set_timestamp(now + 86_400);
    }

    /// A charge admitted today, settleable until `last_ledger`.
    fn charge_until(&self, owner: &Address, n: u8, amount: i128, last_ledger: u32) -> Charge {
        Charge(owner.clone(), self.id(n), amount, last_ledger, self.today())
    }

    fn today(&self) -> u64 {
        self.env.ledger().timestamp() / 86_400
    }

    /// A charge with the longest window, as if admitted now.
    fn charge(&self, owner: &Address, n: u8, amount: i128) -> Charge {
        self.charge_until(owner, n, amount, self.now() + MAX_CHARGE_WINDOW)
    }

    fn settle(&self, charges: &[Charge]) -> StdVec<Outcome> {
        let mut batch = soroban_sdk::Vec::new(&self.env);
        for charge in charges {
            batch.push_back(charge.clone());
        }
        self.client().charge_batch(&batch).iter().collect()
    }

    /// Who the host required to authorize the last call.
    fn signers(&self) -> StdVec<Address> {
        self.env.auths().into_iter().map(|(address, _)| address).collect()
    }

    fn held(&self) -> i128 {
        self.token().balance(&self.vault)
    }

    fn assert_solvent(&self) {
        let totals = self.client().get_totals();
        assert_eq!(self.held(), totals.liabilities + totals.revenue);
    }
}

fn exit_unlock(w: &World, buyer: &Address) -> u32 {
    match w.client().get_account(buyer).unwrap().exit {
        Exit::Requested(request) => request.unlock_at,
        Exit::None => panic!("no exit requested"),
    }
}

/// The contract error a call was refused with.
fn refusal<T: core::fmt::Debug>(
    result: Result<T, Result<soroban_sdk::Error, soroban_sdk::InvokeError>>,
) -> Error {
    match result {
        Err(Ok(error)) => {
            Error::try_from(error).unwrap_or_else(|_| panic!("not a contract error: {error:?}"))
        }
        other => panic!("not refused: {other:?}"),
    }
}

// ---- custody -----------------------------------------------------------------

#[test]
fn test_deposit_moves_usdc_into_the_vault_and_needs_only_the_buyer() {
    let w = world();
    let buyer = w.buyer(10 * USDC);
    w.client().deposit(&buyer, &(10 * USDC), &w.id(1), &None);
    assert_eq!(w.signers(), core::slice::from_ref(&buyer));
    assert_eq!((w.token().balance(&buyer), w.held()), (0, 10 * USDC));
    assert_eq!(w.client().get_balance(&buyer), 10 * USDC);
    w.assert_solvent();
}

#[test]
fn test_cooperative_withdrawal_needs_the_buyer_and_the_operator() {
    let w = world();
    let buyer = w.funded(10 * USDC, USDC);
    w.client().withdraw(&buyer, &(4 * USDC), &buyer, &w.id(1));
    assert_eq!(w.signers(), [buyer.clone(), w.operator.clone()]);
    assert_eq!((w.token().balance(&buyer), w.client().get_balance(&buyer)), (4 * USDC, 6 * USDC));
    w.assert_solvent();
}

#[test]
fn test_revenue_is_paid_out_by_the_seller_alone() {
    let w = world();
    let buyer = w.funded(10 * USDC, 5 * USDC);
    w.settle(&[w.charge(&buyer, 1, 3 * USDC)]);
    let payee = Address::generate(&w.env);
    w.client().withdraw_revenue(&payee, &(3 * USDC), &w.id(1));
    assert_eq!(w.signers(), core::slice::from_ref(&w.seller));
    assert_eq!(w.token().balance(&payee), 3 * USDC);
    w.assert_solvent();
}

#[test]
fn test_payout_to_the_vault_itself_is_refused() {
    let w = world();
    let buyer = w.funded(10 * USDC, USDC);
    assert_eq!(
        refusal(w.client().try_withdraw(&buyer, &USDC, &w.vault, &w.id(1))),
        Error::InvalidDestination
    );
    assert_eq!(
        refusal(w.client().try_request_exit(&buyer, &USDC, &w.vault)),
        Error::InvalidDestination
    );
}

// ---- the buyer's spending limit -----------------------------------------------

#[test]
fn test_a_buyer_without_a_limit_cannot_be_charged() {
    let w = world();
    let buyer = w.buyer(10 * USDC);
    w.client().deposit(&buyer, &(10 * USDC), &w.id(1), &None);
    assert_eq!(w.settle(&[w.charge(&buyer, 1, 1)]), [Outcome::AboveCap]);
    assert_eq!(w.client().get_balance(&buyer), 10 * USDC);
}

#[test]
fn test_charges_stay_within_the_limit_each_day() {
    let w = world();
    let buyer = w.funded(20 * USDC, 5 * USDC);
    assert_eq!(
        w.settle(&[
            w.charge(&buyer, 1, 3 * USDC),
            w.charge(&buyer, 2, 2 * USDC),
            w.charge(&buyer, 3, 1)
        ]),
        [Outcome::Charged, Outcome::Charged, Outcome::AboveCap]
    );
    w.next_day();
    assert_eq!(w.settle(&[w.charge(&buyer, 4, 5 * USDC)]), [Outcome::Charged]);
    assert_eq!(w.client().get_balance(&buyer), 10 * USDC);
}

#[test]
fn test_raising_the_limit_applies_at_once() {
    let w = world();
    let buyer = w.funded(20 * USDC, USDC);
    w.client().set_cap(&buyer, &(10 * USDC));
    assert_eq!(w.signers(), core::slice::from_ref(&buyer));
    assert_eq!(w.settle(&[w.charge(&buyer, 1, 10 * USDC)]), [Outcome::Charged]);
}

/// The seller guarantee for a lower limit: a charge admitted before the
/// buyer lowers it settles under the old limit until its last ledger.
#[test]
fn test_a_lower_limit_waits_out_every_charge_admitted_before_it() {
    let w = world();
    let buyer = w.funded(20 * USDC, 10 * USDC);
    let admitted = w.charge(&buyer, 1, 10 * USDC);
    let lowered_at = w.now();
    w.client().set_cap(&buyer, &0);
    assert_eq!(w.client().get_cap(&buyer), 10 * USDC);

    w.env.ledger().set_sequence_number(admitted.3);
    assert_eq!(w.settle(&[admitted]), [Outcome::Charged]);

    w.env.ledger().set_sequence_number(lowered_at + NOTICE_LEDGERS);
    w.next_day();
    assert_eq!(w.client().get_cap(&buyer), 0);
    assert_eq!(w.settle(&[w.charge(&buyer, 2, 1)]), [Outcome::AboveCap]);
}

/// A raise drops a pending lower limit, so charges admitted against the
/// raise are not refused when the older request would have taken effect.
#[test]
fn test_a_raise_drops_a_pending_lower_limit() {
    let w = world();
    let buyer = w.funded(20 * USDC, 5 * USDC);
    let lowered_at = w.now();
    w.client().set_cap(&buyer, &0);
    w.advance(NOTICE_LEDGERS - 1);
    w.client().set_cap(&buyer, &(10 * USDC));
    let admitted = w.charge(&buyer, 1, 10 * USDC);
    w.env.ledger().set_sequence_number(lowered_at + NOTICE_LEDGERS);
    assert_eq!(w.client().get_cap(&buyer), 10 * USDC);
    assert_eq!(w.settle(&[admitted]), [Outcome::Charged]);
}

#[test]
fn test_a_second_lower_limit_restarts_the_notice() {
    let w = world();
    let buyer = w.funded(20 * USDC, 5 * USDC);
    let first = w.now();
    w.client().set_cap(&buyer, &(3 * USDC));
    w.advance(NOTICE_LEDGERS - 1);
    w.client().set_cap(&buyer, &USDC);
    w.env.ledger().set_sequence_number(first + NOTICE_LEDGERS);
    assert_eq!(w.client().get_cap(&buyer), 5 * USDC);
    w.env.ledger().set_sequence_number(first + 2 * NOTICE_LEDGERS - 1);
    assert_eq!(w.client().get_cap(&buyer), USDC);
}

#[test]
fn test_a_lower_limit_given_with_a_deposit_also_waits() {
    let w = world();
    let buyer = w.funded(10 * USDC, 5 * USDC);
    token::StellarAssetClient::new(&w.env, &w.usdc).mint(&buyer, &USDC);
    w.client().deposit(&buyer, &USDC, &w.id(2), &Some(0));
    assert_eq!(w.client().get_cap(&buyer), 5 * USDC);
    w.advance(NOTICE_LEDGERS);
    assert_eq!(w.client().get_cap(&buyer), 0);
}

#[test]
fn test_a_negative_limit_is_refused() {
    let w = world();
    let buyer = w.funded(10 * USDC, USDC);
    assert_eq!(refusal(w.client().try_set_cap(&buyer, &-1)), Error::InvalidCap);
}

// ---- exits ---------------------------------------------------------------------

#[test]
fn test_exit_pays_after_the_notice_and_needs_no_signature() {
    let w = world();
    let buyer = w.funded(10 * USDC, USDC);
    let wallet = Address::generate(&w.env);
    w.client().request_exit(&buyer, &(6 * USDC), &wallet);
    assert_eq!(w.signers(), core::slice::from_ref(&buyer));
    w.advance(NOTICE_LEDGERS - 1);
    assert_eq!(refusal(w.client().try_exit(&buyer)), Error::ExitLocked);
    w.advance(1);
    w.client().exit(&buyer);
    assert!(w.signers().is_empty());
    assert_eq!((w.token().balance(&wallet), w.client().get_balance(&buyer)), (6 * USDC, 4 * USDC));
    assert_eq!(refusal(w.client().try_exit(&buyer)), Error::NoExit);
    w.assert_solvent();
}

/// The seller guarantee for an exit: a charge admitted before the request
/// can settle until its last ledger, which comes before the exit unlocks.
#[test]
fn test_an_exit_waits_out_every_charge_admitted_before_it() {
    let w = world();
    let buyer = w.funded(10 * USDC, 10 * USDC);
    let admitted = w.charge(&buyer, 1, 10 * USDC);
    w.client().request_exit(&buyer, &(10 * USDC), &buyer);
    let unlock_at = exit_unlock(&w, &buyer);
    assert!(admitted.3 < unlock_at);
    w.env.ledger().set_sequence_number(admitted.3);
    assert_eq!(refusal(w.client().try_exit(&buyer)), Error::ExitLocked);
    assert_eq!(w.settle(&[admitted]), [Outcome::Charged]);
    w.env.ledger().set_sequence_number(unlock_at);
    // The charge took everything: nothing is left to pay, and the request
    // stays for a later deposit.
    assert_eq!(refusal(w.client().try_exit(&buyer)), Error::NothingToExit);
    assert_eq!(exit_unlock(&w, &buyer), unlock_at);
    assert_eq!(w.client().get_totals(), Totals { liabilities: 0, revenue: 10 * USDC });
    w.assert_solvent();
}

#[test]
fn test_a_new_exit_request_restarts_the_notice() {
    let w = world();
    let buyer = w.funded(10 * USDC, USDC);
    let first = w.now();
    w.client().request_exit(&buyer, &USDC, &buyer);
    w.advance(NOTICE_LEDGERS - 1);
    w.client().request_exit(&buyer, &(10 * USDC), &buyer);
    w.env.ledger().set_sequence_number(first + NOTICE_LEDGERS);
    assert_eq!(refusal(w.client().try_exit(&buyer)), Error::ExitLocked);
}

#[test]
fn test_an_exit_pays_what_is_left_when_less_than_requested() {
    let w = world();
    let buyer = w.funded(10 * USDC, 10 * USDC);
    w.client().request_exit(&buyer, &(10 * USDC), &buyer);
    w.settle(&[w.charge(&buyer, 1, 7 * USDC)]);
    w.advance(NOTICE_LEDGERS);
    w.client().exit(&buyer);
    assert_eq!((w.token().balance(&buyer), w.client().get_balance(&buyer)), (3 * USDC, 0));
    w.assert_solvent();
}

#[test]
fn test_pause_stops_deposits_and_charges_but_never_a_way_out() {
    let w = world();
    let buyer = w.funded(10 * USDC, 5 * USDC);
    w.settle(&[w.charge(&buyer, 1, 2 * USDC)]);
    w.client().request_exit(&buyer, &USDC, &buyer);
    w.client().pause();

    let other = w.buyer(USDC);
    assert_eq!(refusal(w.client().try_deposit(&other, &USDC, &w.id(1), &None)), Error::Paused);
    let mut batch = soroban_sdk::Vec::new(&w.env);
    batch.push_back(w.charge(&buyer, 2, USDC));
    assert_eq!(refusal(w.client().try_charge_batch(&batch)), Error::Paused);

    w.client().set_cap(&buyer, &0);
    w.client().withdraw(&buyer, &USDC, &buyer, &w.id(1));
    w.client().request_exit(&buyer, &USDC, &buyer);
    w.advance(NOTICE_LEDGERS);
    w.client().exit(&buyer);
    w.client().withdraw_revenue(&w.seller, &(2 * USDC), &w.id(1));
    w.client().revoke_recurring(&buyer);
    assert_eq!(w.client().get_balance(&buyer), 6 * USDC);
    w.assert_solvent();
}

#[test]
fn test_only_the_buyer_can_request_an_exit() {
    let w = world();
    let buyer = w.funded(10 * USDC, USDC);
    w.client().request_exit(&buyer, &USDC, &buyer);
    assert_eq!(w.signers(), [buyer]);
}

// ---- roles and the upgrade timelock ---------------------------------------------

#[test]
fn test_the_seller_role_moves_only_with_the_seller() {
    let w = world();
    let successor = Address::generate(&w.env);
    w.client().set_seller(&successor);
    assert_eq!(w.signers(), [w.seller.clone(), successor.clone()]);
    assert_eq!(w.client().get_config().seller, successor);
}

#[test]
fn test_the_operator_role_moves_with_the_admin() {
    let w = world();
    let successor = Address::generate(&w.env);
    w.client().set_operator(&successor);
    assert_eq!(w.signers(), [w.admin.clone(), successor]);
}

#[test]
fn test_roles_stay_distinct() {
    let w = world();
    assert_eq!(refusal(w.client().try_set_operator(&w.seller)), Error::DuplicateRole);
    assert_eq!(refusal(w.client().try_set_seller(&w.admin)), Error::DuplicateRole);
}

#[test]
fn test_an_upgrade_waits_its_delay() {
    let w = world();
    let hash = BytesN::from_array(&w.env, &[7; 32]);
    assert_eq!(refusal(w.client().try_upgrade()), Error::NoUpgrade);
    let proposed_at = w.now();
    w.client().propose_upgrade(&hash);
    assert_eq!(w.signers(), core::slice::from_ref(&w.admin));
    w.advance(UPGRADE_DELAY_LEDGERS - 1);
    assert_eq!(refusal(w.client().try_upgrade()), Error::UpgradeLocked);

    // Proposing again restarts the delay.
    w.client().propose_upgrade(&hash);
    w.env.ledger().set_sequence_number(proposed_at + UPGRADE_DELAY_LEDGERS);
    assert_eq!(refusal(w.client().try_upgrade()), Error::UpgradeLocked);

    w.client().cancel_upgrade();
    assert_eq!(w.client().get_pending_upgrade(), None);
    assert_eq!(refusal(w.client().try_upgrade()), Error::NoUpgrade);
}

/// A buyer who requests an exit within the margin after a proposal can exit
/// before the new code could be installed.
#[test]
fn test_an_exit_requested_within_the_margin_unlocks_before_an_upgrade() {
    let w = world();
    let buyer = w.funded(10 * USDC, USDC);
    w.client().propose_upgrade(&BytesN::from_array(&w.env, &[7; 32]));
    let effective_at = w.client().get_pending_upgrade().unwrap().effective_at;
    w.advance(EXIT_MARGIN_LEDGERS);
    w.client().request_exit(&buyer, &(10 * USDC), &buyer);
    let unlock_at = exit_unlock(&w, &buyer);
    assert!(unlock_at < effective_at, "{unlock_at} >= {effective_at}");
    w.env.ledger().set_sequence_number(unlock_at);
    w.client().exit(&buyer);
    assert_eq!(w.token().balance(&buyer), 10 * USDC);
}

/// Past the delay the call installs the proposed hash: with no such Wasm
/// uploaded the host refuses it, which shows the timelock let it through.
#[test]
fn test_past_its_delay_an_upgrade_installs_the_proposed_hash() {
    let w = world();
    w.client().propose_upgrade(&BytesN::from_array(&w.env, &[7; 32]));
    w.advance(UPGRADE_DELAY_LEDGERS);
    match w.client().try_upgrade() {
        Err(Ok(error)) => {
            assert!(Error::try_from(error).is_err(), "refused by the contract: {error:?}")
        }
        Err(Err(_)) => {}
        Ok(_) => panic!("installed a Wasm that was never uploaded"),
    }
}

fn wasm_under_test() -> StdVec<u8> {
    let path = std::env::var("VAULT_WASM")
        .expect("set VAULT_WASM to the contract built by `stellar contract build`");
    std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
}

#[test]
#[ignore = "needs the contract Wasm: set VAULT_WASM (see `just contract-resources`)"]
fn test_full_upgrade_after_the_delay_keeps_balances() {
    let code = wasm_under_test();
    let w = world_with(Some(&code));
    let buyer = w.funded(10 * USDC, USDC);
    let hash = w.env.deployer().upload_contract_wasm(code.as_slice());
    w.client().propose_upgrade(&hash);
    w.advance(UPGRADE_DELAY_LEDGERS);
    w.client().upgrade();
    assert_eq!(w.client().get_pending_upgrade(), None);
    assert_eq!(w.client().get_balance(&buyer), 10 * USDC);
}

// ---- launch limits ------------------------------------------------------------

#[test]
fn test_launch_limits_refuse_deposits_past_them() {
    let w = world();
    w.client()
        .set_launch_limits(&Some(LaunchLimits { max_balance: 5 * USDC, max_total: 8 * USDC }));
    let first = w.buyer(10 * USDC);
    w.client().deposit(&first, &(5 * USDC), &w.id(1), &None);
    assert_eq!(
        refusal(w.client().try_deposit(&first, &USDC, &w.id(2), &None)),
        Error::AboveLaunchLimit
    );
    let second = w.buyer(10 * USDC);
    assert_eq!(
        refusal(w.client().try_deposit(&second, &(4 * USDC), &w.id(1), &None)),
        Error::AboveLaunchLimit
    );
    w.client().deposit(&second, &(3 * USDC), &w.id(1), &None);
    w.client().set_launch_limits(&None);
    w.client().deposit(&first, &USDC, &w.id(2), &None);
}

// ---- recurring charges and storage lifetime ---------------------------------------

#[test]
fn test_a_recurring_charge_lands_in_the_vault_as_revenue() {
    let w = world();
    let buyer = w.buyer(10 * USDC);
    let live_until = w.now() + 100_000;
    w.client().authorize_recurring(&buyer, &w.id(1), &USDC, &86_400, &3, &live_until);
    let charge = RecurringCharge {
        owner: buyer.clone(),
        charge_id: w.id(1),
        mandate_id: w.id(1),
        cycle: 0,
        amount: USDC,
        last_ledger: w.now() + 100,
    };
    let outcomes = w.client().charge_recurring_batch(&vec![&w.env, charge]);
    assert_eq!(outcomes, vec![&w.env, RecurringOutcome::Charged]);
    assert_eq!((w.held(), w.client().get_totals().revenue), (USDC, USDC));
    w.assert_solvent();
}

/// A deployment left idle until the vault's instance, the USDC contract's
/// instance and the buyer's account have all expired: the buyer still exits,
/// and a replayed deposit is still refused.
#[test]
fn test_exit_works_after_every_entry_has_expired() {
    let w = world();
    let buyer = w.funded(10 * USDC, USDC);
    w.client().request_exit(&buyer, &(10 * USDC), &buyer);
    let now = w.now();
    let account_ttl = w.env.as_contract(&w.vault, || {
        w.env.storage().persistent().get_ttl(&Key::Account(buyer.clone()))
    });
    let live_until = [
        w.env.deployer().get_contract_instance_ttl(&w.vault),
        w.env.deployer().get_contract_instance_ttl(&w.usdc),
        account_ttl,
        NOTICE_LEDGERS,
    ]
    .map(|ttl| now + ttl);
    let idle_until = live_until.iter().max().unwrap() + 1;
    w.env.ledger().set_sequence_number(idle_until);
    assert!(live_until.iter().all(|&ledger| ledger < idle_until));

    w.client().exit(&buyer);
    assert_eq!(w.token().balance(&buyer), 10 * USDC);
    assert_eq!(
        refusal(w.client().try_deposit(&buyer, &(10 * USDC), &w.id(1), &None)),
        Error::DepositAlreadyProcessed
    );
    w.assert_solvent();
}

#[test]
fn test_a_charge_record_is_temporary_and_a_replay_is_refused() {
    let w = world();
    let buyer = w.funded(10 * USDC, 5 * USDC);
    let charge = w.charge(&buyer, 1, USDC);
    w.settle(core::slice::from_ref(&charge));
    assert_eq!(w.settle(core::slice::from_ref(&charge)), [Outcome::Duplicate]);
    let has = w.env.as_contract(&w.vault, || {
        w.env.storage().temporary().get_ttl(&Key::Charge(buyer.clone(), w.id(1))) > 0
    });
    assert!(has);
    w.env.ledger().set_sequence_number(charge.3 + CHARGE_RECORD_GRACE + 1);
    assert_eq!(w.settle(&[charge]), [Outcome::Expired]);
    assert_eq!(w.client().get_balance(&buyer), 9 * USDC);
}

// ---- resources -------------------------------------------------------------------

/// Settles `MAX_BATCH` charges for as many distinct buyers, each with a
/// pending lower limit and an exit request so their accounts are as large as
/// they get, in one invocation of the built Wasm.
#[test]
#[ignore = "needs the contract Wasm: set VAULT_WASM (see `just contract-resources`)"]
fn test_full_batch_of_distinct_buyers_fits_one_transaction() {
    let code = wasm_under_test();
    let w = world_with(Some(&code));
    let mut charges = StdVec::new();
    for i in 0..MAX_BATCH {
        let buyer = w.funded(USDC, USDC);
        w.client().set_cap(&buyer, &(USDC / 2));
        w.client().request_exit(&buyer, &USDC, &buyer);
        charges.push(Charge(
            buyer,
            BytesN::from_array(&w.env, &[(i % 256) as u8; 32]),
            USDC / 10,
            w.now() + MAX_CHARGE_WINDOW,
            w.today(),
        ));
    }
    let outcomes = w.settle(&charges);
    let r = w.env.cost_estimate().resources();
    std::println!(
        "entries={} instructions={} mem_bytes={} write_entries={} write_bytes={} footprint={} events_bytes={} estimated_fee_stroops={}",
        outcomes.len(),
        r.instructions,
        r.mem_bytes,
        r.write_entries,
        r.write_bytes,
        r.memory_read_entries + r.disk_read_entries,
        r.contract_events_size_bytes,
        w.env.cost_estimate().fee().total,
    );
    assert_eq!(outcomes, std::vec![Outcome::Charged; MAX_BATCH as usize]);
    assert!(r.instructions <= 400_000_000, "instructions {}", r.instructions);
    assert!(r.mem_bytes <= 41_943_040, "memory {}", r.mem_bytes);
    // Authorizations are mocked, so the operator's nonce, which a real
    // transaction also writes, is not counted here: leave it room.
    assert!(r.write_entries < 200, "write entries {}", r.write_entries);
    assert!(r.write_bytes <= 132_096, "write bytes {}", r.write_bytes);
    assert!(r.disk_read_entries <= 200, "disk reads {}", r.disk_read_entries);
    assert!(r.memory_read_entries + r.disk_read_entries <= 400, "footprint");
    assert!(r.contract_events_size_bytes <= 16_384, "event bytes {}", r.contract_events_size_bytes);
}

// ---- review findings and coverage --------------------------------------------------

/// A charge counts against the day it was admitted in: one admitted within
/// the limit late in a day still settles after midnight, next to the new
/// day's charges.
#[test]
fn test_a_charge_admitted_before_midnight_counts_against_its_own_day() {
    let w = world();
    let buyer = w.funded(30 * USDC, 10 * USDC);
    let late = w.charge(&buyer, 1, 10 * USDC);
    w.next_day();
    let early = w.charge(&buyer, 2, 10 * USDC);
    assert_eq!(w.settle(&[early, late]), [Outcome::Charged, Outcome::Charged]);
    assert_eq!(w.settle(&[w.charge(&buyer, 3, 1)]), [Outcome::AboveCap]);
    assert_eq!(w.client().get_balance(&buyer), 10 * USDC);
}

#[test]
fn test_a_charge_may_name_only_today_or_yesterday() {
    let w = world();
    let buyer = w.funded(30 * USDC, 10 * USDC);
    let old = w.charge(&buyer, 1, USDC);
    w.next_day();
    w.next_day();
    assert_eq!(w.settle(&[old]), [Outcome::Expired]);
    let ahead = Charge(buyer.clone(), w.id(2), USDC, w.now() + 10, w.today() + 1);
    let batch = soroban_sdk::vec![&w.env, ahead];
    assert_eq!(refusal(w.client().try_charge_batch(&batch)), Error::InvalidDay);
}

/// A destination that stops accepting USDC is replaced without losing the
/// notice already served: a request for no more keeps its unlock ledger.
#[test]
fn test_an_exit_can_be_redirected_without_restarting_the_notice() {
    let w = world();
    let buyer = w.funded(10 * USDC, USDC);
    // A classic account with no USDC trustline cannot receive.
    let unpayable = Address::from_string(&SorobanString::from_str(
        &w.env,
        "GAAZI4TCR3TY5OJHCTJC2A4QSY6CJWJH5IAJTGKIN2ER7LBNVKOCCWN7",
    ));
    w.client().request_exit(&buyer, &(5 * USDC), &unpayable);
    let unlock_at = exit_unlock(&w, &buyer);
    w.env.ledger().set_sequence_number(unlock_at);
    assert!(w.client().try_exit(&buyer).is_err());
    let wallet = Address::generate(&w.env);
    w.client().request_exit(&buyer, &(5 * USDC), &wallet);
    assert_eq!(exit_unlock(&w, &buyer), unlock_at);
    w.client().exit(&buyer);
    assert_eq!(w.token().balance(&wallet), 5 * USDC);
    // Asking for more restarts it.
    w.client().request_exit(&buyer, &USDC, &wallet);
    let first = exit_unlock(&w, &buyer);
    w.advance(10);
    w.client().request_exit(&buyer, &(2 * USDC), &wallet);
    assert_eq!(exit_unlock(&w, &buyer), first + 10);
}

#[test]
fn test_repeating_a_pending_lower_limit_keeps_its_effective_ledger() {
    let w = world();
    let buyer = w.funded(10 * USDC, 10 * USDC);
    let lowered_at = w.now();
    w.client().set_cap(&buyer, &0);
    w.advance(NOTICE_LEDGERS - 10);
    token::StellarAssetClient::new(&w.env, &w.usdc).mint(&buyer, &USDC);
    w.client().deposit(&buyer, &USDC, &w.id(2), &Some(0));
    w.env.ledger().set_sequence_number(lowered_at + NOTICE_LEDGERS);
    assert_eq!(w.client().get_cap(&buyer), 0);
    // A stored account carries the limit in force once it applies.
    w.client().set_cap(&buyer, &0);
    assert_eq!(w.client().get_account(&buyer).unwrap().cap, 0);
}

#[test]
fn test_an_operator_that_is_also_a_buyer_withdraws_with_one_signature() {
    let w = world();
    token::StellarAssetClient::new(&w.env, &w.usdc).mint(&w.operator, &(10 * USDC));
    w.client().deposit(&w.operator, &(10 * USDC), &w.id(1), &None);
    w.client().withdraw(&w.operator, &(4 * USDC), &w.operator, &w.id(1));
    assert_eq!(w.signers(), core::slice::from_ref(&w.operator));
    assert_eq!(w.token().balance(&w.operator), 4 * USDC);
}

#[test]
fn test_a_payout_to_the_usdc_contract_is_refused() {
    let w = world();
    let buyer = w.funded(10 * USDC, USDC);
    assert_eq!(
        refusal(w.client().try_withdraw(&buyer, &USDC, &w.usdc, &w.id(1))),
        Error::InvalidDestination
    );
    assert_eq!(
        refusal(w.client().try_request_exit(&buyer, &USDC, &w.usdc)),
        Error::InvalidDestination
    );
    assert_eq!(
        refusal(w.client().try_withdraw_revenue(&w.usdc, &USDC, &w.id(1))),
        Error::InvalidDestination
    );
}

#[test]
fn test_a_mandate_is_revoked_while_paused() {
    let w = world();
    let buyer = w.buyer(10 * USDC);
    let live_until = w.now() + 100_000;
    w.client().authorize_recurring(&buyer, &w.id(1), &USDC, &86_400, &3, &live_until);
    assert_eq!(w.token().allowance(&buyer, &w.vault), 3 * USDC);
    w.client().pause();
    w.client().revoke_recurring(&buyer);
    assert_eq!(w.token().allowance(&buyer, &w.vault), 0);
    assert_eq!(w.client().get_mandate(&buyer), None);
}

#[test]
fn test_a_recurring_charge_before_its_period_is_not_due() {
    let w = world();
    let buyer = w.buyer(10 * USDC);
    let live_until = w.now() + 100_000;
    w.client().authorize_recurring(&buyer, &w.id(1), &USDC, &86_400, &3, &live_until);
    let charge = |cycle: u32, n: u8| RecurringCharge {
        owner: buyer.clone(),
        charge_id: w.id(n),
        mandate_id: w.id(1),
        cycle,
        amount: USDC,
        last_ledger: w.now() + 100,
    };
    assert_eq!(
        w.client().charge_recurring_batch(&vec![&w.env, charge(1, 1)]),
        vec![&w.env, RecurringOutcome::NotDue]
    );
    assert_eq!(
        w.client().charge_recurring_batch(&vec![&w.env, charge(0, 2)]),
        vec![&w.env, RecurringOutcome::Charged]
    );
    assert_eq!(
        w.client().charge_recurring_batch(&vec![&w.env, charge(0, 3)]),
        vec![&w.env, RecurringOutcome::AlreadyCharged]
    );
}

#[test]
fn test_the_admin_role_moves_with_the_admin_and_the_new_holder() {
    let w = world();
    let successor = Address::generate(&w.env);
    w.client().set_admin(&successor);
    assert_eq!(w.signers(), [w.admin.clone(), successor.clone()]);
    assert_eq!(w.client().get_config().admin, successor);
}

#[test]
fn test_malformed_batches_are_refused_whole() {
    let w = world();
    let buyer = w.funded(10 * USDC, 10 * USDC);
    let empty = soroban_sdk::Vec::<Charge>::new(&w.env);
    assert_eq!(refusal(w.client().try_charge_batch(&empty)), Error::EmptyBatch);
    let mut big = soroban_sdk::Vec::new(&w.env);
    for i in 0..=MAX_BATCH {
        big.push_back(w.charge(&buyer, (i % 256) as u8, 1));
    }
    assert_eq!(refusal(w.client().try_charge_batch(&big)), Error::BatchTooLarge);
    let far = w.charge_until(&buyer, 1, 1, w.now() + MAX_CHARGE_WINDOW + 1);
    assert_eq!(
        refusal(w.client().try_charge_batch(&vec![&w.env, far])),
        Error::ChargeWindowTooLong
    );
}

#[test]
fn test_admin_limits_still_bound_each_charge_and_day() {
    let w = world();
    let buyer = w.funded(300 * USDC, 300 * USDC);
    assert_eq!(w.settle(&[w.charge(&buyer, 1, MAX_CHARGE + 1)]), [Outcome::AboveLimit]);
    w.client().set_daily_limits(&DailyLimits { per_buyer: 5 * USDC, per_seller: 100 * USDC });
    assert_eq!(
        w.settle(&[w.charge(&buyer, 2, 5 * USDC), w.charge(&buyer, 3, 1)]),
        [Outcome::Charged, Outcome::AboveDailyLimit]
    );
}
