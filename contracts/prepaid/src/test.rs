//! Contract tests against the real Soroban host.
//!
//! Buyers, operator, seller and treasury are classic `G...` accounts with
//! USDC trustlines, and every call is authorized by Ed25519-signed entries
//! built with the same code the gateway uses, which the host verifies against
//! the account's signers. The USDC contract is the host's built-in Stellar
//! Asset Contract over a test-only asset: a local fixture, not network
//! evidence.

extern crate std;

use std::cell::Cell;
use std::rc::Rc;

use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::prepaid::{
    ChargeRequest, ContractConfig, DepositIntent, PrepaidDeployment, RevenueWithdrawIntent,
    SettledEntry, WithdrawIntent, account_balance, batch_outcomes, charge_record, contract_config,
    settled_entries,
};
use fermah_pay_stellar_domain::AccountAddress;
use soroban_sdk::testutils::Deployer as _;
use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _};
use soroban_sdk::xdr::{self, ScErrorCode, ScErrorType};
use soroban_sdk::{Event as _, IntoVal, Symbol, TryIntoVal, Val, symbol_short};

use super::*;

const USDC: i128 = 10_000_000;
const MIN_DEPOSIT: i128 = USDC / 100;
const MAX_CHARGE: i128 = 5 * USDC;

struct Party {
    key: SecretKey,
    address: Address,
}

struct World {
    env: Env,
    contract: Address,
    usdc: Address,
    asset: xdr::Asset,
    admin: Party,
    operator: Party,
    seller: Party,
    treasury: Party,
    nonce: Cell<i64>,
    /// Test label -> buyer account, so tests can name accounts by number.
    owners: std::cell::RefCell<std::collections::BTreeMap<u8, Address>>,
}

fn account_xdr_id(key: &SecretKey) -> xdr::AccountId {
    xdr::AccountId(xdr::PublicKey::PublicKeyTypeEd25519(xdr::Uint256(*key.address().public_key())))
}

fn contract_bytes(address: &Address) -> [u8; 32] {
    match xdr::ScAddress::from(address) {
        xdr::ScAddress::Contract(xdr::ContractId(xdr::Hash(bytes))) => bytes,
        other => panic!("not a contract address: {other:?}"),
    }
}

/// Adds a classic account whose master key has weight 1, and its USDC
/// trustline holding `usdc` base units.
fn add_party(env: &Env, asset: &xdr::Asset, usdc: i64) -> Party {
    let key = SecretKey::generate().unwrap();
    let account_key = Rc::new(xdr::LedgerKey::Account(xdr::LedgerKeyAccount {
        account_id: account_xdr_id(&key),
    }));
    let account = Rc::new(xdr::LedgerEntry {
        last_modified_ledger_seq: 0,
        data: xdr::LedgerEntryData::Account(xdr::AccountEntry {
            account_id: account_xdr_id(&key),
            balance: 100 * 10_000_000,
            seq_num: xdr::SequenceNumber(0),
            num_sub_entries: 1,
            inflation_dest: None,
            flags: 0,
            home_domain: xdr::String32::default(),
            thresholds: xdr::Thresholds([1, 0, 0, 0]),
            signers: xdr::VecM::default(),
            ext: xdr::AccountEntryExt::V0,
        }),
        ext: xdr::LedgerEntryExt::V0,
    });
    env.host().add_ledger_entry(&account_key, &account, None).unwrap();
    set_trustline(env, &key, asset, usdc);
    let address = Address::from_str(env, key.address().as_str());
    Party { key, address }
}

fn set_trustline(env: &Env, key: &SecretKey, asset: &xdr::Asset, balance: i64) {
    let xdr::Asset::CreditAlphanum4(alpha) = asset else { panic!("test asset is alphanum4") };
    let line = xdr::TrustLineAsset::CreditAlphanum4(alpha.clone());
    let trustline_key = Rc::new(xdr::LedgerKey::Trustline(xdr::LedgerKeyTrustLine {
        account_id: account_xdr_id(key),
        asset: line.clone(),
    }));
    let trustline = Rc::new(xdr::LedgerEntry {
        last_modified_ledger_seq: 0,
        data: xdr::LedgerEntryData::Trustline(xdr::TrustLineEntry {
            account_id: account_xdr_id(key),
            asset: line,
            balance,
            limit: i64::MAX,
            flags: xdr::TrustLineFlags::AuthorizedFlag as u32,
            ext: xdr::TrustLineEntryExt::V0,
        }),
        ext: xdr::LedgerEntryExt::V0,
    });
    env.host().add_ledger_entry(&trustline_key, &trustline, None).unwrap();
}

fn world() -> World {
    world_with(None)
}

/// An environment that writes no per-test JSON ledger snapshot, which is not
/// wanted in the repository.
fn bare_env() -> Env {
    Env::new_with_config(soroban_sdk::testutils::EnvTestConfig { capture_snapshot_at_drop: false })
}

/// A world whose ledger contract runs as `wasm` in the Soroban VM, which is
/// what resource measurements need: a natively registered contract skips VM
/// instantiation, execution and code-size costs.
fn world_with(wasm: Option<&[u8]>) -> World {
    let env = bare_env();
    env.ledger().set_sequence_number(10_000);
    let sac = env.register_stellar_asset_contract_v2(Address::generate(&env));
    let asset = sac.asset();
    let admin = add_party(&env, &asset, 0);
    let operator = add_party(&env, &asset, 0);
    let seller = add_party(&env, &asset, 0);
    let treasury = add_party(&env, &asset, 0);
    let contract = match wasm {
        None => deploy(&env, &admin, &operator, &seller, &treasury, &sac.address()),
        Some(code) => env.register(
            code,
            constructor_args(&admin, &operator, &seller, &treasury, &sac.address()),
        ),
    };
    World {
        env,
        contract,
        usdc: sac.address(),
        asset,
        admin,
        operator,
        seller,
        treasury,
        nonce: Cell::new(1),
        owners: std::cell::RefCell::new(std::collections::BTreeMap::new()),
    }
}

fn constructor_args(
    admin: &Party,
    operator: &Party,
    seller: &Party,
    treasury: &Party,
    usdc: &Address,
) -> (Address, Address, Address, Address, Address, Limits) {
    (
        admin.address.clone(),
        operator.address.clone(),
        seller.address.clone(),
        treasury.address.clone(),
        usdc.clone(),
        Limits { min_deposit: MIN_DEPOSIT, max_charge: MAX_CHARGE },
    )
}

fn deploy(
    env: &Env,
    admin: &Party,
    operator: &Party,
    seller: &Party,
    treasury: &Party,
    usdc: &Address,
) -> Address {
    env.register(PrepaidLedger, constructor_args(admin, operator, seller, treasury, usdc))
}

/// A distinct charge identifier per label.
fn charge_id(label: u64) -> [u8; 32] {
    let mut id = [0xc0; 32];
    id[..8].copy_from_slice(&label.to_le_bytes());
    id
}

fn id32(n: u8) -> [u8; 32] {
    [n; 32]
}

fn contract_error(error: Error) -> soroban_sdk::Error {
    soroban_sdk::Error::from_contract_error(error as u32)
}

fn auth_failure() -> soroban_sdk::Error {
    soroban_sdk::Error::from_type_and_code(ScErrorType::Auth, ScErrorCode::InvalidAction)
}

fn generic_host_error() -> soroban_sdk::Error {
    soroban_sdk::Error::from_type_and_code(ScErrorType::Context, ScErrorCode::InvalidAction)
}

/// The USDC contract's own "balance" refusal, as it surfaces through a call.
fn usdc_insufficient_balance() -> soroban_sdk::Error {
    soroban_sdk::Error::from_contract_error(10)
}

impl World {
    fn client(&self) -> PrepaidLedgerClient<'_> {
        PrepaidLedgerClient::new(&self.env, &self.contract)
    }

    fn party(&self, usdc: i128) -> Party {
        add_party(&self.env, &self.asset, i64::try_from(usdc).unwrap())
    }

    fn usdc_balance(&self, party: &Party) -> i128 {
        token::Client::new(&self.env, &self.usdc).balance(&party.address)
    }

    fn deployment(&self) -> PrepaidDeployment {
        PrepaidDeployment {
            contract: contract_bytes(&self.contract),
            usdc: contract_bytes(&self.usdc),
            treasury: self.treasury.key.address(),
        }
    }

    fn signed(
        &self,
        signer: &Party,
        invocation: xdr::SorobanAuthorizedInvocation,
    ) -> xdr::SorobanAuthorizationEntry {
        let nonce = self.nonce.get();
        self.nonce.set(nonce + 1);
        let entry = xdr::SorobanAuthorizationEntry {
            credentials: xdr::SorobanCredentials::AddressV2(xdr::SorobanAddressCredentials {
                address: xdr::ScAddress::Account(account_xdr_id(&signer.key)),
                nonce,
                signature_expiration_ledger: self.env.ledger().sequence() + 100,
                signature: xdr::ScVal::Void,
            }),
            root_invocation: invocation,
        };
        sign_entry(&entry, self.env.ledger().network_id().to_array(), &[&signer.key]).unwrap()
    }

    /// Invokes `function` with exactly `auths` and returns the precise error
    /// on failure. The host narrows every non-contract error to
    /// `(Context, InvalidAction)` when it crosses a call boundary; the original
    /// error, e.g. a failed signature check, survives only in the diagnostic
    /// events, so that is where the root cause is read from. Contract error
    /// codes pass through unchanged, and this contract's codes do not overlap
    /// the USDC contract's.
    fn invoke<T: soroban_sdk::TryFromVal<Env, Val>>(
        &self,
        function: &str,
        args: soroban_sdk::Vec<Val>,
        auths: &[xdr::SorobanAuthorizationEntry],
    ) -> Result<T, soroban_sdk::Error> {
        self.env.set_auths(auths);
        match self.env.try_invoke_contract::<T, soroban_sdk::Error>(
            &self.contract,
            &Symbol::new(&self.env, function),
            args,
        ) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(conversion)) => panic!("return value conversion failed: {conversion:?}"),
            Err(Ok(error)) if error == generic_host_error() => Err(self.root_cause()),
            Err(Ok(error)) => Err(error),
            Err(Err(invoke)) => panic!("unclassified invoke error: {invoke:?}"),
        }
    }

    /// The error of the invocation that just failed: the most recent error
    /// event. The host does not keep every earlier invocation's events, so
    /// an index taken before the call cannot locate this one's.
    fn root_cause(&self) -> soroban_sdk::Error {
        let events = self.env.host().get_diagnostic_events().unwrap().0;
        events
            .iter()
            .rev()
            .find_map(|event| {
                let xdr::ContractEventBody::V0(body) = &event.event.body;
                match (body.topics.first(), body.topics.get(1)) {
                    (Some(xdr::ScVal::Symbol(symbol)), Some(xdr::ScVal::Error(error)))
                        if symbol.to_utf8_string_lossy() == "error" =>
                    {
                        Some(soroban_sdk::Error::from(error.clone()))
                    }
                    _ => None,
                }
            })
            .expect("a failed invocation records its error in the diagnostic events")
    }

    /// The account labelled `n`: the buyer registered under it, or a fresh
    /// account that never deposited.
    fn owner_address(&self, n: u8) -> Address {
        self.owners.borrow_mut().entry(n).or_insert_with(|| self.party(0).address).clone()
    }

    fn owner_of(&self, n: u8) -> AccountAddress {
        let address = self.owner_address(n);
        let xdr::ScAddress::Account(account) = xdr::ScAddress::from(&address) else {
            panic!("buyer accounts are classic accounts")
        };
        let xdr::AccountId(xdr::PublicKey::PublicKeyTypeEd25519(xdr::Uint256(key))) = account;
        AccountAddress::from_public_key(key)
    }

    /// A charge labelled `id` for the account labelled `n`, settleable for
    /// the next 1000 ledgers.
    fn charge(&self, n: u8, id: u64, amount: i128) -> ChargeRequest {
        self.charge_until(n, id, amount, self.env.ledger().sequence() + 1_000)
    }

    fn charge_until(&self, n: u8, id: u64, amount: i128, last_ledger: u32) -> ChargeRequest {
        ChargeRequest { owner: self.owner_of(n), charge_id: charge_id(id), amount, last_ledger }
    }

    fn contract_charge(&self, request: &ChargeRequest) -> Charge {
        Charge(
            Address::from_str(&self.env, request.owner.as_str()),
            BytesN::from_array(&self.env, &request.charge_id),
            request.amount,
            request.last_ledger,
        )
    }

    fn deposit_intent(&self, buyer: &Party, n: u8, amount: i128, deposit: u8) -> DepositIntent {
        let registered = self.owners.borrow_mut().entry(n).or_insert(buyer.address.clone()).clone();
        assert_eq!(registered, buyer.address, "test label {n} names another buyer");
        DepositIntent { owner: buyer.key.address(), amount, deposit_id: id32(deposit) }
    }

    fn deposit_args(&self, intent: &DepositIntent, owner: &Address) -> soroban_sdk::Vec<Val> {
        (owner.clone(), intent.amount, BytesN::from_array(&self.env, &intent.deposit_id))
            .into_val(&self.env)
    }

    /// A deposit signed by `buyer` for exactly this deployment and intent.
    fn deposit(
        &self,
        buyer: &Party,
        n: u8,
        amount: i128,
        deposit: u8,
    ) -> Result<(), soroban_sdk::Error> {
        let intent = self.deposit_intent(buyer, n, amount, deposit);
        let auth = self.signed(buyer, self.deployment().deposit_authorization(&intent));
        self.invoke(DEPOSIT, self.deposit_args(&intent, &buyer.address), &[auth])
    }

    fn charge_batch(
        &self,
        charges: &[ChargeRequest],
    ) -> Result<soroban_sdk::Vec<Outcome>, soroban_sdk::Error> {
        self.charge_batch_by(&self.operator, charges)
    }

    fn charge_batch_by(
        &self,
        signer: &Party,
        charges: &[ChargeRequest],
    ) -> Result<soroban_sdk::Vec<Outcome>, soroban_sdk::Error> {
        let auth = self.signed(signer, self.deployment().charge_batch_authorization(charges));
        let mut entries = soroban_sdk::Vec::new(&self.env);
        for c in charges {
            entries.push_back(self.contract_charge(c));
        }
        self.invoke("charge_batch", (entries,).into_val(&self.env), &[auth])
    }

    fn withdraw_intent(&self, buyer: &Party, amount: i128, to: &Party, id: u8) -> WithdrawIntent {
        WithdrawIntent {
            owner: buyer.key.address(),
            amount,
            destination: to.key.address(),
            withdrawal_id: id32(id),
        }
    }

    fn withdraw_args(
        &self,
        intent: &WithdrawIntent,
        owner: &Address,
        to: &Address,
    ) -> soroban_sdk::Vec<Val> {
        (
            owner.clone(),
            intent.amount,
            to.clone(),
            BytesN::from_array(&self.env, &intent.withdrawal_id),
        )
            .into_val(&self.env)
    }

    fn balance(&self, n: u8) -> i128 {
        self.client().get_balance(&self.owner_address(n))
    }
}

const DEPOSIT: &str = "deposit";

// ---- deposit: custody and authorization ----------------------------------

#[test]
fn test_deposit_moves_usdc_to_treasury_and_credits_account() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 1, 3 * USDC, 1).unwrap();
    assert_eq!(
        (w.usdc_balance(&buyer), w.usdc_balance(&w.treasury), w.balance(1)),
        (7 * USDC, 3 * USDC, 3 * USDC)
    );
}

#[test]
fn test_contract_never_holds_usdc_after_deposit() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 1, 3 * USDC, 1).unwrap();
    let held = token::Client::new(&w.env, &w.usdc).balance(&w.contract);
    assert_eq!(held, 0);
}

#[test]
fn test_deposit_signed_for_another_treasury_is_refused() {
    let w = world();
    let buyer = w.party(10 * USDC);
    let attacker = w.party(0);
    // The buyer's authorization names a different treasury than the one this
    // contract is pinned to, e.g. a signature obtained for another deployment.
    let intent = w.deposit_intent(&buyer, 1, 3 * USDC, 1);
    let mut elsewhere = w.deployment();
    elsewhere.treasury = attacker.key.address();
    let auth = w.signed(&buyer, elsewhere.deposit_authorization(&intent));
    let result: Result<(), _> = w.invoke(DEPOSIT, w.deposit_args(&intent, &buyer.address), &[auth]);
    assert_eq!(result, Err(auth_failure()));
    assert_eq!((w.usdc_balance(&buyer), w.balance(1)), (10 * USDC, 0));
}

#[test]
fn test_deposit_signature_cannot_be_used_on_another_ledger_contract() {
    let w = world();
    let buyer = w.party(10 * USDC);
    let attacker_treasury = w.party(0);
    let other = deploy(&w.env, &w.admin, &w.operator, &w.seller, &attacker_treasury, &w.usdc);
    // Signed for this deployment, submitted to one whose treasury is the
    // attacker's.
    let intent = w.deposit_intent(&buyer, 1, 3 * USDC, 1);
    let auth = w.signed(&buyer, w.deployment().deposit_authorization(&intent));
    let other_world = World { contract: other, ..w };
    let result: Result<(), _> =
        other_world.invoke(DEPOSIT, other_world.deposit_args(&intent, &buyer.address), &[auth]);
    assert_eq!(result, Err(auth_failure()));
    let w = other_world;
    assert_eq!(w.usdc_balance(&attacker_treasury), 0);
}

#[test]
fn test_deposit_with_amount_other_than_signed_is_refused() {
    let w = world();
    let buyer = w.party(10 * USDC);
    let signed_intent = w.deposit_intent(&buyer, 1, USDC, 1);
    let auth = w.signed(&buyer, w.deployment().deposit_authorization(&signed_intent));
    let larger = w.deposit_intent(&buyer, 1, 5 * USDC, 1);
    let result: Result<(), _> = w.invoke(DEPOSIT, w.deposit_args(&larger, &buyer.address), &[auth]);
    assert_eq!(result, Err(auth_failure()));
}

#[test]
fn test_deposit_signed_by_someone_else_is_refused() {
    let w = world();
    let buyer = w.party(10 * USDC);
    let stranger = w.party(0);
    let intent = w.deposit_intent(&buyer, 1, USDC, 1);
    // A valid signature, but from an account that is not the owner.
    let mut auth = w.signed(&stranger, w.deployment().deposit_authorization(&intent));
    if let xdr::SorobanCredentials::AddressV2(creds) = &mut auth.credentials {
        creds.address = xdr::ScAddress::Account(account_xdr_id(&buyer.key));
    }
    let result: Result<(), _> = w.invoke(DEPOSIT, w.deposit_args(&intent, &buyer.address), &[auth]);
    assert_eq!(result, Err(auth_failure()));
    assert_eq!(w.usdc_balance(&buyer), 10 * USDC);
}

#[test]
fn test_legacy_v1_credentials_are_accepted() {
    let w = world();
    let buyer = w.party(10 * USDC);
    let intent = w.deposit_intent(&buyer, 1, USDC, 1);
    let v2 = w.signed(&buyer, w.deployment().deposit_authorization(&intent));
    let xdr::SorobanCredentials::AddressV2(creds) = v2.credentials else { unreachable!() };
    let v1 = xdr::SorobanAuthorizationEntry {
        credentials: xdr::SorobanCredentials::Address(xdr::SorobanAddressCredentials {
            signature: xdr::ScVal::Void,
            ..creds
        }),
        root_invocation: v2.root_invocation,
    };
    let v1 = sign_entry(&v1, w.env.ledger().network_id().to_array(), &[&buyer.key]).unwrap();
    let result: Result<(), _> = w.invoke(DEPOSIT, w.deposit_args(&intent, &buyer.address), &[v1]);
    assert_eq!((result, w.balance(1)), (Ok(()), USDC));
}

#[test]
fn test_deposit_beyond_buyer_funds_leaves_no_credit() {
    let w = world();
    let buyer = w.party(USDC);
    let result = w.deposit(&buyer, 1, 2 * USDC, 1);
    assert_eq!(result, Err(usdc_insufficient_balance()));
    assert_eq!(
        (w.balance(1), w.client().get_totals().liabilities, w.usdc_balance(&w.treasury)),
        (0, 0, 0)
    );
}

#[test]
fn test_failed_deposit_does_not_consume_its_id() {
    let w = world();
    let buyer = w.party(USDC);
    assert_eq!(w.deposit(&buyer, 1, 2 * USDC, 1), Err(usdc_insufficient_balance()));
    assert_eq!(w.deposit(&buyer, 1, USDC, 1), Ok(()));
}

#[test]
fn test_direct_transfer_to_treasury_creates_no_credit() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.env.mock_all_auths();
    token::Client::new(&w.env, &w.usdc).transfer(&buyer.address, &w.treasury.address, &(3 * USDC));
    assert_eq!(
        (w.usdc_balance(&w.treasury), w.balance(1), w.client().get_totals().liabilities),
        (3 * USDC, 0, 0)
    );
}

#[test]
fn test_duplicate_deposit_id_is_refused_without_moving_usdc() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 1, USDC, 1).unwrap();
    assert_eq!(w.deposit(&buyer, 1, USDC, 1), Err(contract_error(Error::DepositAlreadyProcessed)));
    assert_eq!((w.usdc_balance(&buyer), w.balance(1)), (9 * USDC, USDC));
}

#[test]
fn test_depositing_first_cannot_claim_another_buyers_account() {
    // Accounts are keyed by their owner: an attacker who deposits before a
    // buyer only ever credits its own account.
    let w = world();
    let attacker = w.party(10 * USDC);
    let buyer = w.party(10 * USDC);
    w.deposit(&attacker, 1, USDC, 1).unwrap();
    w.deposit(&buyer, 2, 2 * USDC, 2).unwrap();
    assert_eq!((w.balance(1), w.balance(2)), (USDC, 2 * USDC));
}

#[test]
fn test_deposit_id_used_by_another_owner_does_not_block_a_deposit() {
    let w = world();
    let attacker = w.party(10 * USDC);
    let buyer = w.party(10 * USDC);
    // The attacker copies the buyer's deposit id before the buyer's deposit.
    w.deposit(&attacker, 1, USDC, 7).unwrap();
    assert_eq!((w.deposit(&buyer, 2, USDC, 7), w.balance(2)), (Ok(()), USDC));
}

#[test]
fn test_deposit_below_minimum_is_refused() {
    let w = world();
    let buyer = w.party(10 * USDC);
    assert_eq!(
        w.deposit(&buyer, 1, MIN_DEPOSIT - 1, 1),
        Err(contract_error(Error::BelowMinimumDeposit))
    );
}

#[test]
fn test_deposit_at_minimum_is_accepted() {
    let w = world();
    let buyer = w.party(10 * USDC);
    assert_eq!(w.deposit(&buyer, 1, MIN_DEPOSIT, 1), Ok(()));
}

#[test]
fn test_deposit_above_trustline_range_is_refused() {
    let w = world();
    let buyer = w.party(10 * USDC);
    let too_large = i128::from(i64::MAX) + 1;
    assert_eq!(w.deposit(&buyer, 1, too_large, 1), Err(contract_error(Error::InvalidAmount)));
}

// ---- charges: replay protection and limits -------------------------------

fn funded(w: &World, n: u8, amount: i128) -> Party {
    let buyer = w.party(amount);
    w.deposit(&buyer, n, amount, n).unwrap();
    buyer
}

#[test]
fn test_same_charge_submitted_twice_debits_once() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let first = w.charge_batch(&[w.charge(1, 1, USDC)]);
    let second = w.charge_batch(&[w.charge(1, 1, USDC)]);
    assert_eq!(
        (first.unwrap(), second.unwrap(), w.balance(1)),
        (
            soroban_sdk::vec![&w.env, Outcome::Charged],
            soroban_sdk::vec![&w.env, Outcome::Duplicate],
            9 * USDC
        )
    );
}

#[test]
fn test_single_charge_duplicate_is_rejected() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let call = |w: &World| {
        let request = w.charge(1, 1, USDC);
        let auth = w.signed(
            &w.operator,
            xdr::SorobanAuthorizedInvocation {
                function: xdr::SorobanAuthorizedFunction::ContractFn(xdr::InvokeContractArgs {
                    contract_address: xdr::ScAddress::Contract(xdr::ContractId(xdr::Hash(
                        contract_bytes(&w.contract),
                    ))),
                    function_name: xdr::ScSymbol("charge".try_into().unwrap()),
                    args: xdr::VecM::try_from(std::vec![charge_scval(&w.env, &request)]).unwrap(),
                }),
                sub_invocations: xdr::VecM::default(),
            },
        );
        let args: soroban_sdk::Vec<Val> = (w.contract_charge(&request),).into_val(&w.env);
        w.invoke::<()>("charge", args, &[auth])
    };
    assert_eq!(call(&w), Ok(()));
    assert_eq!(call(&w), Err(contract_error(Error::DuplicateCharge)));
    assert_eq!(w.balance(1), 9 * USDC);
}

#[test]
fn test_refused_single_charge_consumes_nothing() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 1, USDC, 1).unwrap();
    let args: soroban_sdk::Vec<Val> =
        (w.contract_charge(&w.charge(1, 1, 2 * USDC)),).into_val(&w.env);
    let scargs: std::vec::Vec<xdr::ScVal> =
        args.iter().map(|v| v.try_into_val(&w.env).unwrap()).collect();
    let auth = w.signed(
        &w.operator,
        xdr::SorobanAuthorizedInvocation {
            function: xdr::SorobanAuthorizedFunction::ContractFn(xdr::InvokeContractArgs {
                contract_address: xdr::ScAddress::Contract(xdr::ContractId(xdr::Hash(
                    contract_bytes(&w.contract),
                ))),
                function_name: xdr::ScSymbol("charge".try_into().unwrap()),
                args: xdr::VecM::try_from(scargs).unwrap(),
            }),
            sub_invocations: xdr::VecM::default(),
        },
    );
    let refused: Result<(), _> = w.invoke("charge", args, &[auth]);
    // A single refused charge reverts, record included: the same charge
    // identifier is still available afterwards.
    let later = w.charge_batch(&[w.charge(1, 1, USDC / 2)]).unwrap();
    assert_eq!(
        (refused, later),
        (
            Err(contract_error(Error::InsufficientBalance)),
            soroban_sdk::vec![&w.env, Outcome::Charged]
        )
    );
}

fn charge_scval(env: &Env, request: &ChargeRequest) -> xdr::ScVal {
    let val: Val = Charge(
        Address::from_str(env, request.owner.as_str()),
        BytesN::from_array(env, &request.charge_id),
        request.amount,
        request.last_ledger,
    )
    .into_val(env);
    val.try_into_val(env).unwrap()
}

#[test]
fn test_batch_encoding_matches_contract_type() {
    // The gateway builds charge arguments without the contract's Rust types;
    // this pins its encoding to the contract's own.
    let w = world();
    let request = w.charge(3, 7, 123);
    let built = w.deployment().charge_batch_call(core::slice::from_ref(&request)).args[0].clone();
    let xdr::ScVal::Vec(Some(items)) = built else { panic!("batch must be a vector") };
    assert_eq!(items[0], charge_scval(&w.env, &request));
}

#[test]
fn test_constructor_encoding_matches_contract_types() {
    let w = world();
    let roles = fermah_pay_stellar_chain::prepaid::Roles {
        admin: w.admin.key.address(),
        operator: w.operator.key.address(),
        seller: w.seller.key.address(),
        treasury: w.treasury.key.address(),
        usdc: contract_bytes(&w.usdc),
    };
    let built =
        fermah_pay_stellar_chain::prepaid::constructor_args(&roles, MIN_DEPOSIT, MAX_CHARGE);
    let args = constructor_args(&w.admin, &w.operator, &w.seller, &w.treasury, &w.usdc);
    let expected: soroban_sdk::Vec<Val> = args.into_val(&w.env);
    let expected: std::vec::Vec<xdr::ScVal> =
        expected.iter().map(|v| v.try_into_val(&w.env).unwrap()).collect();
    assert_eq!(built, expected);
}

#[test]
fn test_error_codes_never_coincide_with_usdc_contract_codes() {
    // The Stellar Asset Contract's error codes run from 1 to 15 and pass
    // through calls into this contract; ours must be distinguishable.
    let lowest = [
        Error::InvalidLimits,
        Error::Paused,
        Error::InvalidAmount,
        Error::BelowMinimumDeposit,
        Error::DepositAlreadyProcessed,
        Error::UnknownAccount,
        Error::InsufficientBalance,
        Error::ChargeAboveLimit,
        Error::DuplicateCharge,
        Error::ChargeExpired,
        Error::EmptyBatch,
        Error::BatchTooLarge,
        Error::WithdrawalAlreadyProcessed,
        Error::InsufficientRevenue,
        Error::Overflow,
        Error::DuplicateRole,
        Error::ChargeWindowTooLong,
    ]
    .iter()
    .map(|e| *e as u32)
    .min()
    .unwrap();
    assert!(lowest > 15, "lowest contract error code {lowest}");
}

#[test]
fn test_charge_signed_by_non_operator_is_refused() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let charges = [w.charge(1, 1, USDC)];
    let stranger = w.party(0);
    let mut auth = w.signed(&stranger, w.deployment().charge_batch_authorization(&charges));
    if let xdr::SorobanCredentials::AddressV2(creds) = &mut auth.credentials {
        creds.address = xdr::ScAddress::Account(account_xdr_id(&w.operator.key));
    }
    let mut entries = soroban_sdk::Vec::new(&w.env);
    entries.push_back(w.contract_charge(&charges[0]));
    let result: Result<soroban_sdk::Vec<Outcome>, _> =
        w.invoke("charge_batch", (entries,).into_val(&w.env), &[auth]);
    assert_eq!(result, Err(auth_failure()));
    assert_eq!(w.balance(1), 10 * USDC);
}

#[test]
fn test_charge_at_limit_is_charged_and_above_is_refused() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let outcomes =
        w.charge_batch(&[w.charge(1, 1, MAX_CHARGE), w.charge(1, 2, MAX_CHARGE + 1)]).unwrap();
    assert_eq!(
        (outcomes, w.balance(1)),
        (soroban_sdk::vec![&w.env, Outcome::Charged, Outcome::AboveLimit], 10 * USDC - MAX_CHARGE)
    );
}

#[test]
fn test_refused_charge_is_not_charged_after_top_up() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 1, USDC, 1).unwrap();
    let refused = w.charge_batch(&[w.charge(1, 1, 2 * USDC)]).unwrap();
    // After the buyer adds funds, retrying the refused charge must still not
    // debit: the refusal recorded its identifier.
    w.deposit(&buyer, 1, 5 * USDC, 2).unwrap();
    let retried = w.charge_batch(&[w.charge(1, 1, 2 * USDC)]).unwrap();
    assert_eq!(
        (refused, retried, w.balance(1)),
        (
            soroban_sdk::vec![&w.env, Outcome::InsufficientBalance],
            soroban_sdk::vec![&w.env, Outcome::Duplicate],
            6 * USDC
        )
    );
}

#[test]
fn test_charge_past_its_last_ledger_is_refused_and_records_nothing() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let now = w.env.ledger().sequence();
    let late = w.charge_until(1, 1, USDC, now - 1);
    let on_time = w.charge_until(1, 2, USDC, now);
    assert_eq!(
        w.charge_batch(&[late.clone(), on_time]).unwrap(),
        soroban_sdk::vec![&w.env, Outcome::Expired, Outcome::Charged]
    );
    // Nothing was recorded for the expired charge: its identifier is free for
    // a charge that is still on time.
    let renewed = w.charge_until(1, 1, USDC, now + 10);
    assert_eq!(w.charge_batch(&[renewed]).unwrap(), soroban_sdk::vec![&w.env, Outcome::Charged]);
    assert_eq!(w.balance(1), 8 * USDC);
}

#[test]
fn test_replay_after_the_record_expires_is_refused_as_expired() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let charge = w.charge(1, 1, USDC);
    w.charge_batch(core::slice::from_ref(&charge)).unwrap();
    // Past the charge's last ledger and the record's grace period, the
    // record is gone; the charge itself is past its last ledger.
    w.env.ledger().set_sequence_number(charge.last_ledger + CHARGE_RECORD_GRACE + 1);
    assert_eq!(w.charge_batch(&[charge]).unwrap(), soroban_sdk::vec![&w.env, Outcome::Expired]);
    assert_eq!(w.balance(1), 9 * USDC);
}

#[test]
fn test_charge_window_is_bounded() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let now = w.env.ledger().sequence();
    let furthest = w.charge_until(1, 1, USDC, now + MAX_CHARGE_WINDOW);
    assert_eq!(w.charge_batch(&[furthest]).unwrap(), soroban_sdk::vec![&w.env, Outcome::Charged]);
    let beyond = w.charge_until(1, 2, USDC, now + MAX_CHARGE_WINDOW + 1);
    assert_eq!(
        w.charge_batch(&[beyond]).map(|_| ()),
        Err(contract_error(Error::ChargeWindowTooLong))
    );
}

#[test]
fn test_duplicate_inside_one_batch_debits_once() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let outcomes = w.charge_batch(&[w.charge(1, 1, USDC), w.charge(1, 1, USDC)]).unwrap();
    assert_eq!(
        (outcomes, w.balance(1)),
        (soroban_sdk::vec![&w.env, Outcome::Charged, Outcome::Duplicate], 9 * USDC)
    );
}

#[test]
fn test_charge_to_unknown_account_changes_nothing() {
    let w = world();
    let outcomes = w.charge_batch(&[w.charge(9, 1, USDC)]).unwrap();
    assert_eq!(
        (outcomes, w.client().get_totals()),
        (soroban_sdk::vec![&w.env, Outcome::UnknownAccount], Totals { liabilities: 0, revenue: 0 })
    );
}

#[test]
fn test_non_positive_charge_amount_reverts_the_whole_batch() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let result = w.charge_batch(&[w.charge(1, 1, USDC), w.charge(1, 2, 0)]);
    assert_eq!((result, w.balance(1)), (Err(contract_error(Error::InvalidAmount)), 10 * USDC));
}

#[test]
fn test_empty_batch_is_refused() {
    let w = world();
    assert_eq!(w.charge_batch(&[]), Err(contract_error(Error::EmptyBatch)));
}

#[test]
fn test_batch_above_maximum_is_refused() {
    let w = world();
    let charges: std::vec::Vec<_> =
        (0..=MAX_BATCH).map(|i| w.charge(1, u64::from(i) + 1, 1)).collect();
    assert_eq!(w.charge_batch(&charges), Err(contract_error(Error::BatchTooLarge)));
}

#[test]
fn test_mixed_batch_settles_each_entry_independently() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    funded(&w, 2, USDC);
    let outcomes = w
        .charge_batch(&[
            w.charge(1, 1, USDC),
            w.charge(2, 1, 2 * USDC),
            w.charge(3, 1, USDC),
            w.charge(2, 2, USDC / 2),
        ])
        .unwrap();
    assert_eq!(
        (outcomes, w.balance(1), w.balance(2), w.client().get_totals()),
        (
            soroban_sdk::vec![
                &w.env,
                Outcome::Charged,
                Outcome::InsufficientBalance,
                Outcome::UnknownAccount,
                Outcome::Charged
            ],
            9 * USDC,
            USDC / 2,
            Totals { liabilities: 9 * USDC + USDC / 2, revenue: USDC + USDC / 2 }
        )
    );
}

#[test]
fn test_batch_event_reports_every_outcome() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    w.charge_batch(&[w.charge(1, 1, USDC), w.charge(1, 1, USDC)]).unwrap();
    let expected = Charges {
        settled: soroban_sdk::vec![
            &w.env,
            Settled(
                w.owner_address(1),
                BytesN::from_array(&w.env, &charge_id(1)),
                USDC,
                Outcome::Charged
            ),
            Settled(
                w.owner_address(1),
                BytesN::from_array(&w.env, &charge_id(1)),
                USDC,
                Outcome::Duplicate
            ),
        ],
    };
    assert_eq!(
        w.env.events().all().filter_by_contract(&w.contract),
        [expected.to_xdr(&w.env, &w.contract)]
    );
}

// ---- withdrawals ----------------------------------------------------------

fn withdraw_with(
    w: &World,
    intent: &WithdrawIntent,
    owner: &Party,
    to: &Party,
    auths: &[xdr::SorobanAuthorizationEntry],
) -> Result<(), soroban_sdk::Error> {
    w.invoke("withdraw", w.withdraw_args(intent, &owner.address, &to.address), auths)
}

#[test]
fn test_withdraw_with_owner_and_treasury_approval_returns_usdc() {
    let w = world();
    let buyer = funded(&w, 1, 10 * USDC);
    let intent = w.withdraw_intent(&buyer, 4 * USDC, &buyer, 1);
    let auths = [
        w.signed(&buyer, w.deployment().owner_withdraw_authorization(&intent)),
        w.signed(&w.treasury, w.deployment().treasury_withdraw_authorization(&intent)),
    ];
    withdraw_with(&w, &intent, &buyer, &buyer, &auths).unwrap();
    assert_eq!(
        (w.usdc_balance(&buyer), w.usdc_balance(&w.treasury), w.balance(1)),
        (4 * USDC, 6 * USDC, 6 * USDC)
    );
}

#[test]
fn test_withdraw_without_treasury_approval_is_refused() {
    let w = world();
    let buyer = funded(&w, 1, 10 * USDC);
    let intent = w.withdraw_intent(&buyer, USDC, &buyer, 1);
    let auths = [w.signed(&buyer, w.deployment().owner_withdraw_authorization(&intent))];
    assert_eq!(withdraw_with(&w, &intent, &buyer, &buyer, &auths), Err(auth_failure()));
    assert_eq!(w.balance(1), 10 * USDC);
}

#[test]
fn test_withdraw_without_owner_approval_is_refused() {
    let w = world();
    let buyer = funded(&w, 1, 10 * USDC);
    let intent = w.withdraw_intent(&buyer, USDC, &buyer, 1);
    let auths = [w.signed(&w.treasury, w.deployment().treasury_withdraw_authorization(&intent))];
    assert_eq!(withdraw_with(&w, &intent, &buyer, &buyer, &auths), Err(auth_failure()));
    assert_eq!(w.balance(1), 10 * USDC);
}

#[test]
fn test_withdraw_to_destination_other_than_signed_is_refused() {
    let w = world();
    let buyer = funded(&w, 1, 10 * USDC);
    let thief = w.party(0);
    let signed = w.withdraw_intent(&buyer, USDC, &buyer, 1);
    let auths = [
        w.signed(&buyer, w.deployment().owner_withdraw_authorization(&signed)),
        w.signed(&w.treasury, w.deployment().treasury_withdraw_authorization(&signed)),
    ];
    let redirected = w.withdraw_intent(&buyer, USDC, &thief, 1);
    assert_eq!(withdraw_with(&w, &redirected, &buyer, &thief, &auths), Err(auth_failure()));
    assert_eq!(w.usdc_balance(&thief), 0);
}

#[test]
fn test_withdraw_by_non_owner_is_refused() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let stranger = w.party(0);
    let intent = w.withdraw_intent(&stranger, USDC, &stranger, 1);
    let auths = [
        w.signed(&stranger, w.deployment().owner_withdraw_authorization(&intent)),
        w.signed(&w.treasury, w.deployment().treasury_withdraw_authorization(&intent)),
    ];
    // A withdrawal names its owner, whose own account is the only one it can
    // debit; the stranger has none, and the buyer's credit is untouched.
    assert_eq!(
        (withdraw_with(&w, &intent, &stranger, &stranger, &auths), w.balance(1)),
        (Err(contract_error(Error::UnknownAccount)), 10 * USDC)
    );
}

#[test]
fn test_withdraw_more_than_credit_is_refused() {
    let w = world();
    let buyer = funded(&w, 1, USDC);
    let intent = w.withdraw_intent(&buyer, 2 * USDC, &buyer, 1);
    let auths = [
        w.signed(&buyer, w.deployment().owner_withdraw_authorization(&intent)),
        w.signed(&w.treasury, w.deployment().treasury_withdraw_authorization(&intent)),
    ];
    assert_eq!(
        withdraw_with(&w, &intent, &buyer, &buyer, &auths),
        Err(contract_error(Error::InsufficientBalance))
    );
}

#[test]
fn test_withdraw_from_drained_treasury_leaves_credit_intact() {
    let w = world();
    let buyer = funded(&w, 1, 10 * USDC);
    // The treasury key moved the USDC out without calling the contract.
    set_trustline(&w.env, &w.treasury.key, &w.asset, 0);
    let intent = w.withdraw_intent(&buyer, USDC, &buyer, 1);
    let auths = [
        w.signed(&buyer, w.deployment().owner_withdraw_authorization(&intent)),
        w.signed(&w.treasury, w.deployment().treasury_withdraw_authorization(&intent)),
    ];
    assert_eq!(
        withdraw_with(&w, &intent, &buyer, &buyer, &auths),
        Err(usdc_insufficient_balance())
    );
    assert_eq!((w.balance(1), w.client().get_totals().liabilities), (10 * USDC, 10 * USDC));
}

#[test]
fn test_duplicate_withdrawal_id_is_refused() {
    let w = world();
    let buyer = funded(&w, 1, 10 * USDC);
    let run = |id: u8| {
        let intent = w.withdraw_intent(&buyer, USDC, &buyer, id);
        let auths = [
            w.signed(&buyer, w.deployment().owner_withdraw_authorization(&intent)),
            w.signed(&w.treasury, w.deployment().treasury_withdraw_authorization(&intent)),
        ];
        withdraw_with(&w, &intent, &buyer, &buyer, &auths)
    };
    assert_eq!(run(1), Ok(()));
    assert_eq!(run(1), Err(contract_error(Error::WithdrawalAlreadyProcessed)));
    assert_eq!(w.balance(1), 9 * USDC);
}

// ---- administration --------------------------------------------------------

/// Invokes an entry point with one signed entry per signer, each for exactly
/// this call.
fn signed_call(
    w: &World,
    signers: &[&Party],
    function: &str,
    args: soroban_sdk::Vec<Val>,
) -> Result<(), soroban_sdk::Error> {
    let scargs: std::vec::Vec<xdr::ScVal> =
        args.iter().map(|v| v.try_into_val(&w.env).unwrap()).collect();
    let invocation = xdr::SorobanAuthorizedInvocation {
        function: xdr::SorobanAuthorizedFunction::ContractFn(xdr::InvokeContractArgs {
            contract_address: xdr::ScAddress::Contract(xdr::ContractId(xdr::Hash(contract_bytes(
                &w.contract,
            )))),
            function_name: xdr::ScSymbol(function.try_into().unwrap()),
            args: xdr::VecM::try_from(scargs).unwrap(),
        }),
        sub_invocations: xdr::VecM::default(),
    };
    let auths: std::vec::Vec<_> =
        signers.iter().map(|signer| w.signed(signer, invocation.clone())).collect();
    w.invoke(function, args, &auths)
}

fn admin_call(
    w: &World,
    signer: &Party,
    function: &str,
    args: soroban_sdk::Vec<Val>,
) -> Result<(), soroban_sdk::Error> {
    signed_call(w, &[signer], function, args)
}

fn no_args(w: &World) -> soroban_sdk::Vec<Val> {
    soroban_sdk::Vec::new(&w.env)
}

/// Gives `party`'s account two co-signers of weight one next to its master
/// key, and a medium threshold of two: any two of the three keys authorize.
fn make_two_of_three(w: &World, party: &Party, cosigners: &[&SecretKey; 2]) {
    let mut signers: std::vec::Vec<xdr::Signer> = cosigners
        .iter()
        .map(|key| xdr::Signer {
            key: xdr::SignerKey::Ed25519(xdr::Uint256(*key.address().public_key())),
            weight: 1,
        })
        .collect();
    signers.sort_by(|a, b| a.key.cmp(&b.key));
    let account_key = Rc::new(xdr::LedgerKey::Account(xdr::LedgerKeyAccount {
        account_id: account_xdr_id(&party.key),
    }));
    let account = Rc::new(xdr::LedgerEntry {
        last_modified_ledger_seq: 0,
        data: xdr::LedgerEntryData::Account(xdr::AccountEntry {
            account_id: account_xdr_id(&party.key),
            balance: 100 * 10_000_000,
            seq_num: xdr::SequenceNumber(0),
            num_sub_entries: 3,
            inflation_dest: None,
            flags: 0,
            home_domain: xdr::String32::default(),
            thresholds: xdr::Thresholds([1, 1, 2, 2]),
            signers: signers.try_into().unwrap(),
            ext: xdr::AccountEntryExt::V0,
        }),
        ext: xdr::LedgerEntryExt::V0,
    });
    w.env.host().add_ledger_entry(&account_key, &account, None).unwrap();
}

/// `account`'s authorization of `call`, carrying the signatures of `keys`.
fn cosigned(
    w: &World,
    account: &Party,
    keys: &[&SecretKey],
    call: xdr::InvokeContractArgs,
) -> xdr::SorobanAuthorizationEntry {
    let nonce = w.nonce.get();
    w.nonce.set(nonce + 1);
    let entry = xdr::SorobanAuthorizationEntry {
        credentials: xdr::SorobanCredentials::AddressV2(xdr::SorobanAddressCredentials {
            address: xdr::ScAddress::Account(account_xdr_id(&account.key)),
            nonce,
            signature_expiration_ledger: w.env.ledger().sequence() + 100,
            signature: xdr::ScVal::Void,
        }),
        root_invocation: xdr::SorobanAuthorizedInvocation {
            function: xdr::SorobanAuthorizedFunction::ContractFn(call),
            sub_invocations: xdr::VecM::default(),
        },
    };
    let network = w.env.ledger().network_id().to_array();
    let payload = fermah_pay_stellar_chain::authorization::signature_payload(
        network,
        &entry.credentials,
        &entry.root_invocation,
    )
    .unwrap();
    let signatures =
        keys.iter().map(|key| (*key.address().public_key(), key.sign_raw(&payload))).collect();
    fermah_pay_stellar_chain::authorization::attach_signatures(&entry, network, signatures).unwrap()
}

/// Invokes the call an operator's tool builds for `action`, with `auths`.
fn run_action(
    w: &World,
    action: &fermah_pay_stellar_chain::prepaid::AdminAction,
    auths: &[xdr::SorobanAuthorizationEntry],
) -> Result<(), soroban_sdk::Error> {
    let call = action.call(contract_bytes(&w.contract));
    let function = std::string::String::from_utf8(call.function_name.0.to_vec()).unwrap();
    let args = soroban_sdk::Vec::from_iter(
        &w.env,
        call.args.iter().map(|arg| {
            <Val as soroban_sdk::TryFromVal<Env, xdr::ScVal>>::try_from_val(&w.env, arg).unwrap()
        }),
    );
    w.invoke(&function, args, auths)
}

/// The contract's admin actions, built by the operator's proposal tool, run
/// only with two of a two-of-three admin's keys.
#[test]
fn test_a_two_of_three_admin_acts_with_two_signatures_and_not_one() {
    use fermah_pay_stellar_chain::prepaid::{AdminAction, Role};
    let w = world();
    let (first, second) = (SecretKey::generate().unwrap(), SecretKey::generate().unwrap());
    make_two_of_three(&w, &w.admin, &[&first, &second]);
    let contract = contract_bytes(&w.contract);
    let limits = AdminAction::SetLimits { min_deposit: 3 * MIN_DEPOSIT, max_charge: USDC };

    // One co-signer weighs one: refused, and nothing changes.
    let alone = cosigned(&w, &w.admin, &[&first], limits.call(contract));
    assert!(run_action(&w, &limits, &[alone]).is_err());
    assert_eq!(w.client().get_config().limits.min_deposit, MIN_DEPOSIT);
    // The positive control: both co-signers.
    let both = cosigned(&w, &w.admin, &[&first, &second], limits.call(contract));
    run_action(&w, &limits, &[both]).unwrap();
    assert_eq!(
        w.client().get_config().limits,
        Limits { min_deposit: 3 * MIN_DEPOSIT, max_charge: USDC }
    );

    // The master key and one co-signer pause.
    let pause = AdminAction::Pause;
    let master_and_one = cosigned(&w, &w.admin, &[&w.admin.key, &second], pause.call(contract));
    run_action(&w, &pause, &[master_and_one]).unwrap();
    assert!(w.client().get_config().paused);

    // A role change needs the admin's two signatures and the new holder's.
    let successor = w.party(0);
    let rotate = AdminAction::SetRole { role: Role::Operator, holder: successor.key.address() };
    let admin_auth = cosigned(&w, &w.admin, &[&first, &second], rotate.call(contract));
    let holder_auth = cosigned(&w, &successor, &[&successor.key], rotate.call(contract));
    run_action(&w, &rotate, &[admin_auth, holder_auth]).unwrap();
    assert_eq!(w.client().get_config().operator, successor.address);
}

#[test]
#[should_panic(expected = "Error(Contract, #117)")]
fn test_construction_with_one_address_in_two_roles_is_refused() {
    let env = bare_env();
    let sac = env.register_stellar_asset_contract_v2(Address::generate(&env));
    let asset = sac.asset();
    let admin = add_party(&env, &asset, 0);
    let operator = add_party(&env, &asset, 0);
    let seller = add_party(&env, &asset, 0);
    // The operator doubling as treasury could pay out USDC it charges.
    deploy(&env, &admin, &operator, &seller, &operator, &sac.address());
}

#[test]
fn test_every_admin_entry_point_refuses_another_signer() {
    let w = world();
    let newcomer = w.party(0);
    let limits = Limits { min_deposit: MIN_DEPOSIT, max_charge: USDC };
    let calls: [(&str, soroban_sdk::Vec<Val>); 8] = [
        ("pause", no_args(&w)),
        ("unpause", no_args(&w)),
        ("set_limits", (limits,).into_val(&w.env)),
        ("set_admin", (newcomer.address.clone(),).into_val(&w.env)),
        ("set_operator", (newcomer.address.clone(),).into_val(&w.env)),
        ("set_seller", (newcomer.address.clone(),).into_val(&w.env)),
        ("set_treasury", (newcomer.address.clone(),).into_val(&w.env)),
        ("upgrade", (BytesN::from_array(&w.env, &[7; 32]),).into_val(&w.env)),
    ];
    for (function, args) in calls {
        // The operator authorizes as itself, and a rotation's newcomer signs
        // too: only the admin's authorization is missing.
        let result = signed_call(&w, &[&w.operator, &newcomer], function, args);
        assert_eq!(result, Err(auth_failure()), "{function}");
    }
    let config = w.client().get_config();
    assert_eq!(
        (config.admin, config.operator, config.paused, config.limits.max_charge),
        (w.admin.address.clone(), w.operator.address.clone(), false, MAX_CHARGE)
    );
}

#[test]
fn test_pause_stops_every_money_movement_until_unpaused() {
    let w = world();
    let buyer = funded(&w, 1, 10 * USDC);
    admin_call(&w, &w.admin, "pause", no_args(&w)).unwrap();
    let paused = Err(contract_error(Error::Paused));
    assert_eq!(w.charge_batch(&[w.charge(1, 1, USDC)]).map(|_| ()), paused);
    assert_eq!(w.deposit(&buyer, 1, USDC, 2), paused);
    let intent = w.withdraw_intent(&buyer, USDC, &buyer, 1);
    let auths = [
        w.signed(&buyer, w.deployment().owner_withdraw_authorization(&intent)),
        w.signed(&w.treasury, w.deployment().treasury_withdraw_authorization(&intent)),
    ];
    assert_eq!(withdraw_with(&w, &intent, &buyer, &buyer, &auths), paused);
    assert_eq!(withdraw_revenue(&w, 1, 1), paused);

    admin_call(&w, &w.admin, "unpause", no_args(&w)).unwrap();
    assert_eq!(
        w.charge_batch(&[w.charge(1, 1, USDC)]).unwrap(),
        soroban_sdk::vec![&w.env, Outcome::Charged]
    );
}

#[test]
fn test_rotation_needs_both_the_admin_and_the_new_holder() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let replacement = w.party(0);
    let args = || (replacement.address.clone(),).into_val(&w.env);
    for signers in [&[&w.admin][..], &[&replacement][..]] {
        assert_eq!(signed_call(&w, signers, "set_operator", args()), Err(auth_failure()));
    }
    signed_call(&w, &[&w.admin, &replacement], "set_operator", args()).unwrap();

    let expected = RoleChanged {
        role: symbol_short!("operator"),
        previous: w.operator.address.clone(),
        current: replacement.address.clone(),
    };
    assert_eq!(
        w.env.events().all().filter_by_contract(&w.contract),
        [expected.to_xdr(&w.env, &w.contract)]
    );
    // The previous operator can no longer charge; the new one can.
    assert_eq!(w.charge_batch(&[w.charge(1, 1, USDC)]).map(|_| ()), Err(auth_failure()));
    assert_eq!(
        w.charge_batch_by(&replacement, &[w.charge(1, 1, USDC)]).unwrap(),
        soroban_sdk::vec![&w.env, Outcome::Charged]
    );
}

#[test]
fn test_rotation_onto_another_role_or_the_same_holder_is_refused() {
    let w = world();
    let cases = [
        ("set_operator", &w.treasury),
        ("set_treasury", &w.seller),
        ("set_seller", &w.admin),
        ("set_admin", &w.operator),
        ("set_operator", &w.operator),
    ];
    for (function, holder) in cases {
        let args = (holder.address.clone(),).into_val(&w.env);
        assert_eq!(
            signed_call(&w, &[&w.admin, holder], function, args),
            Err(contract_error(Error::DuplicateRole)),
            "{function}"
        );
    }
}

#[test]
fn test_admin_rotation_moves_every_admin_power() {
    let w = world();
    let successor = w.party(0);
    signed_call(
        &w,
        &[&w.admin, &successor],
        "set_admin",
        (successor.address.clone(),).into_val(&w.env),
    )
    .unwrap();
    assert_eq!(admin_call(&w, &w.admin, "pause", no_args(&w)), Err(auth_failure()));
    admin_call(&w, &successor, "pause", no_args(&w)).unwrap();
    assert!(w.client().get_config().paused);
}

#[test]
fn test_treasury_rotation_sends_new_deposits_to_the_new_treasury() {
    let w = world();
    let successor = w.party(0);
    signed_call(
        &w,
        &[&w.admin, &successor],
        "set_treasury",
        (successor.address.clone(),).into_val(&w.env),
    )
    .unwrap();
    let buyer = w.party(10 * USDC);
    // Signed for the old treasury: the contract now transfers to the new one,
    // so the buyer's authorization no longer matches and nothing moves.
    assert_eq!(w.deposit(&buyer, 1, USDC, 1), Err(auth_failure()));

    let rotated = PrepaidDeployment { treasury: successor.key.address(), ..w.deployment() };
    let intent = w.deposit_intent(&buyer, 1, USDC, 2);
    let auth = w.signed(&buyer, rotated.deposit_authorization(&intent));
    w.invoke::<()>("deposit", w.deposit_args(&intent, &buyer.address), &[auth]).unwrap();
    assert_eq!((w.usdc_balance(&successor), w.usdc_balance(&w.treasury)), (USDC, 0));
}

#[test]
fn test_seller_rotation_moves_revenue_withdrawal() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    w.charge_batch(&[w.charge(1, 1, 2 * USDC)]).unwrap();
    let successor = w.party(0);
    signed_call(
        &w,
        &[&w.admin, &successor],
        "set_seller",
        (successor.address.clone(),).into_val(&w.env),
    )
    .unwrap();
    assert_eq!(withdraw_revenue(&w, USDC, 1), Err(auth_failure()));

    let intent = RevenueWithdrawIntent {
        destination: successor.key.address(),
        amount: USDC,
        withdrawal_id: id32(2),
    };
    let auths = [
        w.signed(&successor, w.deployment().seller_revenue_authorization(&intent)),
        w.signed(&w.treasury, w.deployment().treasury_revenue_authorization(&intent)),
    ];
    let args: soroban_sdk::Vec<Val> =
        (successor.address.clone(), USDC, BytesN::from_array(&w.env, &id32(2))).into_val(&w.env);
    w.invoke::<()>("withdraw_revenue", args, &auths).unwrap();
    assert_eq!(w.usdc_balance(&successor), USDC);
}

#[test]
fn test_admin_calls_keep_the_instance_alive() {
    let w = world();
    // Idle until fewer ledgers than the extension threshold remain.
    let idle = TTL_EXTEND_TO - TTL_THRESHOLD + 1;
    let start = w.env.ledger().sequence();
    w.env.ledger().set_sequence_number(start + idle);
    let before = w.env.deployer().get_contract_instance_ttl(&w.contract);
    assert!(before < TTL_THRESHOLD, "{before}");
    admin_call(&w, &w.admin, "pause", no_args(&w)).unwrap();
    assert_eq!(w.env.deployer().get_contract_instance_ttl(&w.contract), TTL_EXTEND_TO);
}

#[test]
fn test_invalid_limits_are_refused() {
    let w = world();
    let limits = Limits { min_deposit: 0, max_charge: USDC };
    assert_eq!(
        admin_call(&w, &w.admin, "set_limits", (limits,).into_val(&w.env)),
        Err(contract_error(Error::InvalidLimits))
    );
}

#[test]
#[ignore = "needs the contract Wasm: set PREPAID_WASM (see `just contract-resources`)"]
fn test_full_upgrade_by_admin_keeps_balances() {
    let code = wasm_under_test();
    let w = world_with(Some(&code));
    funded(&w, 1, 10 * USDC);
    let hash = w.env.deployer().upload_contract_wasm(code.as_slice());
    admin_call(&w, &w.admin, "upgrade", (hash,).into_val(&w.env)).unwrap();
    assert_eq!(w.client().get_balance(&w.owner_address(1)), 10 * USDC);
}

// ---- daily limits -----------------------------------------------------------

fn set_daily(w: &World, per_buyer: i128, per_seller: i128) -> Result<(), soroban_sdk::Error> {
    admin_call(
        w,
        &w.admin,
        "set_daily_limits",
        (DailyLimits { per_buyer, per_seller },).into_val(&w.env),
    )
}

fn next_day(w: &World) {
    let now = w.env.ledger().timestamp();
    w.env.ledger().set_timestamp(now + 86_400);
}

#[test]
fn test_without_daily_limits_nothing_is_counted_against_a_charge() {
    let w = world();
    funded(&w, 1, 3 * MAX_CHARGE);
    let outcomes = w
        .charge_batch(&[
            w.charge(1, 1, MAX_CHARGE),
            w.charge(1, 2, MAX_CHARGE),
            w.charge(1, 3, MAX_CHARGE),
        ])
        .unwrap();
    assert!(outcomes.iter().all(|o| o == Outcome::Charged));
    assert_eq!(w.client().get_daily_limits(), None);
}

#[test]
fn test_a_buyer_past_its_daily_limit_is_refused_until_the_next_day() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    set_daily(&w, 3 * USDC, 100 * USDC).unwrap();
    let outcomes = w
        .charge_batch(&[w.charge(1, 1, 2 * USDC), w.charge(1, 2, 2 * USDC), w.charge(1, 3, USDC)])
        .unwrap();
    assert_eq!(
        (outcomes, w.balance(1)),
        (
            soroban_sdk::vec![&w.env, Outcome::Charged, Outcome::AboveDailyLimit, Outcome::Charged],
            7 * USDC
        )
    );
    // The refusal is recorded: the same charge is a duplicate, not a debit.
    assert_eq!(
        w.charge_batch(&[w.charge(1, 2, 2 * USDC)]).unwrap(),
        soroban_sdk::vec![&w.env, Outcome::Duplicate]
    );
    assert_eq!(
        w.charge_batch(&[w.charge(1, 4, USDC)]).unwrap(),
        soroban_sdk::vec![&w.env, Outcome::AboveDailyLimit]
    );
    next_day(&w);
    assert_eq!(
        w.charge_batch(&[w.charge(1, 5, 3 * USDC)]).unwrap(),
        soroban_sdk::vec![&w.env, Outcome::Charged]
    );
    assert_eq!(w.balance(1), 4 * USDC);
}

#[test]
fn test_the_seller_daily_limit_counts_every_account_across_calls() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    funded(&w, 2, 10 * USDC);
    set_daily(&w, 10 * USDC, 5 * USDC).unwrap();
    let outcomes = w.charge_batch(&[w.charge(1, 1, 2 * USDC), w.charge(2, 2, 2 * USDC)]).unwrap();
    assert!(outcomes.iter().all(|o| o == Outcome::Charged));
    let outcomes = w.charge_batch(&[w.charge(1, 3, 2 * USDC), w.charge(2, 4, USDC)]).unwrap();
    assert_eq!(outcomes, soroban_sdk::vec![&w.env, Outcome::AboveDailyLimit, Outcome::Charged]);
    let totals = w.client().get_totals();
    assert_eq!(totals.revenue, 5 * USDC);
    next_day(&w);
    assert_eq!(
        w.charge_batch(&[w.charge(1, 5, 2 * USDC)]).unwrap(),
        soroban_sdk::vec![&w.env, Outcome::Charged]
    );
}

#[test]
fn test_a_single_charge_past_a_daily_limit_reverts_with_its_reason() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    set_daily(&w, USDC, 100 * USDC).unwrap();
    let request = w.charge(1, 1, 2 * USDC);
    let auth = w.signed(&w.operator, w.deployment().charge_authorization(&request));
    let args: soroban_sdk::Vec<Val> = (w.contract_charge(&request),).into_val(&w.env);
    assert_eq!(
        w.invoke::<()>("charge", args, &[auth]),
        Err(contract_error(Error::ChargeAboveDailyLimit))
    );
    assert_eq!(w.balance(1), 10 * USDC);
}

#[test]
fn test_only_the_admin_sets_positive_daily_limits_and_each_change_is_announced() {
    let w = world();
    let limits = (DailyLimits { per_buyer: USDC, per_seller: USDC },).into_val(&w.env);
    assert_eq!(admin_call(&w, &w.operator, "set_daily_limits", limits), Err(auth_failure()));
    assert_eq!(set_daily(&w, 0, USDC), Err(contract_error(Error::InvalidLimits)));
    assert_eq!(set_daily(&w, USDC, -1), Err(contract_error(Error::InvalidLimits)));
    set_daily(&w, USDC, 2 * USDC).unwrap();
    let last = w.env.events().all().events().last().unwrap().clone();
    let expected = DailyLimitsChanged {
        previous: None,
        current: DailyLimits { per_buyer: USDC, per_seller: 2 * USDC },
    };
    assert_eq!(last, expected.to_xdr(&w.env, &w.contract));
    // The gateway's decoder reads the event the contract really emits.
    let xdr::ContractEventBody::V0(body) = &last.body;
    assert_eq!(
        fermah_pay_stellar_chain::prepaid::ledger_event(&body.topics, &body.data),
        Some(fermah_pay_stellar_chain::prepaid::LedgerEvent::DailyLimitsChanged {
            previous: None,
            current: fermah_pay_stellar_chain::prepaid::DailyLimits {
                per_buyer: USDC,
                per_seller: 2 * USDC,
            },
        })
    );
    assert_eq!(
        w.client().get_daily_limits(),
        Some(DailyLimits { per_buyer: USDC, per_seller: 2 * USDC })
    );
}

#[test]
fn test_an_account_stored_before_daily_counts_is_read_and_charged() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    let owner = w.owner_address(1);
    // As the first version of the contract wrote it.
    w.env.as_contract(&w.contract, || {
        w.env
            .storage()
            .persistent()
            .set(&Key::Account(owner.clone()), &AccountV1 { balance: 10 * USDC });
    });
    assert_eq!(w.balance(1), 10 * USDC);
    set_daily(&w, 3 * USDC, 100 * USDC).unwrap();
    let outcomes = w.charge_batch(&[w.charge(1, 1, 2 * USDC), w.charge(1, 2, 2 * USDC)]).unwrap();
    assert_eq!(outcomes, soroban_sdk::vec![&w.env, Outcome::Charged, Outcome::AboveDailyLimit]);
    let account = w.client().get_account(&owner).unwrap();
    assert_eq!((account.balance, account.charged), (8 * USDC, 2 * USDC));
}

// ---- revenue ----------------------------------------------------------------

fn withdraw_revenue(w: &World, amount: i128, id: u8) -> Result<(), soroban_sdk::Error> {
    let intent = RevenueWithdrawIntent {
        destination: w.seller.key.address(),
        amount,
        withdrawal_id: id32(id),
    };
    let auths = [
        w.signed(&w.seller, w.deployment().seller_revenue_authorization(&intent)),
        w.signed(&w.treasury, w.deployment().treasury_revenue_authorization(&intent)),
    ];
    let args: soroban_sdk::Vec<Val> =
        (w.seller.address.clone(), amount, BytesN::from_array(&w.env, &id32(id))).into_val(&w.env);
    w.invoke("withdraw_revenue", args, &auths)
}

#[test]
fn test_revenue_withdrawal_pays_seller_from_treasury() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    w.charge_batch(&[w.charge(1, 1, 2 * USDC)]).unwrap();
    withdraw_revenue(&w, 2 * USDC, 1).unwrap();
    assert_eq!(
        (w.usdc_balance(&w.seller), w.usdc_balance(&w.treasury), w.client().get_totals()),
        (2 * USDC, 8 * USDC, Totals { liabilities: 8 * USDC, revenue: 0 })
    );
}

#[test]
fn test_revenue_withdrawal_is_capped_by_earned_revenue() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    w.charge_batch(&[w.charge(1, 1, 2 * USDC)]).unwrap();
    assert_eq!(
        withdraw_revenue(&w, 2 * USDC + 1, 1),
        Err(contract_error(Error::InsufficientRevenue))
    );
}

// ---- resource ceiling: MAX_BATCH buyers in one invocation ------------------

/// Stellar testnet per-transaction Soroban limits read from the network
/// configuration on 2026-09-28 (`ContractComputeV0`, `ContractLedgerCostV0`,
/// `ContractLedgerCostExtV0`, `ContractEventsV0`).
const TX_MAX_INSTRUCTIONS: i64 = 400_000_000;
const TX_MEMORY_LIMIT: i64 = 41_943_040;
const TX_MAX_WRITE_ENTRIES: u32 = 200;
const TX_MAX_WRITE_BYTES: u32 = 132_096;
const TX_MAX_DISK_READ_ENTRIES: u32 = 200;
const TX_MAX_FOOTPRINT_ENTRIES: u32 = 400;
const TX_MAX_EVENTS_BYTES: u32 = 16_384;

fn wasm_under_test() -> std::vec::Vec<u8> {
    let path = std::env::var("PREPAID_WASM")
        .expect("set PREPAID_WASM to the contract built by `stellar contract build`");
    std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
}

#[derive(Debug)]
struct Measured {
    instructions: i64,
    mem_bytes: i64,
    write_entries: u32,
    write_bytes: u32,
    disk_read_entries: u32,
    memory_read_entries: u32,
    events_bytes: u32,
    fee_stroops: i64,
    outcomes: std::vec::Vec<Outcome>,
}

/// Settles `MAX_BATCH` charges for as many distinct buyers in one
/// invocation. In the mixed batch every fourth charge exceeds its balance and
/// every fifth repeats an already-recorded charge.
fn measure_full_batch(mixed: bool) -> Measured {
    let code = wasm_under_test();
    let w = world_with(Some(&code));
    let n = u8::try_from(MAX_BATCH).unwrap();
    let mut charges = std::vec::Vec::new();
    for i in 1..=n {
        funded(&w, i, USDC);
        let amount = if mixed && i % 4 == 0 { 2 * USDC } else { USDC / 10 };
        charges.push(w.charge(i, 1, amount));
    }
    if mixed {
        let consumed: std::vec::Vec<_> =
            (1..=n).filter(|i| i % 5 == 0).map(|i| w.charge(i, 1, USDC / 10)).collect();
        w.charge_batch(&consumed).unwrap();
    }
    let outcomes = w.charge_batch(&charges).unwrap();
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
        outcomes: outcomes.iter().collect(),
    };
    std::println!(
        "entries={} instructions={} mem_bytes={} write_entries={} write_bytes={} footprint={} events_bytes={} estimated_fee_stroops={}",
        measured.outcomes.len(),
        measured.instructions,
        measured.mem_bytes,
        measured.write_entries,
        measured.write_bytes,
        measured.memory_read_entries + measured.disk_read_entries,
        measured.events_bytes,
        measured.fee_stroops
    );
    measured
}

fn assert_within_limits(m: &Measured) {
    let footprint = m.memory_read_entries + m.disk_read_entries;
    assert!(m.instructions <= TX_MAX_INSTRUCTIONS, "instructions {}", m.instructions);
    assert!(m.mem_bytes <= TX_MEMORY_LIMIT, "memory {}", m.mem_bytes);
    assert!(m.write_entries <= TX_MAX_WRITE_ENTRIES, "write entries {}", m.write_entries);
    assert!(m.write_bytes <= TX_MAX_WRITE_BYTES, "write bytes {}", m.write_bytes);
    assert!(m.disk_read_entries <= TX_MAX_DISK_READ_ENTRIES, "disk reads {}", m.disk_read_entries);
    assert!(footprint <= TX_MAX_FOOTPRINT_ENTRIES, "footprint {footprint}");
    assert!(m.events_bytes <= TX_MAX_EVENTS_BYTES, "event bytes {}", m.events_bytes);
}

#[test]
#[ignore = "needs the contract Wasm: set PREPAID_WASM (see `just contract-resources`)"]
fn test_full_batch_of_distinct_buyers_fits_one_transaction() {
    let m = measure_full_batch(false);
    assert_eq!(m.outcomes, std::vec![Outcome::Charged; MAX_BATCH as usize]);
    assert_within_limits(&m);
}

#[test]
#[ignore = "needs the contract Wasm: set PREPAID_WASM (see `just contract-resources`)"]
fn test_full_mixed_failure_batch_fits_one_transaction() {
    let m = measure_full_batch(true);
    let refused = m.outcomes.iter().filter(|o| **o == Outcome::InsufficientBalance).count();
    let duplicates = m.outcomes.iter().filter(|o| **o == Outcome::Duplicate).count();
    // Every fifth buyer's charge was already settled, and every fourth of
    // the rest asks for more than the buyer holds.
    let n = MAX_BATCH as usize;
    let expected_duplicates = n / 5;
    let expected_refused = n / 4 - n / 20;
    assert_eq!((m.outcomes.len(), refused, duplicates), (n, expected_refused, expected_duplicates));
    assert_within_limits(&m);
}

#[test]
fn test_revenue_withdrawal_without_seller_approval_is_refused() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    w.charge_batch(&[w.charge(1, 1, 2 * USDC)]).unwrap();
    let intent = RevenueWithdrawIntent {
        destination: w.seller.key.address(),
        amount: USDC,
        withdrawal_id: id32(1),
    };
    let auths = [w.signed(&w.treasury, w.deployment().treasury_revenue_authorization(&intent))];
    let args: soroban_sdk::Vec<Val> =
        (w.seller.address.clone(), USDC, BytesN::from_array(&w.env, &id32(1))).into_val(&w.env);
    let result: Result<(), _> = w.invoke("withdraw_revenue", args, &auths);
    assert_eq!((result, w.client().get_totals().revenue), (Err(auth_failure()), 2 * USDC));
}

#[test]
fn test_treasury_approval_of_a_bare_transfer_cannot_fund_a_withdrawal() {
    let w = world();
    let buyer = funded(&w, 1, 10 * USDC);
    let intent = w.withdraw_intent(&buyer, USDC, &buyer, 1);
    // The treasury approved only a transfer of the same amount to the same
    // destination, e.g. for an unrelated payout; it did not approve this
    // withdrawal from this account.
    let transfer_only = xdr::SorobanAuthorizedInvocation {
        function: xdr::SorobanAuthorizedFunction::ContractFn(xdr::InvokeContractArgs {
            contract_address: xdr::ScAddress::Contract(xdr::ContractId(xdr::Hash(contract_bytes(
                &w.usdc,
            )))),
            function_name: xdr::ScSymbol("transfer".try_into().unwrap()),
            args: xdr::VecM::try_from(std::vec![
                xdr::ScVal::Address(xdr::ScAddress::Account(account_xdr_id(&w.treasury.key))),
                xdr::ScVal::Address(xdr::ScAddress::Account(account_xdr_id(&buyer.key))),
                xdr::ScVal::I128(xdr::Int128Parts { hi: 0, lo: u64::try_from(USDC).unwrap() }),
            ])
            .unwrap(),
        }),
        sub_invocations: xdr::VecM::default(),
    };
    let auths = [
        w.signed(&buyer, w.deployment().owner_withdraw_authorization(&intent)),
        w.signed(&w.treasury, transfer_only),
    ];
    assert_eq!(withdraw_with(&w, &intent, &buyer, &buyer, &auths), Err(auth_failure()));
    assert_eq!(w.balance(1), 10 * USDC);
}

// ---- storage layout the gateway reads -----------------------------------

/// The gateway decides whether a deposit was credited, and how a charge was
/// settled, by reading these entries directly. Their keys and
/// value shapes are contract-internal, so they are pinned here against the
/// real host rather than restated in the gateway.
#[test]
fn test_gateway_storage_keys_match_the_contract_layout() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 1, USDC, 1).unwrap();
    let outcomes = w.charge_batch(&[w.charge(1, 1, 100), w.charge(1, 2, MAX_CHARGE + 1)]).unwrap();
    assert_eq!(outcomes, soroban_sdk::vec![&w.env, Outcome::Charged, Outcome::AboveLimit]);

    let snapshot = w.env.to_ledger_snapshot();
    let entry = |key: &xdr::LedgerKey| {
        snapshot.ledger_entries.iter().find(|(k, _)| **k == *key).map(|(_, (e, _))| e.data.clone())
    };
    let deployment = w.deployment();
    let owner = w.owner_of(1);
    // Only the first charge debited; both recorded their outcome.
    let account = entry(&deployment.account_key(&owner)).expect("account entry at the derived key");
    assert_eq!(account_balance(&account), Some(USDC - 100));
    let recorded = |id| {
        entry(&deployment.charge_record_key(&owner, &charge_id(id)))
            .as_ref()
            .and_then(charge_record)
    };
    use fermah_pay_stellar_chain::prepaid::Outcome as Decoded;
    assert_eq!(
        (recorded(1), recorded(2), recorded(3)),
        (Some(Decoded::Charged), Some(Decoded::AboveLimit), None)
    );
    assert!(entry(&deployment.deposit_key(&owner, &id32(1))).is_some());
    // Controls: a deposit id never used, and an owner that never deposited.
    assert!(entry(&deployment.deposit_key(&owner, &id32(2))).is_none());
    assert!(entry(&deployment.account_key(&w.owner_of(2))).is_none());
}

#[test]
fn test_gateway_outcome_decoding_matches_every_contract_outcome() {
    use fermah_pay_stellar_chain::prepaid::Outcome as Decoded;
    let env = bare_env();
    let all = soroban_sdk::vec![
        &env,
        Outcome::Charged,
        Outcome::InsufficientBalance,
        Outcome::AboveLimit,
        Outcome::Duplicate,
        Outcome::Expired,
        Outcome::UnknownAccount,
    ];
    let val: Val = all.into_val(&env);
    let encoded =
        <xdr::ScVal as soroban_sdk::TryFromVal<Env, Val>>::try_from_val(&env, &val).unwrap();
    assert_eq!(
        batch_outcomes(&encoded),
        Some(std::vec![
            Decoded::Charged,
            Decoded::InsufficientBalance,
            Decoded::AboveLimit,
            Decoded::Duplicate,
            Decoded::Expired,
            Decoded::UnknownAccount,
        ])
    );
}

/// An operator resolving a quarantined charge proves its outcome from this
/// event in the transaction that settled the charge, so its layout is
/// pinned against the real host.
#[test]
fn test_gateway_reads_each_settled_charge_from_the_batch_event() {
    use fermah_pay_stellar_chain::prepaid::Outcome as Decoded;
    let w = world();
    funded(&w, 1, 10 * USDC);
    w.charge_batch(&[w.charge(1, 1, USDC), w.charge(1, 2, MAX_CHARGE + 1)]).unwrap();
    let events = w.env.events().all();
    let decoded: std::vec::Vec<SettledEntry> = events
        .events()
        .iter()
        .filter_map(|event| settled_entries(event, &contract_bytes(&w.contract)))
        .flatten()
        .collect();
    let owner = w.owner_of(1);
    assert_eq!(
        decoded,
        [
            SettledEntry {
                owner: owner.clone(),
                charge_id: charge_id(1),
                amount: USDC,
                outcome: Decoded::Charged,
            },
            SettledEntry {
                owner,
                charge_id: charge_id(2),
                amount: MAX_CHARGE + 1,
                outcome: Decoded::AboveLimit,
            },
        ]
    );
    // Control: the same events, attributed to another contract, yield nothing.
    assert!(events.events().iter().all(|event| settled_entries(event, &[0; 32]).is_none()));
}

/// A deployment's binding is moved after a role rotation from what
/// `get_config` returns, so the decoding is pinned against the real host.
#[test]
fn test_gateway_reads_the_roles_from_get_config() {
    let w = world();
    let val: Val = w.client().get_config().into_val(&w.env);
    let encoded =
        <xdr::ScVal as soroban_sdk::TryFromVal<Env, Val>>::try_from_val(&w.env, &val).unwrap();
    assert_eq!(
        contract_config(&encoded),
        Some(ContractConfig {
            admin: w.admin.key.address(),
            operator: w.operator.key.address(),
            seller: w.seller.key.address(),
            treasury: w.treasury.key.address(),
            usdc: contract_bytes(&w.usdc),
            paused: false,
        })
    );
}

#[test]
fn test_gateway_mirrors_the_contract_limits() {
    use fermah_pay_stellar_chain::prepaid as gateway;
    assert_eq!(
        (gateway::MAX_BATCH as u32, gateway::MAX_CHARGE_WINDOW, gateway::CHARGE_RECORD_GRACE),
        (MAX_BATCH, MAX_CHARGE_WINDOW, CHARGE_RECORD_GRACE)
    );
}

#[test]
fn test_pause_and_unpause_are_announced() {
    let w = world();
    let announced = |w: &World| w.env.events().all().filter_by_contract(&w.contract);
    admin_call(&w, &w.admin, "pause", no_args(&w)).unwrap();
    assert_eq!(announced(&w), [PauseChanged { paused: true }.to_xdr(&w.env, &w.contract)]);
    admin_call(&w, &w.admin, "unpause", no_args(&w)).unwrap();
    assert_eq!(announced(&w), [PauseChanged { paused: false }.to_xdr(&w.env, &w.contract)]);
}

#[test]
fn test_limit_change_is_announced_with_previous_and_current_limits() {
    let w = world();
    let current = Limits { min_deposit: 2 * MIN_DEPOSIT, max_charge: USDC };
    admin_call(&w, &w.admin, "set_limits", (current.clone(),).into_val(&w.env)).unwrap();
    let expected = LimitsChanged {
        previous: Limits { min_deposit: MIN_DEPOSIT, max_charge: MAX_CHARGE },
        current: current.clone(),
    };
    assert_eq!(
        w.env.events().all().filter_by_contract(&w.contract),
        [expected.to_xdr(&w.env, &w.contract)]
    );
    assert_eq!(w.client().get_config().limits, current);
}

/// The events of the last invocation, as this contract emitted them, decoded
/// the way the chain observer decodes what `getEvents` returns.
fn observed(w: &World) -> std::vec::Vec<fermah_pay_stellar_chain::prepaid::LedgerEvent> {
    w.env
        .events()
        .all()
        .filter_by_contract(&w.contract)
        .events()
        .iter()
        .map(|event| {
            let xdr::ContractEventBody::V0(body) = &event.body;
            fermah_pay_stellar_chain::prepaid::ledger_event(&body.topics, &body.data)
                .unwrap_or_else(|| panic!("undecodable event {body:?}"))
        })
        .collect()
}

/// The chain observer matches every event the contract publishes against the
/// gateway's records, so each layout is pinned here against the real host.
#[test]
fn test_observer_decodes_every_event_the_contract_publishes() {
    use fermah_pay_stellar_chain::prepaid::{
        ChainAddress, ChargeEntry, ContractLimits, LedgerEvent, Outcome as Decoded, Role,
    };
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 1, 3 * USDC, 7).unwrap();
    let owner = ChainAddress::Account(buyer.key.address());
    assert_eq!(
        observed(&w),
        [LedgerEvent::Deposited { owner: owner.clone(), amount: 3 * USDC, deposit_id: id32(7) }]
    );

    let now = w.env.ledger().sequence();
    w.charge_batch(&[
        w.charge(1, 1, USDC),
        w.charge(1, 2, MAX_CHARGE + 1),
        w.charge(1, 1, USDC),
        w.charge_until(1, 3, USDC, now - 1),
        w.charge(1, 4, 5 * USDC),
        w.charge(2, 5, USDC),
    ])
    .unwrap();
    let stranger = ChainAddress::Account(w.owner_of(2));
    let entry = |owner: &ChainAddress, id, amount, outcome| ChargeEntry {
        owner: owner.clone(),
        charge_id: charge_id(id),
        amount,
        outcome,
    };
    assert_eq!(
        observed(&w),
        [LedgerEvent::Charges(std::vec![
            entry(&owner, 1, USDC, Decoded::Charged),
            entry(&owner, 2, MAX_CHARGE + 1, Decoded::AboveLimit),
            entry(&owner, 1, USDC, Decoded::Duplicate),
            entry(&owner, 3, USDC, Decoded::Expired),
            entry(&owner, 4, 5 * USDC, Decoded::InsufficientBalance),
            entry(&stranger, 5, USDC, Decoded::UnknownAccount),
        ])]
    );

    let destination = w.party(0);
    let intent = w.withdraw_intent(&buyer, USDC / 2, &destination, 8);
    let auths = [
        w.signed(&buyer, w.deployment().owner_withdraw_authorization(&intent)),
        w.signed(&w.treasury, w.deployment().treasury_withdraw_authorization(&intent)),
    ];
    withdraw_with(&w, &intent, &buyer, &destination, &auths).unwrap();
    assert_eq!(
        observed(&w),
        [LedgerEvent::Withdrawn {
            owner: owner.clone(),
            destination: ChainAddress::Account(destination.key.address()),
            amount: USDC / 2,
            withdrawal_id: id32(8),
        }]
    );

    withdraw_revenue(&w, USDC / 4, 9).unwrap();
    assert_eq!(
        observed(&w),
        [LedgerEvent::RevenueWithdrawn {
            destination: ChainAddress::Account(w.seller.key.address()),
            amount: USDC / 4,
            withdrawal_id: id32(9),
        }]
    );

    admin_call(&w, &w.admin, "pause", no_args(&w)).unwrap();
    assert_eq!(observed(&w), [LedgerEvent::PauseChanged { paused: true }]);
    admin_call(&w, &w.admin, "unpause", no_args(&w)).unwrap();
    assert_eq!(observed(&w), [LedgerEvent::PauseChanged { paused: false }]);
    let limits = Limits { min_deposit: 3, max_charge: 4 };
    admin_call(&w, &w.admin, "set_limits", (limits,).into_val(&w.env)).unwrap();
    assert_eq!(
        observed(&w),
        [LedgerEvent::LimitsChanged {
            previous: ContractLimits { min_deposit: MIN_DEPOSIT, max_charge: MAX_CHARGE },
            current: ContractLimits { min_deposit: 3, max_charge: 4 },
        }]
    );

    for (function, role) in [
        ("set_operator", Role::Operator),
        ("set_treasury", Role::Treasury),
        ("set_seller", Role::Seller),
        ("set_admin", Role::Admin),
    ] {
        let config = w.client().get_config();
        let previous = match role {
            Role::Operator => config.operator,
            Role::Treasury => config.treasury,
            Role::Seller => config.seller,
            Role::Admin => config.admin,
        };
        // The admin rotates last, so `w.admin` authorizes every rotation.
        let successor = w.party(0);
        signed_call(
            &w,
            &[&w.admin, &successor],
            function,
            (successor.address.clone(),).into_val(&w.env),
        )
        .unwrap();
        let account = |address: &Address| {
            let xdr::ScAddress::Account(account) = xdr::ScAddress::from(address) else {
                panic!("roles are classic accounts")
            };
            ChainAddress::Account(fermah_pay_stellar_chain::transaction::address_of(&account))
        };
        assert_eq!(
            observed(&w),
            [LedgerEvent::RoleChanged {
                role,
                previous: account(&previous),
                current: ChainAddress::Account(successor.key.address()),
            }],
            "{function}"
        );
    }
}

/// The observer reads the configuration and totals from the instance entry,
/// in one ledger-entry read, so that the treasury balance it compares them
/// with is read at the same ledger. The layout is pinned against the host.
#[test]
fn test_observer_reads_config_and_totals_from_the_instance_entry() {
    use fermah_pay_stellar_chain::prepaid::{InstanceState, Totals as Decoded, instance_state};
    let w = world();
    funded(&w, 1, 10 * USDC);
    w.charge_batch(&[w.charge(1, 1, 2 * USDC)]).unwrap();
    withdraw_revenue(&w, USDC / 2, 1).unwrap();

    let snapshot = w.env.to_ledger_snapshot();
    let key = w.deployment().instance_key();
    let entry = snapshot
        .ledger_entries
        .iter()
        .find(|(k, _)| **k == key)
        .map(|(_, (e, _))| e.data.clone())
        .expect("instance entry at the derived key");
    // Expected from the contract's own getters, and from arithmetic on the
    // calls above: 10 deposited, 2 charged, 0.5 of revenue paid out.
    assert_eq!(w.client().get_totals(), Totals { liabilities: 8 * USDC, revenue: 3 * USDC / 2 });
    assert_eq!(
        instance_state(&entry),
        Some(InstanceState {
            config: ContractConfig {
                admin: w.admin.key.address(),
                operator: w.operator.key.address(),
                seller: w.seller.key.address(),
                treasury: w.treasury.key.address(),
                usdc: contract_bytes(&w.usdc),
                paused: false,
            },
            totals: Decoded { liabilities: 8 * USDC, revenue: 3 * USDC / 2 },
        })
    );
    // Control: another contract's entry of the same kind is not read as it.
    let other = snapshot
        .ledger_entries
        .iter()
        .find(|(k, _)| {
            matches!(&**k, xdr::LedgerKey::ContractData(d)
                if d.key == xdr::ScVal::LedgerKeyContractInstance && **k != key)
        })
        .map(|(_, (e, _))| e.data.clone())
        .expect("the USDC contract's instance entry");
    assert_eq!(instance_state(&other), None);
}

#[test]
fn test_solvency_tooling_reads_get_totals() {
    let w = world();
    funded(&w, 1, 10 * USDC);
    w.charge_batch(&[w.charge(1, 1, 3 * USDC)]).unwrap();
    let val: Val = w.client().get_totals().into_val(&w.env);
    let encoded =
        <xdr::ScVal as soroban_sdk::TryFromVal<Env, Val>>::try_from_val(&w.env, &val).unwrap();
    assert_eq!(
        fermah_pay_stellar_chain::prepaid::contract_totals(&encoded),
        Some(fermah_pay_stellar_chain::prepaid::Totals {
            liabilities: 7 * USDC,
            revenue: 3 * USDC
        })
    );
}

mod properties;
