//! Prepaid ledger for one seller deployment.
//!
//! The contract records each buyer's prepaid credit and the seller's earned
//! revenue. It never holds USDC: deposits move USDC from the buyer to a
//! separate treasury account through the USDC Stellar Asset Contract, and
//! withdrawals move it back out of the treasury, each in the same invocation
//! that updates the ledger, so the transfer and the ledger change either both
//! happen or neither does.
//!
//! The treasury key can also move USDC without calling this contract. The
//! contract therefore cannot guarantee that the treasury holds at least the
//! recorded liabilities; it records the totals so that property can be
//! monitored.
//!
//! Accounts are keyed by the owner's address: one account per owner, which
//! only a deposit authorized by that owner can create. There is no
//! caller-chosen account identifier another party could claim first, and
//! deposit and withdrawal identifiers are scoped to their owner for the same
//! reason.
//!
//! Charge idempotency: every charge names its account and a per-account
//! sequence number, and the account entry stores the last sequence consumed.
//! A charge must carry exactly the next sequence; an already-consumed sequence
//! is a duplicate and never debits again. Keeping the replay state inside the
//! account entry makes a charge cost one ledger write, which is what lets one
//! transaction settle `MAX_BATCH` charges within the network's per-transaction
//! write limit.

#![no_std]

use soroban_sdk::{
    Address, BytesN, ContractExecutable, Env, Vec, contract, contracterror, contractevent,
    contractimpl, contracttype, panic_with_error, token,
};

/// Largest number of charges one `charge_batch` call accepts.
pub const MAX_BATCH: u32 = 100;

/// A classic trustline balance is a signed 64-bit integer, so a larger `i128`
/// amount can never be transferred to or from a `G...` account.
const MAX_TRANSFER: i128 = i64::MAX as i128;

// Live-until targets in ledgers (~5 s each): entries touched by a call are
// extended to about 30 days once fewer than about 7 days remain, so an active
// account cannot be archived between uses.
const TTL_THRESHOLD: u32 = 120_960;
const TTL_EXTEND_TO: u32 = 518_400;

/// Codes start at 101 so they never coincide with the USDC Stellar Asset
/// Contract's own error codes, which propagate through calls into this
/// contract: a refusal can always be attributed to the contract that made it.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    InvalidLimits = 101,
    Paused = 102,
    InvalidAmount = 103,
    BelowMinimumDeposit = 104,
    DepositAlreadyProcessed = 105,
    UnknownAccount = 107,
    InsufficientBalance = 108,
    ChargeAboveLimit = 109,
    DuplicateCharge = 110,
    OutOfOrderCharge = 111,
    EmptyBatch = 112,
    BatchTooLarge = 113,
    WithdrawalAlreadyProcessed = 114,
    InsufficientRevenue = 115,
    Overflow = 116,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Smallest accepted deposit, in USDC base units (7 decimals).
    pub min_deposit: i128,
    /// Largest accepted single charge, in USDC base units.
    pub max_charge: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub operator: Address,
    pub seller: Address,
    pub treasury: Address,
    pub usdc: Address,
    pub limits: Limits,
    pub paused: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Totals {
    /// Sum of all buyer balances.
    pub liabilities: i128,
    /// Charged amounts not yet withdrawn by the seller.
    pub revenue: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Account {
    pub balance: i128,
    /// Last consumed charge sequence; the next charge must carry this plus one.
    pub charge_seq: u64,
}

/// One charge: the account owner, the account's next sequence number, and
/// the amount.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Charge(pub Address, pub u64, pub i128);

/// Outcome of one charge in a batch. `Charged`, `InsufficientBalance` and
/// `AboveLimit` consume the sequence number, so retrying the same charge can
/// never debit; `Duplicate`, `OutOfOrder` and `UnknownAccount` consume nothing.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Outcome {
    Charged = 0,
    InsufficientBalance = 1,
    AboveLimit = 2,
    Duplicate = 3,
    OutOfOrder = 4,
    UnknownAccount = 5,
}

/// Per-charge result in an event: account owner, sequence, amount, outcome.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settled(pub Address, pub u64, pub i128, pub Outcome);

#[contracttype]
#[derive(Clone)]
enum Key {
    Config,
    Totals,
    Account(Address),
    Deposit(Address, BytesN<32>),
    Withdrawal(Address, BytesN<32>),
    RevenueWithdrawal(BytesN<32>),
}

#[contractevent(topics = ["deposit"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deposited {
    #[topic]
    pub owner: Address,
    pub amount: i128,
    pub deposit_id: BytesN<32>,
}

/// One event per charge call carrying every entry's outcome: a single event
/// avoids the fixed per-event overhead that would exceed the network's
/// per-transaction event size limit for a full batch.
#[contractevent(topics = ["charges"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Charges {
    pub settled: Vec<Settled>,
}

#[contractevent(topics = ["withdraw"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Withdrawn {
    #[topic]
    pub owner: Address,
    pub destination: Address,
    pub amount: i128,
    pub withdrawal_id: BytesN<32>,
}

#[contractevent(topics = ["revenue"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevenueWithdrawn {
    pub destination: Address,
    pub amount: i128,
    pub withdrawal_id: BytesN<32>,
}

#[contract]
pub struct PrepaidLedger;

#[contractimpl]
impl PrepaidLedger {
    /// Runs once, atomically with deployment, so no one can initialize the
    /// contract with other roles between deployment and set-up.
    pub fn __constructor(
        env: Env,
        admin: Address,
        operator: Address,
        seller: Address,
        treasury: Address,
        usdc: Address,
        limits: Limits,
    ) {
        validate_limits(&env, &limits);
        let config = Config { admin, operator, seller, treasury, usdc, limits, paused: false };
        env.storage().instance().set(&Key::Config, &config);
        env.storage().instance().set(&Key::Totals, &Totals { liabilities: 0, revenue: 0 });
    }

    /// Moves `amount` USDC from `owner` to the treasury and credits the
    /// owner's account, creating it on the first deposit.
    pub fn deposit(env: Env, owner: Address, amount: i128, deposit_id: BytesN<32>) {
        owner.require_auth();
        let config = active_config(&env);
        if amount <= 0 || amount > MAX_TRANSFER {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        if amount < config.limits.min_deposit {
            panic_with_error!(&env, Error::BelowMinimumDeposit);
        }
        let deposit_key = Key::Deposit(owner.clone(), deposit_id.clone());
        if env.storage().persistent().has(&deposit_key) {
            panic_with_error!(&env, Error::DepositAlreadyProcessed);
        }

        let account_key = Key::Account(owner.clone());
        let mut account = env
            .storage()
            .persistent()
            .get::<Key, Account>(&account_key)
            .unwrap_or(Account { balance: 0, charge_seq: 0 });
        account.balance = checked_add(&env, account.balance, amount);
        let mut totals = totals(&env);
        totals.liabilities = checked_add(&env, totals.liabilities, amount);

        put_persistent(&env, &account_key, &account);
        put_persistent(&env, &deposit_key, &());
        env.storage().instance().set(&Key::Totals, &totals);
        extend_instance(&env);

        token::Client::new(&env, &config.usdc).transfer(&owner, &config.treasury, &amount);
        Deposited { owner, amount, deposit_id }.publish(&env);
    }

    /// Settles one charge; any refusal reverts the call with its reason.
    pub fn charge(env: Env, charge: Charge) {
        let config = active_config(&env);
        config.operator.require_auth();
        let mut totals = totals(&env);
        let settled = settle(&env, &config, &mut totals, charge);
        match settled.3 {
            Outcome::Charged => {}
            Outcome::InsufficientBalance => panic_with_error!(&env, Error::InsufficientBalance),
            Outcome::AboveLimit => panic_with_error!(&env, Error::ChargeAboveLimit),
            Outcome::Duplicate => panic_with_error!(&env, Error::DuplicateCharge),
            Outcome::OutOfOrder => panic_with_error!(&env, Error::OutOfOrderCharge),
            Outcome::UnknownAccount => panic_with_error!(&env, Error::UnknownAccount),
        }
        env.storage().instance().set(&Key::Totals, &totals);
        extend_instance(&env);
        Charges { settled: Vec::from_array(&env, [settled]) }.publish(&env);
    }

    /// Settles up to `MAX_BATCH` charges in one call and returns each entry's
    /// outcome. A refused entry does not affect the others; only a malformed
    /// batch (empty or too large) reverts the whole call.
    pub fn charge_batch(env: Env, charges: Vec<Charge>) -> Vec<Outcome> {
        let config = active_config(&env);
        config.operator.require_auth();
        if charges.is_empty() {
            panic_with_error!(&env, Error::EmptyBatch);
        }
        if charges.len() > MAX_BATCH {
            panic_with_error!(&env, Error::BatchTooLarge);
        }
        let mut totals = totals(&env);
        let mut settled = Vec::new(&env);
        let mut outcomes = Vec::new(&env);
        for charge in charges.iter() {
            let result = settle(&env, &config, &mut totals, charge);
            outcomes.push_back(result.3);
            settled.push_back(result);
        }
        env.storage().instance().set(&Key::Totals, &totals);
        extend_instance(&env);
        Charges { settled }.publish(&env);
        outcomes
    }

    /// Returns unused credit to `destination`. Needs the owner's authorization
    /// for the exact amount and destination, and the treasury's, because the
    /// USDC leaves the treasury account.
    pub fn withdraw(
        env: Env,
        owner: Address,
        amount: i128,
        destination: Address,
        withdrawal_id: BytesN<32>,
    ) {
        owner.require_auth();
        let config = active_config(&env);
        config.treasury.require_auth();
        if amount <= 0 || amount > MAX_TRANSFER {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        let withdrawal_key = Key::Withdrawal(owner.clone(), withdrawal_id.clone());
        if env.storage().persistent().has(&withdrawal_key) {
            panic_with_error!(&env, Error::WithdrawalAlreadyProcessed);
        }
        let account_key = Key::Account(owner.clone());
        let mut account: Account = env
            .storage()
            .persistent()
            .get(&account_key)
            .unwrap_or_else(|| panic_with_error!(&env, Error::UnknownAccount));
        if account.balance < amount {
            panic_with_error!(&env, Error::InsufficientBalance);
        }
        account.balance -= amount;
        let mut totals = totals(&env);
        totals.liabilities -= amount;

        put_persistent(&env, &account_key, &account);
        put_persistent(&env, &withdrawal_key, &());
        env.storage().instance().set(&Key::Totals, &totals);
        extend_instance(&env);

        token::Client::new(&env, &config.usdc).transfer(&config.treasury, &destination, &amount);
        Withdrawn { owner, destination, amount, withdrawal_id }.publish(&env);
    }

    /// Pays earned revenue out of the treasury. Needs the seller's and the
    /// treasury's authorization.
    pub fn withdraw_revenue(
        env: Env,
        destination: Address,
        amount: i128,
        withdrawal_id: BytesN<32>,
    ) {
        let config = active_config(&env);
        config.seller.require_auth();
        config.treasury.require_auth();
        if amount <= 0 || amount > MAX_TRANSFER {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        let withdrawal_key = Key::RevenueWithdrawal(withdrawal_id.clone());
        if env.storage().persistent().has(&withdrawal_key) {
            panic_with_error!(&env, Error::WithdrawalAlreadyProcessed);
        }
        let mut totals = totals(&env);
        if totals.revenue < amount {
            panic_with_error!(&env, Error::InsufficientRevenue);
        }
        totals.revenue -= amount;

        put_persistent(&env, &withdrawal_key, &());
        env.storage().instance().set(&Key::Totals, &totals);
        extend_instance(&env);

        token::Client::new(&env, &config.usdc).transfer(&config.treasury, &destination, &amount);
        RevenueWithdrawn { destination, amount, withdrawal_id }.publish(&env);
    }

    pub fn get_balance(env: Env, owner: Address) -> i128 {
        env.storage()
            .persistent()
            .get::<Key, Account>(&Key::Account(owner))
            .map_or(0, |account| account.balance)
    }

    pub fn get_account(env: Env, owner: Address) -> Option<Account> {
        env.storage().persistent().get(&Key::Account(owner))
    }

    pub fn get_config(env: Env) -> Config {
        config(&env)
    }

    pub fn get_totals(env: Env) -> Totals {
        totals(&env)
    }

    /// Stops deposits, charges and withdrawals until `unpause`.
    pub fn pause(env: Env) {
        let mut config = config(&env);
        config.admin.require_auth();
        config.paused = true;
        env.storage().instance().set(&Key::Config, &config);
    }

    pub fn unpause(env: Env) {
        let mut config = config(&env);
        config.admin.require_auth();
        config.paused = false;
        env.storage().instance().set(&Key::Config, &config);
    }

    pub fn set_operator(env: Env, operator: Address) {
        let mut config = config(&env);
        config.admin.require_auth();
        config.operator = operator;
        env.storage().instance().set(&Key::Config, &config);
    }

    pub fn set_limits(env: Env, limits: Limits) {
        let mut config = config(&env);
        config.admin.require_auth();
        validate_limits(&env, &limits);
        config.limits = limits;
        env.storage().instance().set(&Key::Config, &config);
    }

    /// Replaces the contract code. The admin can change what every balance
    /// means through this, which is why it is a separate key from the
    /// operator and the treasury.
    pub fn upgrade(env: Env, wasm_hash: BytesN<32>) {
        config(&env).admin.require_auth();
        env.deployer().update_current_contract(ContractExecutable::Wasm(wasm_hash));
    }
}

fn settle(env: &Env, config: &Config, totals: &mut Totals, charge: Charge) -> Settled {
    let Charge(owner, seq, amount) = charge;
    let key = Key::Account(owner.clone());
    let outcome = match env.storage().persistent().get::<Key, Account>(&key) {
        None => Outcome::UnknownAccount,
        Some(account) if seq <= account.charge_seq => Outcome::Duplicate,
        Some(account) if seq != account.charge_seq + 1 => Outcome::OutOfOrder,
        Some(mut account) => {
            // A non-positive amount is a malformed request, not a policy
            // refusal a retry could change: reject the whole call.
            if amount <= 0 {
                panic_with_error!(env, Error::InvalidAmount);
            }
            let outcome = if amount > config.limits.max_charge {
                Outcome::AboveLimit
            } else if amount > account.balance {
                Outcome::InsufficientBalance
            } else {
                account.balance -= amount;
                totals.liabilities -= amount;
                totals.revenue = checked_add(env, totals.revenue, amount);
                Outcome::Charged
            };
            account.charge_seq = seq;
            put_persistent(env, &key, &account);
            outcome
        }
    };
    Settled(owner, seq, amount, outcome)
}

fn config(env: &Env) -> Config {
    env.storage().instance().get(&Key::Config).expect("config is set by the constructor")
}

fn active_config(env: &Env) -> Config {
    let config = config(env);
    if config.paused {
        panic_with_error!(env, Error::Paused);
    }
    config
}

fn totals(env: &Env) -> Totals {
    env.storage().instance().get(&Key::Totals).expect("totals are set by the constructor")
}

fn validate_limits(env: &Env, limits: &Limits) {
    if limits.min_deposit <= 0 || limits.max_charge <= 0 {
        panic_with_error!(env, Error::InvalidLimits);
    }
}

fn checked_add(env: &Env, a: i128, b: i128) -> i128 {
    a.checked_add(b).unwrap_or_else(|| panic_with_error!(env, Error::Overflow))
}

fn put_persistent<V: soroban_sdk::IntoVal<Env, soroban_sdk::Val>>(env: &Env, key: &Key, value: &V) {
    env.storage().persistent().set(key, value);
    env.storage().persistent().extend_ttl(key, TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn extend_instance(env: &Env) {
    env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

#[cfg(test)]
mod test;
