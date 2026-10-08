//! Prepaid vault for one seller deployment.
//!
//! The contract holds the buyers' USDC itself and keeps the ledger of each
//! buyer's credit and the seller's earned revenue. Only this contract's code
//! can move that USDC: the USDC contract accepts a spend from a contract
//! address only when that contract is the direct invoker, and this contract
//! exports no `__check_auth`. Every payout debits the ledger in the same
//! invocation as the transfer, so the USDC it holds always covers
//! `liabilities + revenue`; only the USDC issuer, which can freeze or (if it
//! ever enables it) claw back a balance, could break that.
//!
//! Two guarantees follow from the rules below.
//!
//! For the buyer: no key the operator or the admin holds can move a buyer's
//! credit except within a daily spending limit the buyer signed, and a buyer
//! can always leave without the operator. Charges need the operator and stay
//! within the buyer's limit; a withdrawal needs the buyer; an exit needs only
//! the buyer and a notice period; the admin's one power over funds, replacing
//! the code, waits a timelock longer than that notice, and pausing never
//! blocks an exit.
//!
//! For the seller: every prepaid charge admitted before a buyer acts alone
//! can still settle. A charge names the last ledger in which it may settle,
//! at most `MAX_CHARGE_WINDOW` ahead, and the UTC day it was admitted in,
//! whose share of the buyer's limit it counts against whenever it settles,
//! so a charge admitted within the limit is not refused for settling after
//! midnight. What a buyer does alone that reduces
//! what can be charged, lowering the limit or exiting, takes effect only
//! `NOTICE_LEDGERS` later, which is longer than that window: by then every
//! charge admitted before it has settled or expired. Raising the limit and
//! depositing take effect at once, since they can only help the seller.
//!
//! Every delay is a number of ledgers, compared with the ledger sequence, as
//! a charge's last ledger is. Only the daily limits' day is ledger time.
//!
//! Charge idempotency, batching and recurring mandates work as in the
//! prepaid ledger contract: every charge carries an identifier recorded with
//! its outcome in temporary storage until shortly after its last ledger, so
//! it can never debit twice; a recurring charge moves USDC from the buyer's
//! wallet into this contract as revenue under an allowance the buyer
//! approved with the mandate.

#![no_std]

use soroban_sdk::{
    Address, BytesN, ContractExecutable, Env, Symbol, Vec, contract, contracterror, contractevent,
    contractimpl, contracttype, panic_with_error, symbol_short, token, xdr::ScErrorType,
};

/// Largest number of charges one `charge_batch` call accepts: each charge of
/// a distinct buyer writes its account and its record, 196 entries plus the
/// instance and the operator's nonce, against the network's 200 writes.
pub const MAX_BATCH: u32 = 98;

/// Largest number of recurring charges one `charge_recurring_batch` call
/// accepts, bounded by the network's 16,384 bytes of events per transaction.
pub const MAX_RECURRING_BATCH: u32 = 35;

/// Furthest ahead, in ledgers (~1 day), a charge's last ledger may be.
pub const MAX_CHARGE_WINDOW: u32 = 17_280;

/// Ledgers (~1 hour) a charge record outlives the charge's last ledger.
pub const CHARGE_RECORD_GRACE: u32 = 720;

/// Ledgers (~26 hours) between a buyer's request to lower their limit or to
/// exit and the moment it takes effect.
pub const NOTICE_LEDGERS: u32 = 18_720;

/// Ledgers (~7 days) between proposing new code and installing it.
pub const UPGRADE_DELAY_LEDGERS: u32 = 120_960;

/// Ledgers (~3 days) after the delay in which proposed code may be
/// installed; after that the proposal has lapsed and a new one restarts the
/// delay. A buyer who joins after a proposal's delay ran out is therefore
/// never exposed to it for long, and the proposal stays visible to all.
pub const UPGRADE_WINDOW_LEDGERS: u32 = 51_840;

/// Ledgers (~5 days) a buyer has after an upgrade is proposed to request an
/// exit that unlocks before the new code can run.
pub const EXIT_MARGIN_LEDGERS: u32 = 86_400;

// The seller guarantee: a charge admitted at or before the ledger a buyer
// acts in names a last ledger at most `MAX_CHARGE_WINDOW` later, so it can no
// longer settle once the notice has passed.
const _: () = assert!(NOTICE_LEDGERS > MAX_CHARGE_WINDOW + CHARGE_RECORD_GRACE);
// The buyer guarantee against an upgrade: a buyer who requests an exit
// within `EXIT_MARGIN_LEDGERS` of a proposal can exit before it is installed.
const _: () = assert!(UPGRADE_DELAY_LEDGERS > NOTICE_LEDGERS + EXIT_MARGIN_LEDGERS);

/// A classic trustline balance is a signed 64-bit integer, so a larger `i128`
/// amount can never be transferred to or from a `G...` account.
const MAX_TRANSFER: i128 = i64::MAX as i128;

// Entries touched by a call are extended to about 30 days once fewer than
// about 7 days remain, so an active account cannot be archived between uses.
const TTL_THRESHOLD: u32 = 120_960;
const TTL_EXTEND_TO: u32 = 518_400;

/// Codes start at 101 so they never coincide with the USDC Stellar Asset
/// Contract's own codes, which propagate through calls into this contract.
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
    ChargeExpired = 111,
    EmptyBatch = 112,
    BatchTooLarge = 113,
    WithdrawalAlreadyProcessed = 114,
    InsufficientRevenue = 115,
    Overflow = 116,
    /// Two roles would share one address, or a rotation names the current
    /// holder of the role.
    DuplicateRole = 117,
    ChargeWindowTooLong = 118,
    ChargeAboveDailyLimit = 119,
    InvalidMandate = 120,
    /// A spending limit below zero.
    InvalidCap = 121,
    /// The charge would take the buyer past the limit they signed.
    ChargeAboveCap = 122,
    /// The deposit would take the buyer's balance, or all balances, past a
    /// launch limit.
    AboveLaunchLimit = 123,
    /// `exit` without a request.
    NoExit = 124,
    /// `exit` before the request's notice has passed.
    ExitLocked = 125,
    /// A payout to this contract itself.
    InvalidDestination = 126,
    /// `upgrade` or `cancel_upgrade` without a proposal.
    NoUpgrade = 127,
    /// `upgrade` before the proposal's delay has passed.
    UpgradeLocked = 128,
    /// A charge names a day after today.
    InvalidDay = 129,
    /// `exit` when nothing is left to pay; the request stays.
    NothingToExit = 130,
    /// The proposed code was not installed within its window.
    UpgradeLapsed = 131,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Smallest accepted deposit, in USDC base units (7 decimals).
    pub min_deposit: i128,
    /// Largest accepted single charge, in USDC base units.
    pub max_charge: i128,
}

/// Bounds on deposits while the contract is new: the largest balance one
/// buyer may hold and the largest total all buyers may hold. They only
/// refuse deposits, so the admin changes them at once.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchLimits {
    pub max_balance: i128,
    pub max_total: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub operator: Address,
    pub seller: Address,
    pub usdc: Address,
    pub limits: Limits,
    pub paused: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Totals {
    /// Sum of all buyer balances.
    pub liabilities: i128,
    /// Charged amounts not yet paid out to the seller.
    pub revenue: i128,
}

/// A lower spending limit the buyer asked for, in force from ledger
/// `effective_at`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingCap {
    pub cap: i128,
    pub effective_at: u32,
}

/// A buyer's request to take `amount` to `destination` without the operator,
/// payable from ledger `unlock_at`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExitRequest {
    pub amount: i128,
    pub destination: Address,
    pub unlock_at: u32,
}

/// A lower limit waiting for its notice, if any.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapChange {
    None,
    Pending(PendingCap),
}

/// An exit waiting for its notice, if any.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Exit {
    None,
    Requested(ExitRequest),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Account {
    pub balance: i128,
    /// The most the buyer lets the seller charge per UTC day of ledger time.
    /// Zero until the buyer signs one: such an account cannot be charged.
    pub cap: i128,
    /// The latest UTC day (ledger time / 86400) charges were counted for,
    /// and what they came to.
    pub day: u64,
    pub charged: i128,
    /// The day before `day` that charges were counted for, if any, and what
    /// they came to: a charge admitted late in a day may settle the next.
    pub prev_day: u64,
    pub prev_charged: i128,
    pub pending_cap: CapChange,
    pub exit: Exit,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DailyLimits {
    pub per_buyer: i128,
    pub per_seller: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DayCount {
    pub day: u64,
    pub charged: i128,
}

/// Code proposed by the admin, installable from ledger `effective_at`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingUpgrade {
    pub wasm_hash: BytesN<32>,
    pub effective_at: u32,
}

/// One charge: the account owner, the seller's identifier for the charge,
/// the amount, the last ledger in which it may be settled, and the UTC day
/// of ledger time it was admitted in, which its amount counts against
/// whenever it settles. Only today or yesterday may be named.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Charge(pub Address, pub BytesN<32>, pub i128, pub u32, pub u64);

/// Outcome of one charge. Every outcome but `Duplicate` and `Expired`
/// records the charge's identifier with that outcome.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Outcome {
    Charged = 0,
    InsufficientBalance = 1,
    AboveLimit = 2,
    Duplicate = 3,
    Expired = 4,
    UnknownAccount = 5,
    AboveDailyLimit = 6,
    /// The charge would pass the spending limit the buyer signed.
    AboveCap = 7,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settled(pub Address, pub BytesN<32>, pub i128, pub Outcome);

/// As in the prepaid ledger contract.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mandate {
    pub mandate_id: BytesN<32>,
    pub amount: i128,
    pub period_secs: u64,
    pub start: u64,
    pub cycles: u32,
    pub live_until: u32,
    pub next_cycle: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecurringCharge {
    pub owner: Address,
    pub charge_id: BytesN<32>,
    pub mandate_id: BytesN<32>,
    pub cycle: u32,
    pub amount: i128,
    pub last_ledger: u32,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum RecurringOutcome {
    Charged = 0,
    Duplicate = 1,
    Expired = 2,
    NoMandate = 3,
    MandateExpired = 4,
    AlreadyCharged = 5,
    NotDue = 6,
    PeriodOver = 7,
    AboveMandate = 8,
    AboveLimit = 9,
    AboveDailyLimit = 10,
    AllowanceShort = 11,
    WalletShort = 12,
    TransferRefused = 13,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecurringSettled(
    pub Address,
    pub BytesN<32>,
    pub BytesN<32>,
    pub u32,
    pub i128,
    pub RecurringOutcome,
);

#[contracttype]
#[derive(Clone)]
enum Key {
    Config,
    Totals,
    Account(Address),
    Deposit(Address, BytesN<32>),
    Withdrawal(Address, BytesN<32>),
    RevenueWithdrawal(BytesN<32>),
    /// Temporary: a settled charge's outcome.
    Charge(Address, BytesN<32>),
    DailyLimits,
    SellerDay,
    Mandate(Address),
    /// Temporary: a settled recurring charge's outcome.
    Recurring(Address, BytesN<32>),
    LaunchLimits,
    PendingUpgrade,
}

#[contractevent(topics = ["deposit"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deposited {
    #[topic]
    pub owner: Address,
    pub amount: i128,
    pub deposit_id: BytesN<32>,
}

/// The buyer's limit rose, or was confirmed, in force at once; any pending
/// lower limit is dropped.
#[contractevent(topics = ["cap_raised"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapRaised {
    #[topic]
    pub owner: Address,
    pub cap: i128,
}

/// The buyer asked for a lower limit, in force from `effective_at`.
#[contractevent(topics = ["cap_lowered"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapLowered {
    #[topic]
    pub owner: Address,
    pub cap: i128,
    pub effective_at: u32,
}

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

#[contractevent(topics = ["exit_requested"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExitRequested {
    #[topic]
    pub owner: Address,
    pub amount: i128,
    pub destination: Address,
    pub unlock_at: u32,
}

/// An exit was paid; `amount` is what the balance allowed, possibly less
/// than requested.
#[contractevent(topics = ["exit"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Exited {
    #[topic]
    pub owner: Address,
    pub destination: Address,
    pub amount: i128,
}

#[contractevent(topics = ["role"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleChanged {
    #[topic]
    pub role: Symbol,
    pub previous: Address,
    pub current: Address,
}

#[contractevent(topics = ["pause"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseChanged {
    pub paused: bool,
}

#[contractevent(topics = ["limits"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitsChanged {
    pub previous: Limits,
    pub current: Limits,
}

#[contractevent(topics = ["daily"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DailyLimitsChanged {
    pub previous: Option<DailyLimits>,
    pub current: DailyLimits,
}

#[contractevent(topics = ["launch"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchLimitsChanged {
    pub previous: Option<LaunchLimits>,
    pub current: Option<LaunchLimits>,
}

#[contractevent(topics = ["upgrade_proposed"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeProposed {
    pub wasm_hash: BytesN<32>,
    pub effective_at: u32,
}

#[contractevent(topics = ["upgrade_cancelled"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeCancelled {
    pub wasm_hash: BytesN<32>,
}

/// The proposed code was installed; it runs from the next invocation.
#[contractevent(topics = ["upgrade_installed"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeInstalled {
    pub wasm_hash: BytesN<32>,
}

#[contractevent(topics = ["mandate"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MandateAuthorized {
    #[topic]
    pub owner: Address,
    pub mandate: Mandate,
}

#[contractevent(topics = ["revoke"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MandateRevoked {
    #[topic]
    pub owner: Address,
    pub mandate_id: Option<BytesN<32>>,
}

#[contractevent(topics = ["recurring"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecurringCharges {
    pub settled: Vec<RecurringSettled>,
}

#[contractevent(topics = ["revenue"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevenueWithdrawn {
    pub destination: Address,
    pub amount: i128,
    pub withdrawal_id: BytesN<32>,
}

#[contract]
pub struct PrepaidVault;

#[contractimpl]
impl PrepaidVault {
    /// Runs once, atomically with deployment. Admin, operator and seller
    /// must be three distinct addresses.
    pub fn __constructor(
        env: Env,
        admin: Address,
        operator: Address,
        seller: Address,
        usdc: Address,
        limits: Limits,
    ) {
        validate_limits(&env, &limits);
        let config = Config { admin, operator, seller, usdc, limits, paused: false };
        require_distinct_roles(&env, &config);
        env.storage().instance().set(&Key::Config, &config);
        env.storage().instance().set(&Key::Totals, &Totals { liabilities: 0, revenue: 0 });
        extend_instance(&env);
    }

    /// Moves `amount` USDC from `owner` into this contract and credits the
    /// owner's account, creating it on the first deposit. A `cap`, if given,
    /// is applied as by `set_cap`, so the first deposit and the spending
    /// limit take one signature.
    pub fn deposit(
        env: Env,
        owner: Address,
        amount: i128,
        deposit_id: BytesN<32>,
        cap: Option<i128>,
    ) {
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
        let mut account = load_account(&env, &account_key).unwrap_or(Account {
            balance: 0,
            cap: 0,
            day: 0,
            charged: 0,
            prev_day: 0,
            prev_charged: 0,
            pending_cap: CapChange::None,
            exit: Exit::None,
        });
        account.balance = checked_add(&env, account.balance, amount);
        let mut totals = totals(&env);
        totals.liabilities = checked_add(&env, totals.liabilities, amount);
        if let Some(launch) = env.storage().instance().get::<Key, LaunchLimits>(&Key::LaunchLimits)
            && (account.balance > launch.max_balance || totals.liabilities > launch.max_total)
        {
            panic_with_error!(&env, Error::AboveLaunchLimit);
        }
        if let Some(cap) = cap {
            change_cap(&env, &owner, &mut account, cap);
        }

        store_account(&env, &account_key, account);
        put_persistent(&env, &deposit_key, &());
        env.storage().instance().set(&Key::Totals, &totals);
        extend_instance(&env);

        token::Client::new(&env, &config.usdc).transfer(
            &owner,
            env.current_contract_address(),
            &amount,
        );
        Deposited { owner, amount, deposit_id }.publish(&env);
    }

    /// Sets the most the seller may charge `owner` per day. A limit at least
    /// the one in force applies at once and drops any pending lower one; a
    /// lower limit applies `NOTICE_LEDGERS` from now, replacing any pending
    /// one and restarting its notice. Allowed while paused.
    pub fn set_cap(env: Env, owner: Address, cap: i128) {
        owner.require_auth();
        let key = Key::Account(owner.clone());
        let mut account = load_account(&env, &key)
            .unwrap_or_else(|| panic_with_error!(&env, Error::UnknownAccount));
        change_cap(&env, &owner, &mut account, cap);
        store_account(&env, &key, account);
        extend_instance(&env);
    }

    /// Settles one charge; any refusal reverts the call with its reason.
    pub fn charge(env: Env, charge: Charge) {
        let config = active_config(&env);
        config.operator.require_auth();
        let mut totals = totals(&env);
        let mut day = Charging::load(&env);
        let settled = settle(&env, &config, &mut totals, &mut day, charge);
        match settled.3 {
            Outcome::Charged => {}
            Outcome::InsufficientBalance => panic_with_error!(&env, Error::InsufficientBalance),
            Outcome::AboveLimit => panic_with_error!(&env, Error::ChargeAboveLimit),
            Outcome::Duplicate => panic_with_error!(&env, Error::DuplicateCharge),
            Outcome::Expired => panic_with_error!(&env, Error::ChargeExpired),
            Outcome::UnknownAccount => panic_with_error!(&env, Error::UnknownAccount),
            Outcome::AboveDailyLimit => panic_with_error!(&env, Error::ChargeAboveDailyLimit),
            Outcome::AboveCap => panic_with_error!(&env, Error::ChargeAboveCap),
        }
        env.storage().instance().set(&Key::Totals, &totals);
        day.store(&env);
        extend_instance(&env);
        Charges { settled: Vec::from_array(&env, [settled]) }.publish(&env);
    }

    /// Settles up to `MAX_BATCH` charges and returns each entry's outcome.
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
        let mut day = Charging::load(&env);
        let mut settled = Vec::new(&env);
        let mut outcomes = Vec::new(&env);
        for charge in charges.iter() {
            let result = settle(&env, &config, &mut totals, &mut day, charge);
            outcomes.push_back(result.3);
            settled.push_back(result);
        }
        env.storage().instance().set(&Key::Totals, &totals);
        day.store(&env);
        extend_instance(&env);
        Charges { settled }.publish(&env);
        outcomes
    }

    /// Pays unused credit to `destination` at once. Needs the owner, for the
    /// amount and destination, and the operator, whose gateway has first
    /// held the charges it already admitted for this buyer. Allowed while
    /// paused: it needs the buyer's signature.
    pub fn withdraw(
        env: Env,
        owner: Address,
        amount: i128,
        destination: Address,
        withdrawal_id: BytesN<32>,
    ) {
        owner.require_auth();
        let config = config(&env);
        // One address authorizes a frame once: an owner that is also the
        // operator has already authorized this call.
        if owner != config.operator {
            config.operator.require_auth();
        }
        if amount <= 0 || amount > MAX_TRANSFER {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        require_payable(&env, &config, &destination);
        let withdrawal_key = Key::Withdrawal(owner.clone(), withdrawal_id.clone());
        if env.storage().persistent().has(&withdrawal_key) {
            panic_with_error!(&env, Error::WithdrawalAlreadyProcessed);
        }
        let account_key = Key::Account(owner.clone());
        let mut account = load_account(&env, &account_key)
            .unwrap_or_else(|| panic_with_error!(&env, Error::UnknownAccount));
        if account.balance < amount {
            panic_with_error!(&env, Error::InsufficientBalance);
        }
        account.balance -= amount;
        let mut totals = totals(&env);
        totals.liabilities -= amount;

        store_account(&env, &account_key, account);
        put_persistent(&env, &withdrawal_key, &());
        env.storage().instance().set(&Key::Totals, &totals);
        extend_instance(&env);

        token::Client::new(&env, &config.usdc).transfer(
            &env.current_contract_address(),
            &destination,
            &amount,
        );
        Withdrawn { owner, destination, amount, withdrawal_id }.publish(&env);
    }

    /// Records `owner`'s request to take `amount` to `destination` without
    /// the operator, payable `NOTICE_LEDGERS` from now. A new request
    /// replaces the previous one: one for no more than the pending amount
    /// keeps its unlock ledger, which lets a buyer change the destination,
    /// for instance after the first one stopped accepting USDC; a larger one
    /// restarts the notice. Allowed while paused. Only the owner can create
    /// or replace a request.
    pub fn request_exit(env: Env, owner: Address, amount: i128, destination: Address) {
        owner.require_auth();
        if amount <= 0 || amount > MAX_TRANSFER {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        require_payable(&env, &config(&env), &destination);
        let key = Key::Account(owner.clone());
        let mut account = load_account(&env, &key)
            .unwrap_or_else(|| panic_with_error!(&env, Error::UnknownAccount));
        let unlock_at = match &account.exit {
            Exit::Requested(pending) if amount <= pending.amount => pending.unlock_at,
            _ => checked_ledger(&env, NOTICE_LEDGERS),
        };
        account.exit =
            Exit::Requested(ExitRequest { amount, destination: destination.clone(), unlock_at });
        store_account(&env, &key, account);
        extend_instance(&env);
        ExitRequested { owner, amount, destination, unlock_at }.publish(&env);
    }

    /// Pays `owner`'s exit request once its notice has passed: the requested
    /// amount, or the whole balance if less is left, to the destination the
    /// owner signed. Anyone may send it, since it can pay nowhere else: a
    /// buyer holding no XLM can have any wallet or relayer send it. Allowed
    /// while paused.
    pub fn exit(env: Env, owner: Address) {
        let config = config(&env);
        let key = Key::Account(owner.clone());
        let mut account = load_account(&env, &key)
            .unwrap_or_else(|| panic_with_error!(&env, Error::UnknownAccount));
        let Exit::Requested(request) = core::mem::replace(&mut account.exit, Exit::None) else {
            panic_with_error!(&env, Error::NoExit);
        };
        if env.ledger().sequence() < request.unlock_at {
            panic_with_error!(&env, Error::ExitLocked);
        }
        let amount = request.amount.min(account.balance);
        if amount == 0 {
            panic_with_error!(&env, Error::NothingToExit);
        }
        account.balance -= amount;
        let mut totals = totals(&env);
        totals.liabilities -= amount;
        store_account(&env, &key, account);
        env.storage().instance().set(&Key::Totals, &totals);
        extend_instance(&env);
        token::Client::new(&env, &config.usdc).transfer(
            &env.current_contract_address(),
            &request.destination,
            &amount,
        );
        Exited { owner, destination: request.destination, amount }.publish(&env);
    }

    /// Pays earned revenue to `destination`. Needs the seller alone. Allowed
    /// while paused.
    pub fn withdraw_revenue(
        env: Env,
        destination: Address,
        amount: i128,
        withdrawal_id: BytesN<32>,
    ) {
        let config = config(&env);
        config.seller.require_auth();
        if amount <= 0 || amount > MAX_TRANSFER {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        require_payable(&env, &config, &destination);
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

        token::Client::new(&env, &config.usdc).transfer(
            &env.current_contract_address(),
            &destination,
            &amount,
        );
        RevenueWithdrawn { destination, amount, withdrawal_id }.publish(&env);
    }

    /// As in the prepaid ledger contract: records the mandate and approves
    /// this contract, in the USDC contract, for its total until `live_until`.
    pub fn authorize_recurring(
        env: Env,
        owner: Address,
        mandate_id: BytesN<32>,
        amount: i128,
        period_secs: u64,
        cycles: u32,
        live_until: u32,
    ) {
        owner.require_auth();
        let config = active_config(&env);
        if amount <= 0 || amount > MAX_TRANSFER {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        if period_secs == 0 || cycles == 0 || live_until < env.ledger().sequence() {
            panic_with_error!(&env, Error::InvalidMandate);
        }
        if amount > config.limits.max_charge {
            panic_with_error!(&env, Error::InvalidMandate);
        }
        let key = Key::Mandate(owner.clone());
        let current: Option<Mandate> = env.storage().persistent().get(&key);
        if current.is_some_and(|current| current.mandate_id == mandate_id) {
            panic_with_error!(&env, Error::InvalidMandate);
        }
        let allowance = checked_mul(&env, amount, i128::from(cycles));
        // While launch limits bound what a buyer may hold, they bound what a
        // buyer's wallet lets this contract take too: code installed by an
        // upgrade could spend the allowance, not only the deposits.
        if let Some(launch) = env.storage().instance().get::<Key, LaunchLimits>(&Key::LaunchLimits)
            && allowance > launch.max_balance
        {
            panic_with_error!(&env, Error::AboveLaunchLimit);
        }
        let mandate = Mandate {
            mandate_id,
            amount,
            period_secs,
            start: env.ledger().timestamp(),
            cycles,
            live_until,
            next_cycle: 0,
        };
        put_persistent(&env, &key, &mandate);
        let lifetime = live_until.saturating_sub(env.ledger().sequence());
        if lifetime > TTL_EXTEND_TO {
            env.storage().persistent().extend_ttl(&key, lifetime, lifetime);
        }
        extend_instance(&env);
        token::Client::new(&env, &config.usdc).approve(
            &owner,
            &env.current_contract_address(),
            &allowance,
            &live_until,
        );
        MandateAuthorized { owner, mandate }.publish(&env);
    }

    /// Ends `owner`'s mandate and sets its allowance to zero. Allowed while
    /// paused.
    pub fn revoke_recurring(env: Env, owner: Address) {
        owner.require_auth();
        let config = config(&env);
        let key = Key::Mandate(owner.clone());
        let mandate_id = env.storage().persistent().get::<Key, Mandate>(&key).map(|m| m.mandate_id);
        env.storage().persistent().remove(&key);
        extend_instance(&env);
        token::Client::new(&env, &config.usdc).approve(
            &owner,
            &env.current_contract_address(),
            &0,
            &0,
        );
        MandateRevoked { owner, mandate_id }.publish(&env);
    }

    /// Settles up to `MAX_RECURRING_BATCH` recurring charges; each moves USDC
    /// from the buyer's wallet into this contract as revenue.
    pub fn charge_recurring_batch(
        env: Env,
        charges: Vec<RecurringCharge>,
    ) -> Vec<RecurringOutcome> {
        let config = active_config(&env);
        config.operator.require_auth();
        if charges.is_empty() {
            panic_with_error!(&env, Error::EmptyBatch);
        }
        if charges.len() > MAX_RECURRING_BATCH {
            panic_with_error!(&env, Error::BatchTooLarge);
        }
        let mut totals = totals(&env);
        let mut day = Charging::load(&env);
        let usdc = token::Client::new(&env, &config.usdc);
        let mut settled = Vec::new(&env);
        let mut outcomes = Vec::new(&env);
        for charge in charges.iter() {
            let outcome = settle_recurring(&env, &config, &usdc, &mut totals, &mut day, &charge);
            outcomes.push_back(outcome);
            settled.push_back(RecurringSettled(
                charge.owner,
                charge.charge_id,
                charge.mandate_id,
                charge.cycle,
                charge.amount,
                outcome,
            ));
        }
        env.storage().instance().set(&Key::Totals, &totals);
        day.store(&env);
        extend_instance(&env);
        RecurringCharges { settled }.publish(&env);
        outcomes
    }

    pub fn get_mandate(env: Env, owner: Address) -> Option<Mandate> {
        env.storage().persistent().get(&Key::Mandate(owner))
    }

    pub fn get_balance(env: Env, owner: Address) -> i128 {
        load_account(&env, &Key::Account(owner)).map_or(0, |account| account.balance)
    }

    pub fn get_account(env: Env, owner: Address) -> Option<Account> {
        load_account(&env, &Key::Account(owner))
    }

    /// The spending limit in force for `owner` now.
    pub fn get_cap(env: Env, owner: Address) -> i128 {
        load_account(&env, &Key::Account(owner))
            .map_or(0, |account| cap_in_force(&account, env.ledger().sequence()))
    }

    pub fn get_daily_limits(env: Env) -> Option<DailyLimits> {
        env.storage().instance().get(&Key::DailyLimits)
    }

    pub fn get_launch_limits(env: Env) -> Option<LaunchLimits> {
        env.storage().instance().get(&Key::LaunchLimits)
    }

    pub fn get_pending_upgrade(env: Env) -> Option<PendingUpgrade> {
        env.storage().instance().get(&Key::PendingUpgrade)
    }

    pub fn get_config(env: Env) -> Config {
        config(&env)
    }

    pub fn get_totals(env: Env) -> Totals {
        totals(&env)
    }

    pub fn set_daily_limits(env: Env, limits: DailyLimits) {
        config(&env).admin.require_auth();
        if limits.per_buyer <= 0 || limits.per_seller <= 0 {
            panic_with_error!(&env, Error::InvalidLimits);
        }
        let previous = env.storage().instance().get(&Key::DailyLimits);
        env.storage().instance().set(&Key::DailyLimits, &limits);
        extend_instance(&env);
        DailyLimitsChanged { previous, current: limits }.publish(&env);
    }

    /// Sets or, with `None`, removes the launch limits. They only refuse
    /// deposits, so they apply at once.
    pub fn set_launch_limits(env: Env, limits: Option<LaunchLimits>) {
        config(&env).admin.require_auth();
        if limits.as_ref().is_some_and(|l| l.max_balance <= 0 || l.max_total <= 0) {
            panic_with_error!(&env, Error::InvalidLimits);
        }
        let previous = env.storage().instance().get(&Key::LaunchLimits);
        match &limits {
            Some(limits) => env.storage().instance().set(&Key::LaunchLimits, limits),
            None => env.storage().instance().remove(&Key::LaunchLimits),
        }
        extend_instance(&env);
        LaunchLimitsChanged { previous, current: limits }.publish(&env);
    }

    /// Stops deposits, charges and new mandates until `unpause`. Exits,
    /// withdrawals, limit changes, revocations and revenue payouts go on.
    pub fn pause(env: Env) {
        set_paused(&env, true);
    }

    pub fn unpause(env: Env) {
        set_paused(&env, false);
    }

    pub fn set_limits(env: Env, limits: Limits) {
        let mut config = config(&env);
        config.admin.require_auth();
        validate_limits(&env, &limits);
        let previous = core::mem::replace(&mut config.limits, limits.clone());
        store_config(&env, &config);
        LimitsChanged { previous, current: limits }.publish(&env);
    }

    /// Hands the admin role to `admin`, which must authorize taking it.
    pub fn set_admin(env: Env, admin: Address) {
        let mut config = config(&env);
        config.admin.require_auth();
        rotate(&env, &mut config, symbol_short!("admin"), admin, |config| &mut config.admin);
    }

    /// Moves the operator role; the previous key can no longer charge. The
    /// operator can take no more than buyers' limits, so the admin moves it
    /// at once.
    pub fn set_operator(env: Env, operator: Address) {
        let mut config = config(&env);
        config.admin.require_auth();
        rotate(&env, &mut config, symbol_short!("operator"), operator, |config| {
            &mut config.operator
        });
    }

    /// Moves the seller role, and with it the right to the revenue. Needs the
    /// current seller and the new one, not the admin, so no other key can
    /// redirect the seller's revenue.
    pub fn set_seller(env: Env, seller: Address) {
        let mut config = config(&env);
        config.seller.require_auth();
        rotate(&env, &mut config, symbol_short!("seller"), seller, |config| &mut config.seller);
    }

    /// Proposes new code, installable `UPGRADE_DELAY_LEDGERS` from now. A new
    /// proposal replaces the previous one and restarts the delay. The Wasm
    /// should already be uploaded, so buyers can read what is coming.
    pub fn propose_upgrade(env: Env, wasm_hash: BytesN<32>) {
        config(&env).admin.require_auth();
        let effective_at = checked_ledger(&env, UPGRADE_DELAY_LEDGERS);
        env.storage().instance().set(
            &Key::PendingUpgrade,
            &PendingUpgrade { wasm_hash: wasm_hash.clone(), effective_at },
        );
        extend_instance(&env);
        UpgradeProposed { wasm_hash, effective_at }.publish(&env);
    }

    pub fn cancel_upgrade(env: Env) {
        config(&env).admin.require_auth();
        let pending = env
            .storage()
            .instance()
            .get::<Key, PendingUpgrade>(&Key::PendingUpgrade)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoUpgrade));
        env.storage().instance().remove(&Key::PendingUpgrade);
        extend_instance(&env);
        UpgradeCancelled { wasm_hash: pending.wasm_hash }.publish(&env);
    }

    /// Installs exactly the proposed code, as a Wasm executable, once its
    /// delay has passed and before its window closes. The host also emits a
    /// system event naming the previous and the new code.
    pub fn upgrade(env: Env) {
        config(&env).admin.require_auth();
        let pending = env
            .storage()
            .instance()
            .get::<Key, PendingUpgrade>(&Key::PendingUpgrade)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoUpgrade));
        let now = env.ledger().sequence();
        if now < pending.effective_at {
            panic_with_error!(&env, Error::UpgradeLocked);
        }
        if now > pending.effective_at.saturating_add(UPGRADE_WINDOW_LEDGERS) {
            panic_with_error!(&env, Error::UpgradeLapsed);
        }
        env.storage().instance().remove(&Key::PendingUpgrade);
        extend_instance(&env);
        UpgradeInstalled { wasm_hash: pending.wasm_hash.clone() }.publish(&env);
        env.deployer().update_current_contract(ContractExecutable::Wasm(pending.wasm_hash));
    }
}

/// What the account was charged for `day`.
fn charged_on(account: &Account, day: u64) -> i128 {
    if account.day == day {
        account.charged
    } else if account.prev_day == day {
        account.prev_charged
    } else {
        0
    }
}

/// Records `charged` as the account's total for `day`, keeping the two
/// latest days counted.
fn count_charge(account: &mut Account, day: u64, charged: i128) {
    if account.day == day {
        account.charged = charged;
    } else if day > account.day {
        account.prev_day = account.day;
        account.prev_charged = account.charged;
        account.day = day;
        account.charged = charged;
    } else {
        account.prev_day = day;
        account.prev_charged = charged;
    }
}

/// The limit in force at ledger `now`: the pending lower limit once its
/// notice has passed, the current one before.
fn cap_in_force(account: &Account, now: u32) -> i128 {
    match &account.pending_cap {
        CapChange::Pending(pending) if now >= pending.effective_at => pending.cap,
        _ => account.cap,
    }
}

/// Applies a limit change by `owner`: at once if it does not lower the limit
/// in force, otherwise after the notice.
fn change_cap(env: &Env, owner: &Address, account: &mut Account, cap: i128) {
    if cap < 0 {
        panic_with_error!(env, Error::InvalidCap);
    }
    let now = env.ledger().sequence();
    let pending = match core::mem::replace(&mut account.pending_cap, CapChange::None) {
        CapChange::Pending(pending) if now >= pending.effective_at => {
            account.cap = pending.cap;
            None
        }
        CapChange::Pending(pending) => Some(pending),
        CapChange::None => None,
    };
    if cap >= account.cap {
        account.cap = cap;
        CapRaised { owner: owner.clone(), cap }.publish(env);
        return;
    }
    // A lower limit no lower than one already pending keeps that one's
    // effective ledger: it never takes effect sooner, so charges admitted
    // under the limit in force still settle. Anything lower waits a full
    // notice from now.
    let effective_at = match pending {
        Some(pending) if cap >= pending.cap => pending.effective_at,
        _ => checked_ledger(env, NOTICE_LEDGERS),
    };
    account.pending_cap = CapChange::Pending(PendingCap { cap, effective_at });
    CapLowered { owner: owner.clone(), cap, effective_at }.publish(env);
}

fn settle(
    env: &Env,
    config: &Config,
    totals: &mut Totals,
    day: &mut Charging,
    charge: Charge,
) -> Settled {
    let Charge(owner, charge_id, amount, last_ledger, admitted_on) = charge;
    if amount <= 0 {
        panic_with_error!(env, Error::InvalidAmount);
    }
    if admitted_on > day.today {
        panic_with_error!(env, Error::InvalidDay);
    }
    let now = env.ledger().sequence();
    if last_ledger > now.saturating_add(MAX_CHARGE_WINDOW) {
        panic_with_error!(env, Error::ChargeWindowTooLong);
    }
    let record = Key::Charge(owner.clone(), charge_id.clone());
    if env.storage().temporary().has(&record) {
        return Settled(owner, charge_id, amount, Outcome::Duplicate);
    }
    // A charge counts against the day it was admitted in, so one admitted
    // within the limit late in a day still fits when it settles the next.
    // Older days are not kept: such a charge is refused like a late one.
    if last_ledger < now || admitted_on + 1 < day.today {
        return Settled(owner, charge_id, amount, Outcome::Expired);
    }
    let key = Key::Account(owner.clone());
    let outcome = match load_account(env, &key) {
        None => Outcome::UnknownAccount,
        Some(_) if amount > config.limits.max_charge => Outcome::AboveLimit,
        Some(account) if amount > account.balance => Outcome::InsufficientBalance,
        Some(mut account) => {
            let counted = charged_on(&account, admitted_on);
            let charged = checked_add(env, counted, amount);
            if charged > cap_in_force(&account, now) {
                Outcome::AboveCap
            } else if day.exceeds(env, counted, amount) {
                Outcome::AboveDailyLimit
            } else {
                account.balance -= amount;
                count_charge(&mut account, admitted_on, charged);
                day.seller.charged = checked_add(env, day.seller.charged, amount);
                totals.liabilities -= amount;
                totals.revenue = checked_add(env, totals.revenue, amount);
                store_account(env, &key, account);
                Outcome::Charged
            }
        }
    };
    env.storage().temporary().set(&record, &outcome);
    let live_for = last_ledger - now + CHARGE_RECORD_GRACE;
    env.storage().temporary().extend_ttl(&record, live_for, live_for);
    Settled(owner, charge_id, amount, outcome)
}

fn settle_recurring(
    env: &Env,
    config: &Config,
    usdc: &token::Client,
    totals: &mut Totals,
    day: &mut Charging,
    charge: &RecurringCharge,
) -> RecurringOutcome {
    let amount = charge.amount;
    if amount <= 0 || amount > MAX_TRANSFER {
        panic_with_error!(env, Error::InvalidAmount);
    }
    let now = env.ledger().sequence();
    if charge.last_ledger > now.saturating_add(MAX_CHARGE_WINDOW) {
        panic_with_error!(env, Error::ChargeWindowTooLong);
    }
    let record = Key::Recurring(charge.owner.clone(), charge.charge_id.clone());
    if env.storage().temporary().has(&record) {
        return RecurringOutcome::Duplicate;
    }
    if charge.last_ledger < now {
        return RecurringOutcome::Expired;
    }
    let outcome = charge_mandate(env, config, usdc, totals, day, charge);
    env.storage().temporary().set(&record, &outcome);
    let live_for = charge.last_ledger - now + CHARGE_RECORD_GRACE;
    env.storage().temporary().extend_ttl(&record, live_for, live_for);
    outcome
}

fn charge_mandate(
    env: &Env,
    config: &Config,
    usdc: &token::Client,
    totals: &mut Totals,
    day: &mut Charging,
    charge: &RecurringCharge,
) -> RecurringOutcome {
    let key = Key::Mandate(charge.owner.clone());
    let Some(mut mandate) = env.storage().persistent().get::<Key, Mandate>(&key) else {
        return RecurringOutcome::NoMandate;
    };
    if mandate.mandate_id != charge.mandate_id {
        return RecurringOutcome::NoMandate;
    }
    let due = (env.ledger().timestamp() - mandate.start) / mandate.period_secs;
    if env.ledger().sequence() > mandate.live_until
        || charge.cycle >= mandate.cycles
        || due >= u64::from(mandate.cycles)
    {
        return RecurringOutcome::MandateExpired;
    }
    if charge.cycle < mandate.next_cycle {
        return RecurringOutcome::AlreadyCharged;
    }
    let cycle = u64::from(charge.cycle);
    if cycle > due {
        return RecurringOutcome::NotDue;
    }
    if cycle < due {
        return RecurringOutcome::PeriodOver;
    }
    if charge.amount > mandate.amount {
        return RecurringOutcome::AboveMandate;
    }
    if charge.amount > config.limits.max_charge {
        return RecurringOutcome::AboveLimit;
    }
    if day.exceeds_seller(env, charge.amount) {
        return RecurringOutcome::AboveDailyLimit;
    }
    let vault = env.current_contract_address();
    match usdc.try_transfer_from(&vault, &charge.owner, &vault, &charge.amount) {
        Ok(_) => {}
        Err(Ok(error)) if error.is_type(ScErrorType::Contract) => {
            return match error.get_code() {
                9 => RecurringOutcome::AllowanceShort,
                10 => RecurringOutcome::WalletShort,
                _ => RecurringOutcome::TransferRefused,
            };
        }
        Err(_) => return RecurringOutcome::TransferRefused,
    }
    mandate.next_cycle = charge.cycle + 1;
    put_persistent(env, &key, &mandate);
    day.seller.charged = checked_add(env, day.seller.charged, charge.amount);
    totals.revenue = checked_add(env, totals.revenue, charge.amount);
    RecurringOutcome::Charged
}

struct Charging {
    limits: Option<DailyLimits>,
    today: u64,
    seller: DayCount,
}

impl Charging {
    fn load(env: &Env) -> Self {
        let today = env.ledger().timestamp() / 86_400;
        let seller = env
            .storage()
            .instance()
            .get::<Key, DayCount>(&Key::SellerDay)
            .filter(|count| count.day == today)
            .unwrap_or(DayCount { day: today, charged: 0 });
        Self { limits: env.storage().instance().get(&Key::DailyLimits), today, seller }
    }

    fn exceeds(&self, env: &Env, buyer_today: i128, amount: i128) -> bool {
        self.limits.as_ref().is_some_and(|limits| {
            checked_add(env, buyer_today, amount) > limits.per_buyer
                || checked_add(env, self.seller.charged, amount) > limits.per_seller
        })
    }

    fn exceeds_seller(&self, env: &Env, amount: i128) -> bool {
        self.limits
            .as_ref()
            .is_some_and(|limits| checked_add(env, self.seller.charged, amount) > limits.per_seller)
    }

    fn store(&self, env: &Env) {
        env.storage().instance().set(&Key::SellerDay, &self.seller);
    }
}

fn load_account(env: &Env, key: &Key) -> Option<Account> {
    env.storage().persistent().get(key)
}

fn set_paused(env: &Env, paused: bool) {
    let mut config = config(env);
    config.admin.require_auth();
    config.paused = paused;
    store_config(env, &config);
    PauseChanged { paused }.publish(env);
}

/// Moves one role to `current`, which must authorize taking it; the caller
/// has already required whoever may move this role.
fn rotate(
    env: &Env,
    config: &mut Config,
    role: Symbol,
    current: Address,
    slot: fn(&mut Config) -> &mut Address,
) {
    if *slot(config) == current {
        panic_with_error!(env, Error::DuplicateRole);
    }
    current.require_auth();
    let previous = core::mem::replace(slot(config), current.clone());
    require_distinct_roles(env, config);
    store_config(env, config);
    RoleChanged { role, previous, current }.publish(env);
}

fn require_distinct_roles(env: &Env, config: &Config) {
    let roles = [&config.admin, &config.operator, &config.seller];
    for (i, role) in roles.iter().enumerate() {
        if roles[i + 1..].contains(role) {
            panic_with_error!(env, Error::DuplicateRole);
        }
    }
}

/// A payout to this contract would leave the USDC where it is while the
/// ledger counted it as paid; one to the USDC contract would lose it.
fn require_payable(env: &Env, config: &Config, destination: &Address) {
    if *destination == env.current_contract_address() || *destination == config.usdc {
        panic_with_error!(env, Error::InvalidDestination);
    }
}

/// Stores an account with any pending lower limit that has taken effect
/// folded into `cap`, so the stored `cap` is the limit in force.
fn store_account(env: &Env, key: &Key, mut account: Account) {
    let now = env.ledger().sequence();
    if let CapChange::Pending(pending) = &account.pending_cap
        && now >= pending.effective_at
    {
        account.cap = pending.cap;
        account.pending_cap = CapChange::None;
    }
    put_persistent(env, key, &account);
}

fn checked_ledger(env: &Env, delay: u32) -> u32 {
    env.ledger()
        .sequence()
        .checked_add(delay)
        .unwrap_or_else(|| panic_with_error!(env, Error::Overflow))
}

fn store_config(env: &Env, config: &Config) {
    env.storage().instance().set(&Key::Config, config);
    extend_instance(env);
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

fn checked_mul(env: &Env, a: i128, b: i128) -> i128 {
    a.checked_mul(b).unwrap_or_else(|| panic_with_error!(env, Error::Overflow))
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
