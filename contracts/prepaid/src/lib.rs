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
//! Charge idempotency: every charge carries an identifier chosen by the
//! seller and a last ledger in which it may be settled. The contract records
//! each identifier it settles, with the outcome, in temporary storage that
//! lives until shortly after that ledger. A charge whose identifier is
//! recorded is a duplicate and never debits again; a charge past its last
//! ledger is refused. Once the record expires the charge is past its last
//! ledger too, so a replay is refused either way. The record also answers,
//! for as long as it lives, what happened to a charge whose transaction's
//! fate is unknown.
//!
//! Daily limits: once the admin sets them, a charge is refused if it would
//! take more than `per_buyer` from one account, or more than `per_seller`
//! across all accounts, in one UTC day of ledger time. The counts live in
//! entries a charge already writes (the account, the instance), so the limit
//! costs a batch no extra write.
//!
//! Recurring charges: a buyer may also authorize the seller to charge their
//! wallet once per period, up to an amount per period, for a number of
//! periods, until a ledger. The buyer signs one authorization that both
//! records these terms (the mandate) and approves this contract, through the
//! USDC contract's `approve`, to move up to the amount times the periods
//! until that ledger. Only this contract can spend that allowance, and only
//! within the mandate: each period's charge moves USDC from the buyer's
//! wallet straight to the treasury as revenue, without touching the buyer's
//! prepaid account. A period the seller does not charge in is not charged
//! later. The allowance's own expiry bounds the mandate even if the contract
//! did not.

#![no_std]

use soroban_sdk::{
    Address, BytesN, ContractExecutable, Env, Symbol, Vec, contract, contracterror, contractevent,
    contractimpl, contracttype, panic_with_error, symbol_short, token, xdr::ScErrorType,
};

/// Largest number of charges one `charge_batch` call accepts. Each charge of
/// a distinct buyer writes its account and its record: 98 charges make 196
/// entries plus the instance and the operator's authorization nonce, 198
/// against the network's 200 write entries per transaction. The footprint,
/// about 200 entries, is well within its limit of 400.
pub const MAX_BATCH: u32 = 98;

/// Largest number of recurring charges one `charge_recurring_batch` call
/// accepts. The bound is the network's 16,384 bytes of contract events per
/// transaction: each charge adds its entry to this contract's event and the
/// USDC contract's own transfer event, about 416 bytes together, so 39 fit
/// and 35 leave a margin. Writes (four per charge: mandate, record,
/// allowance, buyer's USDC balance) would allow 49.
pub const MAX_RECURRING_BATCH: u32 = 35;

/// Furthest ahead, in ledgers (~1 day), a charge's last ledger may be. A
/// charge record's rent grows with how long it lives, so this bounds it.
pub const MAX_CHARGE_WINDOW: u32 = 17_280;

/// Ledgers (~1 hour) a charge record outlives the charge's last ledger, so a
/// submitter can still read the outcome of a charge that just expired.
pub const CHARGE_RECORD_GRACE: u32 = 720;

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
    ChargeExpired = 111,
    EmptyBatch = 112,
    BatchTooLarge = 113,
    WithdrawalAlreadyProcessed = 114,
    InsufficientRevenue = 115,
    Overflow = 116,
    /// Two roles would share one address, or a rotation names the current
    /// holder of the role.
    DuplicateRole = 117,
    /// A charge's last ledger is further ahead than `MAX_CHARGE_WINDOW`.
    ChargeWindowTooLong = 118,
    /// The charge would take the account or the seller past a daily limit.
    ChargeAboveDailyLimit = 119,
    /// A mandate's amount, period, number of periods or last ledger is not
    /// usable.
    InvalidMandate = 120,
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
    /// UTC day (ledger time / 86400) `charged` counts.
    pub day: u64,
    /// Charged from this account on `day`.
    pub charged: i128,
}

/// The most that may be charged in one UTC day of ledger time, from one
/// account and across the seller's accounts, in USDC base units.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DailyLimits {
    pub per_buyer: i128,
    pub per_seller: i128,
}

/// What the seller's accounts were charged on `day`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DayCount {
    pub day: u64,
    pub charged: i128,
}

/// One charge: the account owner, the seller's identifier for the charge,
/// the amount, and the last ledger in which it may be settled.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Charge(pub Address, pub BytesN<32>, pub i128, pub u32);

/// Outcome of one charge in a batch. Every outcome but `Duplicate` and
/// `Expired` records the charge's identifier with that outcome, so the same
/// charge can never debit twice; `Duplicate` and `Expired` record nothing.
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
    /// The charge would pass the account's or the seller's daily limit.
    AboveDailyLimit = 6,
}

/// Per-charge result in an event: account owner, charge identifier, amount,
/// outcome.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settled(pub Address, pub BytesN<32>, pub i128, pub Outcome);

/// A buyer's standing authorization for the seller to charge their wallet:
/// up to `amount` once per `period_secs` of ledger time from `start`, for
/// `cycles` periods, and never after ledger `live_until`. `next_cycle` is the
/// first period not charged yet.
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

/// One recurring charge: the buyer, the identifier of this attempt, the
/// mandate and period it charges for, the amount, and the last ledger in
/// which it may be settled. A refused period may be attempted again under a
/// new identifier; a period is charged at most once whatever the identifier.
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

/// Outcome of one recurring charge. As for prepaid charges, every outcome
/// but `Duplicate` and `Expired` records the attempt with that outcome.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum RecurringOutcome {
    Charged = 0,
    /// This attempt's identifier was already settled.
    Duplicate = 1,
    /// Past the attempt's last ledger.
    Expired = 2,
    /// The buyer has no mandate, or a different one than the charge names.
    NoMandate = 3,
    /// Past the mandate's last ledger or its last period.
    MandateExpired = 4,
    /// The period was already charged.
    AlreadyCharged = 5,
    /// The period has not started.
    NotDue = 6,
    /// The period is over; it is not charged late.
    PeriodOver = 7,
    /// Above the mandate's amount per period.
    AboveMandate = 8,
    /// Above the largest single charge.
    AboveLimit = 9,
    /// The charge would pass the seller's daily limit.
    AboveDailyLimit = 10,
    /// The buyer's allowance to this contract no longer covers it, e.g.
    /// because the buyer lowered it in the USDC contract.
    AllowanceShort = 11,
    /// The buyer's wallet holds less USDC than the amount.
    WalletShort = 12,
    /// The USDC contract refused the transfer for another reason, e.g. the
    /// wallet has no USDC trustline or is frozen.
    TransferRefused = 13,
}

/// Per-charge result in a `recurring` event: owner, attempt identifier,
/// mandate, period, amount, outcome. A tuple, like [`Settled`], because
/// field names would add to an event whose size bounds the batch.
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
    /// Temporary: a settled charge's outcome, by owner and charge identifier.
    Charge(Address, BytesN<32>),
    /// Instance: the daily limits, absent until the admin sets them.
    DailyLimits,
    /// Instance: what the seller's accounts were charged today.
    SellerDay,
    /// A buyer's mandate.
    Mandate(Address),
    /// Temporary: a settled recurring charge's outcome, by owner and the
    /// attempt's identifier.
    Recurring(Address, BytesN<32>),
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

/// A role moved to another address. Every role change is visible on-chain,
/// so a rotation nobody expected can be detected.
#[contractevent(topics = ["role"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleChanged {
    #[topic]
    pub role: Symbol,
    pub previous: Address,
    pub current: Address,
}

/// The admin paused or unpaused the contract; `paused` is the state after
/// the call. Emitted on every call, even one that leaves the state as it was.
#[contractevent(topics = ["pause"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseChanged {
    pub paused: bool,
}

/// The admin replaced the deposit and charge limits.
#[contractevent(topics = ["limits"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitsChanged {
    pub previous: Limits,
    pub current: Limits,
}

/// The admin set the daily limits; `previous` is `None` the first time.
#[contractevent(topics = ["daily"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DailyLimitsChanged {
    pub previous: Option<DailyLimits>,
    pub current: DailyLimits,
}

/// A buyer authorized a mandate, replacing any previous one.
#[contractevent(topics = ["mandate"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MandateAuthorized {
    #[topic]
    pub owner: Address,
    pub mandate: Mandate,
}

/// A buyer revoked their mandate; `mandate_id` is `None` if there was none.
/// The buyer's allowance to this contract is zero afterwards either way.
#[contractevent(topics = ["revoke"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MandateRevoked {
    #[topic]
    pub owner: Address,
    pub mandate_id: Option<BytesN<32>>,
}

/// One event per recurring charge call, for the same reason as [`Charges`].
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
pub struct PrepaidLedger;

#[contractimpl]
impl PrepaidLedger {
    /// Runs once, atomically with deployment, so no one can initialize the
    /// contract with other roles between deployment and set-up. The four
    /// roles must be four distinct addresses: each authorizes something the
    /// others must not be able to do alone.
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
        require_distinct_roles(&env, &config);
        env.storage().instance().set(&Key::Config, &config);
        env.storage().instance().set(&Key::Totals, &Totals { liabilities: 0, revenue: 0 });
        extend_instance(&env);
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
        let mut account =
            load_account(&env, &account_key).unwrap_or(Account { balance: 0, day: 0, charged: 0 });
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
        }
        env.storage().instance().set(&Key::Totals, &totals);
        day.store(&env);
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
        let mut account = load_account(&env, &account_key)
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

    /// Records `owner`'s mandate, replacing any previous one, and approves
    /// this contract to move up to `amount` times `cycles` of the owner's USDC
    /// until ledger `live_until`. The approval replaces any previous one, so
    /// what is left of a replaced mandate's allowance cannot be spent under
    /// the new one. The owner authorizes both the call and the approval, with
    /// exactly these terms; the first period starts now.
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
        let allowance = checked_mul(&env, amount, i128::from(cycles));
        let mandate = Mandate {
            mandate_id,
            amount,
            period_secs,
            start: env.ledger().timestamp(),
            cycles,
            live_until,
            next_cycle: 0,
        };
        put_persistent(&env, &Key::Mandate(owner.clone()), &mandate);
        extend_instance(&env);
        // The USDC contract refuses a last ledger beyond what the network
        // lets an entry live, so that bound needs no check here.
        token::Client::new(&env, &config.usdc).approve(
            &owner,
            &env.current_contract_address(),
            &allowance,
            &live_until,
        );
        MandateAuthorized { owner, mandate }.publish(&env);
    }

    /// Ends `owner`'s mandate and sets the owner's allowance to this contract
    /// to zero. Allowed while the contract is paused: stopping future charges
    /// must not depend on the admin. The owner can also lower the allowance
    /// in the USDC contract directly, which stops charges just the same.
    pub fn revoke_recurring(env: Env, owner: Address) {
        owner.require_auth();
        let config = config(&env);
        let key = Key::Mandate(owner.clone());
        let mandate_id = env.storage().persistent().get::<Key, Mandate>(&key).map(|m| m.mandate_id);
        env.storage().persistent().remove(&key);
        extend_instance(&env);
        // A fixed last ledger keeps the call the owner signs independent of
        // the ledger it lands in; the USDC contract accepts any last ledger
        // for a zero allowance.
        token::Client::new(&env, &config.usdc).approve(
            &owner,
            &env.current_contract_address(),
            &0,
            &0,
        );
        MandateRevoked { owner, mandate_id }.publish(&env);
    }

    /// Settles up to `MAX_RECURRING_BATCH` recurring charges and returns each
    /// one's outcome. A refused entry does not affect the others; only a
    /// malformed batch reverts the whole call.
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

    pub fn get_daily_limits(env: Env) -> Option<DailyLimits> {
        env.storage().instance().get(&Key::DailyLimits)
    }

    /// Limits what one account, and all the seller's accounts together, may
    /// be charged in a UTC day of ledger time. Both must be positive; a limit
    /// set above anything charged in a day disables it in effect.
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

    pub fn get_config(env: Env) -> Config {
        config(&env)
    }

    pub fn get_totals(env: Env) -> Totals {
        totals(&env)
    }

    /// Stops deposits, charges and withdrawals until `unpause`.
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

    /// Hands the admin role to `admin`, which must authorize taking it, so
    /// the role cannot be moved to an address nobody controls.
    pub fn set_admin(env: Env, admin: Address) {
        rotate(&env, symbol_short!("admin"), admin, |config| &mut config.admin);
    }

    /// Moves the operator role, e.g. after its key leaked; the previous key
    /// can no longer authorize charges.
    pub fn set_operator(env: Env, operator: Address) {
        rotate(&env, symbol_short!("operator"), operator, |config| &mut config.operator);
    }

    pub fn set_seller(env: Env, seller: Address) {
        rotate(&env, symbol_short!("seller"), seller, |config| &mut config.seller);
    }

    /// Points deposits and withdrawals at another treasury account. The
    /// contract holds no USDC, so nothing moves here: the previous treasury
    /// must transfer what it holds to the new one, which needs a USDC
    /// trustline, for the liabilities to stay covered.
    pub fn set_treasury(env: Env, treasury: Address) {
        rotate(&env, symbol_short!("treasury"), treasury, |config| &mut config.treasury);
    }

    /// Replaces the contract code. The admin can change what every balance
    /// means through this, which is why it is a separate key from every
    /// other role. The host itself emits a system event naming the previous
    /// and the new code, so this call publishes no event of its own.
    pub fn upgrade(env: Env, wasm_hash: BytesN<32>) {
        config(&env).admin.require_auth();
        env.deployer().update_current_contract(ContractExecutable::Wasm(wasm_hash));
        extend_instance(&env);
    }
}

fn settle(
    env: &Env,
    config: &Config,
    totals: &mut Totals,
    day: &mut Charging,
    charge: Charge,
) -> Settled {
    let Charge(owner, charge_id, amount, last_ledger) = charge;
    // A malformed charge is not a refusal a retry could change: reject the
    // whole call.
    if amount <= 0 {
        panic_with_error!(env, Error::InvalidAmount);
    }
    let now = env.ledger().sequence();
    if last_ledger > now.saturating_add(MAX_CHARGE_WINDOW) {
        panic_with_error!(env, Error::ChargeWindowTooLong);
    }
    let record = Key::Charge(owner.clone(), charge_id.clone());
    if env.storage().temporary().has(&record) {
        return Settled(owner, charge_id, amount, Outcome::Duplicate);
    }
    if last_ledger < now {
        return Settled(owner, charge_id, amount, Outcome::Expired);
    }
    let key = Key::Account(owner.clone());
    let outcome = match load_account(env, &key) {
        None => Outcome::UnknownAccount,
        Some(_) if amount > config.limits.max_charge => Outcome::AboveLimit,
        Some(account) if amount > account.balance => Outcome::InsufficientBalance,
        Some(mut account) => {
            let buyer_today = if account.day == day.today { account.charged } else { 0 };
            if day.exceeds(env, buyer_today, amount) {
                Outcome::AboveDailyLimit
            } else {
                account.balance -= amount;
                account.day = day.today;
                account.charged = checked_add(env, buyer_today, amount);
                day.seller.charged = checked_add(env, day.seller.charged, amount);
                totals.liabilities -= amount;
                totals.revenue = checked_add(env, totals.revenue, amount);
                put_persistent(env, &key, &account);
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

/// Checks a recurring charge against its mandate and limits and, if they
/// allow it, moves the USDC and counts the period as charged.
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
    // The buyer's own daily limit counts prepaid charges in the account,
    // which a recurring charge does not touch; the mandate bounds what it
    // takes from the buyer instead.
    if day.exceeds_seller(env, charge.amount) {
        return RecurringOutcome::AboveDailyLimit;
    }
    // A refused transfer reverts only itself: nothing moved, and the period
    // stays chargeable.
    let moved = usdc.try_transfer_from(
        &env.current_contract_address(),
        &charge.owner,
        &config.treasury,
        &charge.amount,
    );
    match moved {
        Ok(_) => {}
        Err(Ok(error)) if error.is_type(ScErrorType::Contract) => {
            // The USDC contract's codes for these refusals.
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

/// The daily limits and what the seller's accounts were charged today, for
/// the charges of one call.
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

    /// Whether charging `amount` to an account charged `buyer_today` today
    /// would pass either limit.
    fn exceeds(&self, env: &Env, buyer_today: i128, amount: i128) -> bool {
        self.limits.as_ref().is_some_and(|limits| {
            checked_add(env, buyer_today, amount) > limits.per_buyer
                || checked_add(env, self.seller.charged, amount) > limits.per_seller
        })
    }

    /// Whether charging `amount` would pass the seller's limit.
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

/// Moves one role to `current`, authorized by the admin and by `current`.
fn rotate(env: &Env, role: Symbol, current: Address, slot: fn(&mut Config) -> &mut Address) {
    let mut config = config(env);
    if *slot(&mut config) == current {
        panic_with_error!(env, Error::DuplicateRole);
    }
    config.admin.require_auth();
    current.require_auth();
    let previous = core::mem::replace(slot(&mut config), current.clone());
    require_distinct_roles(env, &config);
    store_config(env, &config);
    RoleChanged { role, previous, current }.publish(env);
}

fn require_distinct_roles(env: &Env, config: &Config) {
    let roles = [&config.admin, &config.operator, &config.seller, &config.treasury];
    for (i, role) in roles.iter().enumerate() {
        if roles[i + 1..].contains(role) {
            panic_with_error!(env, Error::DuplicateRole);
        }
    }
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
