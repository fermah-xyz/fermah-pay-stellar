//! Settlement worker against real PostgreSQL, with the API and the worker
//! under their production roles, and a scripted network that applies the
//! prepaid contract's rules: expiring per-charge records, per-owner deposit
//! markers, single-use authorization nonces and expiring signatures. The
//! script can lose a send, include a transaction as failed, or include a
//! broadcast authorization through someone else's transaction.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use common::{
    EventStream, Harness, Ledger, Tenant, assert_refused, authed, pool_as, start_with_options,
};
use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{
    CHARGE_RECORD_GRACE, MAX_CHARGE_WINDOW, Outcome, PrepaidDeployment,
};
use fermah_pay_stellar_chain::rpc::{
    EventPage, EventsFrom, FeeDistribution, FeePercentile, FeeStats, Health, IncludedTransaction,
    LatestLedgerInfo, LedgerEntries, LedgerEntryRecord, NodeView, RpcError, SendOutcome,
    Simulation, SimulationOutcome, TransactionStatus,
};
use fermah_pay_stellar_chain::signer::LocalSigner;
use fermah_pay_stellar_chain::soroban::fee_bump_hash;
use fermah_pay_stellar_chain::stellar_xdr::{
    AccountEntry, AccountEntryExt, BytesM, ContractCodeEntry, ContractCodeEntryExt,
    ContractDataEntry, ContractEvent, ContractEventBody, ContractEventType, ContractEventV0,
    ContractExecutable, ContractId, ExtensionPoint, FeeBumpTransactionInnerTx, Hash, HostFunction,
    Int128Parts, InvokeContractArgs, InvokeHostFunctionOp, LedgerEntryChanges, LedgerEntryData,
    LedgerEntryExt, LedgerFootprint, LedgerKey, LedgerKeyAccount, LedgerKeyContractCode,
    LedgerKeyContractData, Limits, Memo, MuxedAccount, Operation, OperationBody, OperationMetaV2,
    Preconditions, ReadXdr, ScAddress, ScContractInstance, ScMap, ScMapEntry, ScSymbol, ScVal,
    ScVec, SequenceNumber, SorobanAddressCredentials, SorobanAuthorizationEntry,
    SorobanAuthorizedFunction, SorobanAuthorizedInvocation, SorobanCredentials, SorobanResources,
    SorobanTransactionData, SorobanTransactionDataExt, SorobanTransactionMetaExt,
    SorobanTransactionMetaV2, String32, Thresholds, Transaction, TransactionEnvelope,
    TransactionExt, TransactionMeta, TransactionMetaV4, TransactionResult, TransactionResultExt,
    TransactionResultResult, TransactionV1Envelope, TrustLineEntry, TrustLineEntryExt, Uint256,
    VecM, WriteXdr,
};
use fermah_pay_stellar_chain::transaction::{account_id, address_of};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::events::EventLog;
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_gateway::lease::{self, Lease};
use fermah_pay_stellar_gateway::ledger::LatestLedger;
use fermah_pay_stellar_gateway::quarantine::{
    self, QuarantineError, QuarantinedCharge, Resolution,
};
use fermah_pay_stellar_gateway::store::Quotas;
use fermah_pay_stellar_gateway::submission::{
    Chain, Clock, Engine, FeePolicy, Keys, Policy, SourceSequence,
};
use fermah_pay_stellar_gateway::worker::{Reserve, Settings, Step, Worker};
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    Charge, ChargeState, CreateBuyerRequest, CreateChargeRequest, CreateRecurringChargeRequest,
    Deposit, DepositState, GetBalanceRequest, GetChargeRequest, GetDepositRequest,
    GetMandateRequest, GetRecurringChargeRequest, GetRevocationRequest, GetWithdrawalRequest,
    Mandate, MandateState, PrepareDepositRequest, PrepareMandateRequest, PrepareRevocationRequest,
    PrepareWithdrawalRequest, RecurringCharge, RecurringChargeState, Revocation, RevocationState,
    SubmitDepositRequest, SubmitMandateRequest, SubmitRevocationRequest, SubmitWithdrawalRequest,
    Withdrawal, WithdrawalState,
};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use time::OffsetDateTime;
use tokio::sync::Barrier;

const CONTRACT: [u8; 32] = [7; 32];
const MAX_CHARGE: i128 = 50;
const START_LEDGER: u32 = 1_000;
/// The network's base reserve, 0.5 XLM.
const BASE_RESERVE: u32 = 5_000_000;
/// The worker's fee floor in these tests: 10 XLM.
const FEE_FLOOR: i64 = 100_000_000;
/// The worker's contract life settings in these tests.
const TTL_THRESHOLD: u32 = 120_960;
const TTL_EXTEND_TO: u32 = 518_400;
const TTL_CHECK_EVERY: Duration = Duration::from_secs(600);
const OPERATOR_LEDGERS: u32 = 12;
const VALIDITY: Duration = Duration::from_secs(60);
const RETRY_AFTER: Duration = Duration::from_secs(30);

// ---- the network --------------------------------------------------------

#[derive(Clone, Default)]
struct ContractState {
    /// owner -> balance
    accounts: HashMap<AccountAddress, i128>,
    /// Settled charge records: outcome code, the last ledger they live, and
    /// the ledger that wrote them.
    records: HashMap<(AccountAddress, [u8; 32]), (u32, u32, u32)>,
    deposits: HashSet<(AccountAddress, [u8; 32])>,
    withdrawals: HashSet<(AccountAddress, [u8; 32])>,
    /// USDC held by each wallet, which a deposit moves into the treasury and
    /// a withdrawal moves back out.
    usdc: HashMap<AccountAddress, i128>,
    treasury_usdc: i128,
    /// Accounts other than the treasury holding a USDC trustline, which a
    /// plain USDC transfer needs at its destination.
    lines: HashSet<AccountAddress>,
    used_nonces: HashSet<(AccountAddress, i64)>,
    /// owner -> the mandate the contract holds.
    mandates: HashMap<AccountAddress, FakeMandate>,
    /// owner -> the contract's USDC allowance and its last ledger.
    allowances: HashMap<AccountAddress, (i128, u32)>,
    /// Recurring charge attempt records: outcome code, last ledger, and the
    /// ledger that wrote them.
    recurring: HashMap<(AccountAddress, [u8; 32]), (u32, u32, u32)>,
}

/// The state after a call, its return value, and the event it emitted, by
/// name.
type Executed = (ContractState, ScVal, Option<(&'static str, ScVal)>);

#[derive(Clone, Debug)]
struct FakeMandate {
    id: [u8; 32],
    amount: i128,
    period_secs: u64,
    start: u64,
    cycles: u32,
    live_until: u32,
    next_cycle: u32,
    /// Ledger it was recorded in: a read at an earlier ledger misses it.
    since: u32,
}

fn field<'a>(map: &'a ScVal, name: &str) -> &'a ScVal {
    let ScVal::Map(Some(ScMap(fields))) = map else { panic!("not a map: {map:?}") };
    &fields.iter().find(|f| f.key == symbol(name)).unwrap_or_else(|| panic!("no {name}")).val
}

fn bytes32(value: &ScVal) -> [u8; 32] {
    let ScVal::Bytes(bytes) = value else { panic!("not bytes") };
    bytes.as_slice().try_into().unwrap()
}

struct Net {
    latest: u32,
    source_sequence: i64,
    /// Sequences of further source accounts; any other source uses
    /// `source_sequence`.
    sequences: HashMap<AccountAddress, i64>,
    state: ContractState,
    included: HashMap<[u8; 32], TransactionStatus>,
    sent: Vec<TransactionEnvelope>,
    /// The next sends are accepted but never included.
    drop_sends: usize,
    /// Sources whose envelopes the network never includes.
    stuck: HashSet<AccountAddress>,
    /// XLM, in stroops, of any classic account read, i.e. the fee account.
    fee_balance: i64,
    /// Last ledger the contract's instance and code live through; `None`
    /// leaves them unreadable, as for a contract this network does not hold.
    instance_live_until: Option<u32>,
    code_live_until: u32,
    /// The next included transactions fail without effect.
    fail_inclusions: usize,
    /// `charge_batch` returns one outcome fewer than it settled.
    truncate_outcomes: bool,
    /// `charge_batch` settles without leaving records, which the real
    /// contract never does: a contradiction the worker must not guess past.
    skip_records: bool,
    simulation_barrier: Option<Arc<Barrier>>,
    /// Buyer accounts whose contract entry is archived: any call touching one
    /// needs a restore first.
    archived: HashSet<AccountAddress>,
    /// Ledger-entry reads trail the latest ledger by this many ledgers.
    entries_behind: u32,
    /// The resource fee a successful simulation reports.
    resource_fee: i64,
    /// The treasury's USDC as a trailing read reports it, when it differs
    /// from what it holds now.
    stale_treasury: Option<i128>,
    /// Owners the contract's daily limit refuses today.
    over_daily: HashSet<AccountAddress>,
    clock: ManualClock,
    /// The contract's events, and the oldest ledger the node retains.
    events: EventStream,
    oldest: u32,
}

#[derive(Clone)]
struct Stellar {
    net: Arc<Mutex<Net>>,
    deployment: PrepaidDeployment,
    operator: AccountAddress,
}

fn i128_of(value: &ScVal) -> i128 {
    let ScVal::I128(Int128Parts { hi, lo }) = value else { panic!("not i128: {value:?}") };
    (i128::from(*hi) << 64) | i128::from(*lo)
}

fn owner_of(value: &ScVal) -> AccountAddress {
    let ScVal::Address(ScAddress::Account(account)) = value else { panic!("not an account") };
    address_of(account)
}

fn symbol(name: &str) -> ScVal {
    ScVal::Symbol(ScSymbol(name.try_into().unwrap()))
}

fn inner(envelope: &TransactionEnvelope) -> Transaction {
    match envelope {
        TransactionEnvelope::TxFeeBump(bump) => {
            let FeeBumpTransactionInnerTx::Tx(inner) = &bump.tx.inner_tx;
            inner.tx.clone()
        }
        TransactionEnvelope::Tx(v1) => v1.tx.clone(),
        TransactionEnvelope::TxV0(_) => panic!("v0 envelope"),
    }
}

fn is_restore(envelope: &TransactionEnvelope) -> bool {
    matches!(inner(envelope).operations[0].body, OperationBody::RestoreFootprint(_))
}

/// The ledgers of life an extension asks for, if the envelope is one.
fn extension(envelope: &TransactionEnvelope) -> Option<u32> {
    match &inner(envelope).operations[0].body {
        OperationBody::ExtendFootprintTtl(op) => Some(op.extend_to),
        _ => None,
    }
}

/// The contract's code hash in these tests.
const WASM: [u8; 32] = [42; 32];

/// The contract call and authorization entries of an envelope.
fn invocation(
    envelope: &TransactionEnvelope,
) -> (InvokeContractArgs, Vec<SorobanAuthorizationEntry>, i64) {
    let tx = inner(envelope);
    let OperationBody::InvokeHostFunction(op) = &tx.operations[0].body else {
        panic!("not an invocation")
    };
    let HostFunction::InvokeContract(args) = &op.host_function else {
        panic!("not a contract call")
    };
    (args.clone(), op.auth.to_vec(), tx.seq_num.0)
}

impl Net {
    /// Publishes the contract's `name` event in the latest ledger.
    fn log_event(&mut self, name: &str, data: ScVal) {
        let closed =
            self.clock.now().format(&time::format_description::well_known::Rfc3339).unwrap();
        self.events.emit(CONTRACT, self.latest, closed, vec![symbol(name)], data);
    }

    /// Ledger time: the clock, in Unix seconds.
    fn timestamp(&self) -> u64 {
        u64::try_from(self.clock.now().unix_timestamp()).unwrap()
    }

    /// Archived buyer accounts the call would read or write.
    fn touched_archived(&self, call: &InvokeContractArgs) -> Vec<AccountAddress> {
        let args = call.args.as_slice();
        let owners: Vec<AccountAddress> = match call.function_name.0.as_slice() {
            b"deposit" | b"withdraw" => vec![owner_of(&args[0])],
            b"charge_batch" => {
                let ScVal::Vec(Some(ScVec(charges))) = &args[0] else { panic!("charges") };
                charges
                    .iter()
                    .map(|charge| {
                        let ScVal::Vec(Some(ScVec(fields))) = charge else { panic!("charge") };
                        owner_of(&fields[0])
                    })
                    .collect()
            }
            _ => vec![],
        };
        let mut touched: Vec<AccountAddress> =
            owners.into_iter().filter(|owner| self.archived.contains(owner)).collect();
        touched.dedup();
        touched
    }

    /// Applies the call to a copy of the state and returns the new state and
    /// the call's return value, or the reason the network refuses it.
    fn execute(
        &self,
        call: &InvokeContractArgs,
        auth: &[SorobanAuthorizationEntry],
        operator: &AccountAddress,
    ) -> Result<Executed, String> {
        let mut state = self.state.clone();
        let mut authorized = HashSet::new();
        for entry in auth {
            let (SorobanCredentials::Address(creds) | SorobanCredentials::AddressV2(creds)) =
                &entry.credentials
            else {
                continue;
            };
            let ScAddress::Account(account) = &creds.address else { continue };
            let signer = address_of(account);
            if self.latest > creds.signature_expiration_ledger {
                return Err("authorization expired".to_owned());
            }
            if !state.used_nonces.insert((signer.clone(), creds.nonce)) {
                return Err("authorization nonce already used".to_owned());
            }
            authorized.insert(signer);
        }
        let args = call.args.as_slice();
        match call.function_name.0.as_slice() {
            b"deposit" => {
                let owner = owner_of(&args[0]);
                let amount = i128_of(&args[1]);
                let ScVal::Bytes(id) = &args[2] else { panic!("deposit id") };
                let id: [u8; 32] = id.as_slice().try_into().unwrap();
                if !authorized.contains(&owner) {
                    return Err("owner did not authorize".to_owned());
                }
                if !state.deposits.insert((owner.clone(), id)) {
                    return Err("Error(Contract, #105)".to_owned());
                }
                let held = state.usdc.entry(owner.clone()).or_default();
                if *held < amount {
                    return Err("USDC balance too low".to_owned());
                }
                *held -= amount;
                state.treasury_usdc += amount;
                *state.accounts.entry(owner).or_insert(0) += amount;
                Ok((state, ScVal::Void, None))
            }
            // The USDC asset contract's own transfer, out of the treasury.
            b"transfer" => {
                let from = owner_of(&args[0]);
                let to = owner_of(&args[1]);
                let amount = i128_of(&args[2]);
                if from != treasury() || !authorized.contains(&from) {
                    return Err("sender did not authorize".to_owned());
                }
                if !state.lines.contains(&to) {
                    return Err("trustline missing".to_owned());
                }
                if state.treasury_usdc < amount {
                    return Err("USDC balance too low".to_owned());
                }
                state.treasury_usdc -= amount;
                *state.usdc.entry(to).or_default() += amount;
                Ok((state, ScVal::Void, None))
            }
            b"withdraw" => {
                let owner = owner_of(&args[0]);
                let amount = i128_of(&args[1]);
                let destination = owner_of(&args[2]);
                let ScVal::Bytes(id) = &args[3] else { panic!("withdrawal id") };
                let id: [u8; 32] = id.as_slice().try_into().unwrap();
                if !authorized.contains(&owner) {
                    return Err("owner did not authorize".to_owned());
                }
                if !authorized.contains(&treasury()) {
                    return Err("treasury did not authorize".to_owned());
                }
                if !state.withdrawals.insert((owner.clone(), id)) {
                    return Err("Error(Contract, #114)".to_owned());
                }
                let balance = state.accounts.get_mut(&owner).ok_or("Error(Contract, #107)")?;
                if *balance < amount {
                    return Err("Error(Contract, #108)".to_owned());
                }
                *balance -= amount;
                if state.treasury_usdc < amount {
                    return Err("USDC balance too low".to_owned());
                }
                state.treasury_usdc -= amount;
                *state.usdc.entry(destination).or_default() += amount;
                Ok((state, ScVal::Void, None))
            }
            b"charge_batch" => {
                if !authorized.contains(operator) {
                    return Err("operator did not authorize".to_owned());
                }
                let ScVal::Vec(Some(ScVec(charges))) = &args[0] else { panic!("charges") };
                let mut outcomes = Vec::new();
                let mut settled = Vec::new();
                let now = self.latest;
                for charge in charges.iter() {
                    let ScVal::Vec(Some(ScVec(fields))) = charge else { panic!("charge") };
                    let owner = owner_of(&fields[0]);
                    let ScVal::Bytes(id) = &fields[1] else { panic!("charge id") };
                    let id: [u8; 32] = id.as_slice().try_into().unwrap();
                    let amount = i128_of(&fields[2]);
                    let ScVal::U32(last_ledger) = fields[3] else { panic!("last ledger") };
                    if last_ledger > now + MAX_CHARGE_WINDOW {
                        return Err("Error(Contract, #118)".to_owned());
                    }
                    let recorded = state
                        .records
                        .get(&(owner.clone(), id))
                        .is_some_and(|(_, live_until, _)| *live_until >= now);
                    let code = if recorded {
                        3
                    } else if last_ledger < now {
                        4
                    } else {
                        let code = match state.accounts.get_mut(&owner) {
                            None => 5,
                            Some(_) if amount > MAX_CHARGE => 2,
                            Some(balance) if amount > *balance => 1,
                            Some(_) if self.over_daily.contains(&owner) => 6,
                            Some(balance) => {
                                *balance -= amount;
                                0
                            }
                        };
                        if !self.skip_records {
                            state.records.insert(
                                (owner.clone(), id),
                                (code, last_ledger + CHARGE_RECORD_GRACE, self.latest),
                            );
                        }
                        code
                    };
                    outcomes.push(ScVal::U32(code));
                    settled.push(ScVal::Vec(Some(ScVec(
                        vec![
                            fields[0].clone(),
                            fields[1].clone(),
                            fields[2].clone(),
                            ScVal::U32(code),
                        ]
                        .try_into()
                        .unwrap(),
                    ))));
                }
                if self.truncate_outcomes {
                    outcomes.pop();
                }
                // The contract's event carries every entry, whatever the
                // return value holds.
                let event = ScVal::Vec(Some(ScVec(settled.try_into().unwrap())));
                Ok((
                    state,
                    ScVal::Vec(Some(ScVec(outcomes.try_into().unwrap()))),
                    Some(("charges", event)),
                ))
            }
            b"authorize_recurring" => {
                let owner = owner_of(&args[0]);
                if !authorized.contains(&owner) {
                    return Err("owner did not authorize".to_owned());
                }
                let amount = i128_of(&args[2]);
                let (ScVal::U64(period_secs), ScVal::U32(cycles), ScVal::U32(live_until)) =
                    (&args[3], &args[4], &args[5])
                else {
                    panic!("mandate terms")
                };
                if *live_until < self.latest {
                    return Err("Error(Contract, #120)".to_owned());
                }
                let mandate = FakeMandate {
                    id: bytes32(&args[1]),
                    amount,
                    period_secs: *period_secs,
                    start: self.timestamp(),
                    cycles: *cycles,
                    live_until: *live_until,
                    next_cycle: 0,
                    since: self.latest,
                };
                state.allowances.insert(owner.clone(), (amount * i128::from(*cycles), *live_until));
                state.mandates.insert(owner, mandate);
                Ok((state, ScVal::Void, None))
            }
            b"revoke_recurring" => {
                let owner = owner_of(&args[0]);
                if !authorized.contains(&owner) {
                    return Err("owner did not authorize".to_owned());
                }
                state.mandates.remove(&owner);
                state.allowances.insert(owner, (0, 0));
                Ok((state, ScVal::Void, None))
            }
            b"charge_recurring_batch" => {
                if !authorized.contains(operator) {
                    return Err("operator did not authorize".to_owned());
                }
                let ScVal::Vec(Some(ScVec(charges))) = &args[0] else { panic!("charges") };
                let now = self.latest;
                let ts = self.timestamp();
                let mut outcomes = Vec::new();
                let mut settled = Vec::new();
                for charge in charges.iter() {
                    let owner = owner_of(field(charge, "owner"));
                    let id = bytes32(field(charge, "charge_id"));
                    let mandate_id = bytes32(field(charge, "mandate_id"));
                    let ScVal::U32(cycle) = *field(charge, "cycle") else { panic!("cycle") };
                    let amount = i128_of(field(charge, "amount"));
                    let ScVal::U32(last_ledger) = *field(charge, "last_ledger") else {
                        panic!("last ledger")
                    };
                    if last_ledger > now + MAX_CHARGE_WINDOW {
                        return Err("Error(Contract, #118)".to_owned());
                    }
                    let recorded = state
                        .recurring
                        .get(&(owner.clone(), id))
                        .is_some_and(|(_, live_until, _)| *live_until >= now);
                    let code = if recorded {
                        1
                    } else if last_ledger < now {
                        2
                    } else {
                        let allowance = state
                            .allowances
                            .get(&owner)
                            .filter(|(_, until)| *until >= now)
                            .map_or(0, |(a, _)| *a);
                        let wallet = state.usdc.get(&owner).copied().unwrap_or(0);
                        let code = match state.mandates.get_mut(&owner) {
                            Some(m) if m.id == mandate_id => {
                                let due = (ts - m.start) / m.period_secs;
                                if now > m.live_until
                                    || cycle >= m.cycles
                                    || due >= u64::from(m.cycles)
                                {
                                    4
                                } else if cycle < m.next_cycle {
                                    5
                                } else if u64::from(cycle) > due {
                                    6
                                } else if u64::from(cycle) < due {
                                    7
                                } else if amount > m.amount {
                                    8
                                } else if amount > MAX_CHARGE {
                                    9
                                } else if self.over_daily.contains(&owner) {
                                    10
                                } else if amount > allowance {
                                    11
                                } else if amount > wallet {
                                    12
                                } else {
                                    m.next_cycle = cycle + 1;
                                    0
                                }
                            }
                            _ => 3,
                        };
                        if code == 0 {
                            state.allowances.get_mut(&owner).unwrap().0 -= amount;
                            *state.usdc.get_mut(&owner).unwrap() -= amount;
                            state.treasury_usdc += amount;
                        }
                        if !self.skip_records {
                            state.recurring.insert(
                                (owner.clone(), id),
                                (code, last_ledger + CHARGE_RECORD_GRACE, self.latest),
                            );
                        }
                        code
                    };
                    outcomes.push(ScVal::U32(code));
                    settled.push(ScVal::Vec(Some(ScVec(
                        vec![
                            field(charge, "owner").clone(),
                            field(charge, "charge_id").clone(),
                            field(charge, "mandate_id").clone(),
                            ScVal::U32(cycle),
                            field(charge, "amount").clone(),
                            ScVal::U32(code),
                        ]
                        .try_into()
                        .unwrap(),
                    ))));
                }
                if self.truncate_outcomes {
                    outcomes.pop();
                }
                let event = ScVal::Vec(Some(ScVec(settled.try_into().unwrap())));
                Ok((
                    state,
                    ScVal::Vec(Some(ScVec(outcomes.try_into().unwrap()))),
                    Some(("recurring", event)),
                ))
            }
            other => panic!("unexpected call {}", String::from_utf8_lossy(other)),
        }
    }
}

fn outer_hash(envelope: &TransactionEnvelope) -> [u8; 32] {
    let TransactionEnvelope::TxFeeBump(bump) = envelope else { panic!("not a fee bump") };
    fee_bump_hash(&bump.tx, Network::Testnet).unwrap()
}

/// The operation meta holding the contract's `name` event.
fn contract_event(name: &str, data: ScVal) -> OperationMetaV2 {
    OperationMetaV2 {
        ext: ExtensionPoint::V0,
        changes: LedgerEntryChanges(VecM::default()),
        events: vec![ContractEvent {
            ext: ExtensionPoint::V0,
            contract_id: Some(ContractId(Hash(CONTRACT))),
            type_: ContractEventType::Contract,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: vec![symbol(name)].try_into().unwrap(),
                data,
            }),
        }]
        .try_into()
        .unwrap(),
    }
}

fn included(
    envelope: &TransactionEnvelope,
    ledger: u32,
    succeeded: bool,
    value: Option<ScVal>,
) -> Box<IncludedTransaction> {
    Box::new(IncludedTransaction {
        ledger,
        envelope: envelope.clone(),
        result: TransactionResult {
            fee_charged: 100,
            result: if succeeded {
                TransactionResultResult::TxSuccess(VecM::default())
            } else {
                TransactionResultResult::TxFailed(VecM::default())
            },
            ext: TransactionResultExt::V0,
        },
        meta: value.map(|value| {
            TransactionMeta::V4(TransactionMetaV4 {
                ext: ExtensionPoint::V0,
                tx_changes_before: LedgerEntryChanges(VecM::default()),
                operations: VecM::default(),
                tx_changes_after: LedgerEntryChanges(VecM::default()),
                soroban_meta: Some(SorobanTransactionMetaV2 {
                    ext: SorobanTransactionMetaExt::V0,
                    return_value: Some(value),
                }),
                events: VecM::default(),
                diagnostic_events: VecM::default(),
            })
        }),
    })
}

impl Stellar {
    fn with<R>(&self, f: impl FnOnce(&mut Net) -> R) -> R {
        f(&mut self.net.lock().unwrap())
    }

    fn set_latest(&self, ledger: u32) {
        self.with(|n| n.latest = ledger);
    }

    fn latest(&self) -> u32 {
        self.with(|n| n.latest)
    }

    fn account(&self, owner: &AccountAddress) -> Option<i128> {
        self.with(|n| n.state.accounts.get(owner).copied())
    }

    fn sent(&self) -> Vec<TransactionEnvelope> {
        self.with(|n| n.sent.clone())
    }

    /// Someone else includes the call and authorizations of `envelope` in a
    /// transaction of their own: the contract state changes, this gateway's
    /// source sequence does not.
    /// Logs a `charges` event in which the contract answered `expired` for
    /// the first charge `envelope` carries, as a sender naming the same
    /// charge with an earlier last ledger would see.
    fn log_expired_answer(&self, envelope: &TransactionEnvelope) {
        let (call, _, _) = invocation(envelope);
        let ScVal::Vec(Some(ScVec(charges))) = &call.args.as_slice()[0] else { panic!("charges") };
        let ScVal::Vec(Some(ScVec(fields))) = &charges[0] else { panic!("charge") };
        let entry = ScVal::Vec(Some(ScVec(
            vec![fields[0].clone(), fields[1].clone(), fields[2].clone(), ScVal::U32(4)]
                .try_into()
                .unwrap(),
        )));
        self.with(|n| {
            n.log_event("charges", ScVal::Vec(Some(ScVec(vec![entry].try_into().unwrap()))));
        });
    }

    fn include_elsewhere(&self, envelope: &TransactionEnvelope) {
        let (call, auth, _) = invocation(envelope);
        let operator = self.operator.clone();
        self.with(|n| {
            let (state, _, event) = n.execute(&call, &auth, &operator).unwrap();
            n.state = state;
            if let Some((name, data)) = event {
                n.log_event(name, data);
            }
        });
    }
}

impl Chain for Stellar {
    async fn account_sequence(&self, account: &AccountAddress) -> Result<SourceSequence, RpcError> {
        Ok(self.with(|n| SourceSequence {
            sequence: Some(n.sequences.get(account).copied().unwrap_or(n.source_sequence)),
            latest_ledger: n.latest - n.entries_behind,
        }))
    }

    async fn simulate(
        &self,
        envelope: &TransactionEnvelope,
    ) -> Result<SimulationOutcome, RpcError> {
        if let Some(barrier) = self.with(|n| n.simulation_barrier.clone()) {
            barrier.wait().await;
        }
        if extension(envelope).is_some() {
            let TransactionExt::V1(data) = &inner(envelope).ext else {
                panic!("an extension is simulated with its footprint")
            };
            return Ok(SimulationOutcome::Succeeded(Box::new(Simulation {
                transaction_data: data.clone(),
                min_resource_fee: 700,
                auth: Vec::new(),
                result: None,
                latest_ledger: self.with(|n| n.latest),
            })));
        }
        let (call, auth, _) = invocation(envelope);
        let archived = self.with(|n| n.touched_archived(&call));
        if !archived.is_empty() {
            let keys: Vec<LedgerKey> =
                archived.iter().map(|owner| self.deployment.account_key(owner)).collect();
            return Ok(SimulationOutcome::RestoreRequired {
                transaction_data: Box::new(SorobanTransactionData {
                    ext: SorobanTransactionDataExt::V0,
                    resources: SorobanResources {
                        footprint: LedgerFootprint {
                            read_only: VecM::default(),
                            read_write: keys.try_into().unwrap(),
                        },
                        instructions: 0,
                        disk_read_bytes: 0,
                        write_bytes: 0,
                    },
                    resource_fee: 0,
                }),
                min_resource_fee: 500,
            });
        }
        let operator = self.operator.clone();
        Ok(match self.with(|n| n.execute(&call, &auth, &operator).map(|_| n.latest)) {
            Err(error) => SimulationOutcome::Failed { error, latest_ledger: self.latest() },
            Ok(latest_ledger) => SimulationOutcome::Succeeded(Box::new(Simulation {
                transaction_data: SorobanTransactionData {
                    ext: SorobanTransactionDataExt::V0,
                    resources: SorobanResources {
                        footprint: LedgerFootprint {
                            read_only: VecM::default(),
                            read_write: VecM::default(),
                        },
                        instructions: 1,
                        disk_read_bytes: 0,
                        write_bytes: 0,
                    },
                    resource_fee: 0,
                },
                min_resource_fee: self.with(|n| n.resource_fee),
                auth: vec![],
                result: None,
                latest_ledger,
            })),
        })
    }

    async fn send(&self, envelope: &TransactionEnvelope) -> Result<SendOutcome, RpcError> {
        let hash = outer_hash(envelope);
        let sequence = inner(envelope).seq_num.0;
        let MuxedAccount::Ed25519(Uint256(source)) = inner(envelope).source_account else {
            panic!("muxed source")
        };
        let source = AccountAddress::from_public_key(source);
        let operator = self.operator.clone();
        let deployment = self.deployment.clone();
        self.with(|n| {
            n.sent.push(envelope.clone());
            if n.included.contains_key(&hash) {
                return Ok(SendOutcome::Duplicate { hash });
            }
            let current = n.sequences.get(&source).copied().unwrap_or(n.source_sequence);
            if sequence != current + 1 {
                return Ok(SendOutcome::Rejected {
                    hash,
                    result: Box::new(TransactionResult {
                        fee_charged: 0,
                        result: TransactionResultResult::TxBadSeq,
                        ext: TransactionResultExt::V0,
                    }),
                });
            }
            if n.stuck.contains(&source) {
                return Ok(SendOutcome::Pending { hash });
            }
            if n.drop_sends > 0 {
                n.drop_sends -= 1;
                return Ok(SendOutcome::Pending { hash });
            }
            match n.sequences.get_mut(&source) {
                Some(extra) => *extra = sequence,
                None => n.source_sequence = sequence,
            }
            let status = if n.fail_inclusions > 0 {
                n.fail_inclusions -= 1;
                TransactionStatus::Failed(included(envelope, n.latest, false, None))
            } else if let Some(extend_to) = extension(envelope) {
                let until = n.latest + extend_to;
                n.instance_live_until = n.instance_live_until.map(|now| now.max(until));
                n.code_live_until = n.code_live_until.max(until);
                TransactionStatus::Success(included(envelope, n.latest, true, None))
            } else if is_restore(envelope) {
                let tx = inner(envelope);
                let TransactionExt::V1(data) = &tx.ext else { panic!("restore without resources") };
                let restored: Vec<LedgerKey> = data.resources.footprint.read_write.to_vec();
                n.archived.retain(|owner| !restored.contains(&deployment.account_key(owner)));
                TransactionStatus::Success(included(envelope, n.latest, true, None))
            } else {
                let (call, auth, _) = invocation(envelope);
                if !n.touched_archived(&call).is_empty() {
                    TransactionStatus::Failed(included(envelope, n.latest, false, None))
                } else {
                    match n.execute(&call, &auth, &operator) {
                        Ok((state, value, event)) => {
                            n.state = state;
                            if let Some((name, data)) = &event {
                                n.log_event(name, data.clone());
                            }
                            let mut tx = included(envelope, n.latest, true, Some(value));
                            if let (Some((name, data)), Some(TransactionMeta::V4(meta))) =
                                (event, tx.meta.as_mut())
                            {
                                meta.operations =
                                    vec![contract_event(name, data)].try_into().unwrap();
                            }
                            TransactionStatus::Success(tx)
                        }
                        Err(_) => {
                            TransactionStatus::Failed(included(envelope, n.latest, false, None))
                        }
                    }
                }
            };
            n.included.insert(hash, status);
            Ok(SendOutcome::Pending { hash })
        })
    }

    async fn transaction(&self, hash: &[u8; 32]) -> Result<TransactionStatus, RpcError> {
        Ok(self.with(|n| {
            n.included.get(hash).cloned().unwrap_or(TransactionStatus::NotFound {
                node: NodeView {
                    latest_ledger: n.latest,
                    latest_close_time: n.clock.now().unix_timestamp(),
                    oldest_close_time: 0,
                },
            })
        }))
    }

    async fn latest_ledger(&self) -> Result<u32, RpcError> {
        Ok(self.latest())
    }

    async fn latest_ledger_info(&self) -> Result<LatestLedgerInfo, RpcError> {
        Ok(self.with(|n| LatestLedgerInfo {
            sequence: n.latest,
            close_time: n.clock.now().unix_timestamp(),
            protocol_version: 28,
            base_reserve: Some(BASE_RESERVE),
        }))
    }

    /// A quiet network: every recent Soroban transaction paid the minimum.
    async fn fee_stats(&self) -> Result<FeeStats, RpcError> {
        let quiet = FeeDistribution {
            max: 100,
            min: 100,
            mode: 100,
            p10: 100,
            p20: 100,
            p30: 100,
            p40: 100,
            p50: 100,
            p60: 100,
            p70: 100,
            p80: 100,
            p90: 100,
            p95: 100,
            p99: 100,
            transaction_count: 10,
            ledger_count: 50,
        };
        Ok(FeeStats {
            soroban_inclusion_fee: quiet,
            inclusion_fee: quiet,
            latest_ledger: self.latest(),
        })
    }

    async fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<LedgerEntries, RpcError> {
        // The real RPC refuses a read of no keys.
        if keys.is_empty() {
            return Err(RpcError::Server {
                method: "getLedgerEntries",
                code: -32603,
                message: "could not query captive core: no keys specified in request".to_owned(),
            });
        }
        let contract_data = |key: &LedgerKey, val: ScVal| {
            let LedgerKey::ContractData(LedgerKeyContractData {
                contract,
                key: data_key,
                durability,
            }) = key
            else {
                unreachable!()
            };
            LedgerEntryRecord {
                key: key.clone(),
                data: LedgerEntryData::ContractData(ContractDataEntry {
                    ext: ExtensionPoint::V0,
                    contract: contract.clone(),
                    key: data_key.clone(),
                    durability: *durability,
                    val,
                }),
                ext: LedgerEntryExt::V0,
                last_modified_ledger: 1,
                live_until_ledger: None,
            }
        };
        let state = self.with(|n| n.state.clone());
        let mut existing: HashMap<LedgerKey, ScVal> = HashMap::new();
        let read_at = self.with(|n| n.latest - n.entries_behind);
        for (owner, balance) in &state.accounts {
            let value = ScVal::Map(Some(ScMap(
                vec![ScMapEntry {
                    key: symbol("balance"),
                    val: ScVal::I128(Int128Parts { hi: 0, lo: u64::try_from(*balance).unwrap() }),
                }]
                .try_into()
                .unwrap(),
            )));
            existing.insert(self.deployment.account_key(owner), value);
        }
        for ((owner, id), (code, live_until, since)) in &state.records {
            if *since <= read_at && *live_until >= read_at {
                existing.insert(self.deployment.charge_record_key(owner, id), ScVal::U32(*code));
            }
        }
        for (owner, id) in &state.withdrawals {
            existing.insert(self.deployment.withdrawal_key(owner, id), ScVal::Void);
        }
        for (owner, id) in &state.deposits {
            existing.insert(self.deployment.deposit_key(owner, id), ScVal::Void);
        }
        for (owner, m) in state.mandates.iter().filter(|(_, m)| m.since <= read_at) {
            let entry = |name: &str, val: ScVal| ScMapEntry { key: symbol(name), val };
            let value = ScVal::Map(Some(ScMap(
                vec![
                    entry(
                        "amount",
                        ScVal::I128(Int128Parts { hi: 0, lo: u64::try_from(m.amount).unwrap() }),
                    ),
                    entry("cycles", ScVal::U32(m.cycles)),
                    entry("live_until", ScVal::U32(m.live_until)),
                    entry("mandate_id", ScVal::Bytes(m.id.to_vec().try_into().unwrap())),
                    entry("next_cycle", ScVal::U32(m.next_cycle)),
                    entry("period_secs", ScVal::U64(m.period_secs)),
                    entry("start", ScVal::U64(m.start)),
                ]
                .try_into()
                .unwrap(),
            )));
            existing.insert(self.deployment.mandate_key(owner), value);
        }
        for ((owner, id), (code, live_until, since)) in &state.recurring {
            if *since <= read_at && *live_until >= read_at {
                existing.insert(self.deployment.recurring_record_key(owner, id), ScVal::U32(*code));
            }
        }
        let fee_balance = self.with(|n| n.fee_balance);
        let (instance_until, code_until) =
            self.with(|n| (n.instance_live_until, n.code_live_until));
        let instance_key = self.deployment.instance_key();
        let code_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: Hash(WASM) });
        let contract_entry = |key: &LedgerKey| {
            let until = instance_until?;
            if *key == instance_key {
                let mut record = contract_data(
                    key,
                    ScVal::ContractInstance(ScContractInstance {
                        executable: ContractExecutable::Wasm(Hash(WASM)),
                        storage: None,
                    }),
                );
                record.live_until_ledger = Some(until);
                Some(record)
            } else if *key == code_key {
                Some(LedgerEntryRecord {
                    key: key.clone(),
                    data: LedgerEntryData::ContractCode(ContractCodeEntry {
                        ext: ContractCodeEntryExt::V0,
                        hash: Hash(WASM),
                        code: BytesM::default(),
                    }),
                    ext: LedgerEntryExt::V0,
                    last_modified_ledger: 1,
                    live_until_ledger: Some(code_until),
                })
            } else {
                None
            }
        };
        let account = |key: &LedgerKey| {
            let LedgerKey::Account(LedgerKeyAccount { account_id }) = key else { return None };
            Some(LedgerEntryRecord {
                key: key.clone(),
                data: LedgerEntryData::Account(AccountEntry {
                    account_id: account_id.clone(),
                    balance: fee_balance,
                    seq_num: SequenceNumber(1),
                    num_sub_entries: 0,
                    inflation_dest: None,
                    flags: 0,
                    home_domain: String32::default(),
                    thresholds: Thresholds([1, 0, 0, 0]),
                    signers: VecM::default(),
                    ext: AccountEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
                last_modified_ledger: 1,
                live_until_ledger: None,
            })
        };
        let stale_treasury = self.with(|n| n.stale_treasury);
        let trustline = |key: &LedgerKey| {
            let LedgerKey::Trustline(line) = key else { return None };
            let holder = address_of(&line.account_id);
            let balance = if holder == treasury() {
                stale_treasury.unwrap_or(state.treasury_usdc)
            } else if state.lines.contains(&holder) {
                state.usdc.get(&holder).copied().unwrap_or(0)
            } else {
                return None;
            };
            Some(LedgerEntryRecord {
                key: key.clone(),
                data: LedgerEntryData::Trustline(TrustLineEntry {
                    account_id: line.account_id.clone(),
                    asset: line.asset.clone(),
                    balance: i64::try_from(balance).unwrap(),
                    limit: i64::MAX,
                    flags: 1,
                    ext: TrustLineEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
                last_modified_ledger: 1,
                live_until_ledger: None,
            })
        };
        Ok(LedgerEntries {
            entries: keys
                .iter()
                .filter_map(|key| {
                    account(key)
                        .or_else(|| trustline(key))
                        .or_else(|| contract_entry(key))
                        .or_else(|| existing.get(key).map(|val| contract_data(key, val.clone())))
                })
                .collect(),
            latest_ledger: self.with(|n| n.latest - n.entries_behind),
        })
    }
}

impl EventLog for Stellar {
    async fn health(&self) -> Result<Health, RpcError> {
        Ok(self.with(|n| Health { latest_ledger: n.latest, oldest_ledger: n.oldest }))
    }

    async fn events(
        &self,
        contract: &[u8; 32],
        from: &EventsFrom,
        limit: u32,
    ) -> Result<EventPage, RpcError> {
        self.with(|n| n.events.page(contract, from, limit, n.oldest, n.latest))
    }
}

impl LatestLedger for Stellar {
    async fn latest_ledger(&self) -> Result<u32, RpcError> {
        Ok(self.latest())
    }

    async fn latest_close_time(&self) -> Result<i64, RpcError> {
        Ok(self.with(|n| n.clock.now().unix_timestamp()))
    }

    // Buyers here hold classic accounts, whose entries are verified without
    // the network.
    async fn simulate_buyer_call(
        &self,
        _source: &AccountAddress,
        _call: fermah_pay_stellar_chain::stellar_xdr::InvokeContractArgs,
        _auth: Vec<fermah_pay_stellar_chain::stellar_xdr::SorobanAuthorizationEntry>,
    ) -> Result<fermah_pay_stellar_gateway::ledger::BuyerCall, RpcError> {
        unreachable!("no buyer here holds a contract account")
    }
}

#[derive(Clone)]
struct ManualClock(Arc<Mutex<OffsetDateTime>>);

impl ManualClock {
    fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }
}

// ---- harness ------------------------------------------------------------

struct World {
    h: Harness,
    tenant: Tenant,
    stellar: Stellar,
    clock: ManualClock,
    worker_pool: PgPool,
    operator_pool: PgPool,
    operator_seed: String,
    source_seed: String,
    fee_seed: String,
}

struct TestBuyer {
    id: String,
    key: SecretKey,
}

/// The treasury's key, the same for every test in this binary.
static TREASURY_SEED: LazyLock<String> =
    LazyLock::new(|| SecretKey::generate().unwrap().to_strkey().to_string());

fn treasury_key() -> SecretKey {
    SecretKey::from_strkey(&TREASURY_SEED).unwrap()
}

fn treasury() -> AccountAddress {
    treasury_key().address()
}

async fn world(opts: PgPoolOptions, connect: PgConnectOptions) -> World {
    world_with(opts, connect, false).await
}

/// A world whose gateway lets withdrawals pay other accounts when
/// `other_destinations`, which production refuses by default.
async fn world_with(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
    other_destinations: bool,
) -> World {
    let operator = SecretKey::generate().unwrap();
    let clock =
        ManualClock(Arc::new(Mutex::new(OffsetDateTime::now_utc().replace_nanosecond(0).unwrap())));
    let usdc = fermah_pay_stellar_chain::usdc::asset_contract_id(
        &fermah_pay_stellar_chain::usdc::circle_usdc(Network::Testnet),
        Network::Testnet,
    );
    let stellar = Stellar {
        net: Arc::new(Mutex::new(Net {
            latest: START_LEDGER,
            source_sequence: 500,
            sequences: HashMap::new(),
            state: ContractState::default(),
            included: HashMap::new(),
            sent: Vec::new(),
            drop_sends: 0,
            stuck: HashSet::new(),
            fee_balance: 100_000_000_000,
            instance_live_until: None,
            code_live_until: 0,
            fail_inclusions: 0,
            truncate_outcomes: false,
            skip_records: false,
            simulation_barrier: None,
            archived: HashSet::new(),
            entries_behind: 0,
            resource_fee: 1_000,
            stale_treasury: None,
            over_daily: HashSet::new(),
            clock: clock.clone(),
            events: EventStream::default(),
            oldest: 1,
        })),
        deployment: PrepaidDeployment { contract: CONTRACT, usdc, treasury: treasury() },
        operator: operator.address(),
    };
    let h = start_with_options(
        opts,
        connect.clone(),
        Network::Testnet,
        stellar.clone(),
        Ledger::default(),
        Quotas { min_withdrawal: 1, ..Quotas::default() },
        other_destinations,
    )
    .await;
    let tenant = h.tenant("shop", "main", Network::Testnet).await;
    let binding = LedgerBinding {
        contract: stellar_strkey::Contract(CONTRACT).to_string().to_string(),
        treasury: treasury(),
        operator: operator.address(),
    };
    issuance::bind_ledger_contract(&h.issuer, tenant.deployment_id, &binding).await.unwrap();
    World {
        h,
        tenant,
        stellar,
        clock,
        worker_pool: pool_as(&connect, "SET ROLE pay_stellar_worker", 3).await,
        operator_pool: pool_as(&connect, "SET ROLE pay_stellar_operator", 1).await,
        operator_seed: operator.to_strkey().to_string(),
        source_seed: SecretKey::generate().unwrap().to_strkey().to_string(),
        fee_seed: SecretKey::generate().unwrap().to_strkey().to_string(),
    }
}

impl World {
    /// A worker process with its own source account, as after a restart
    /// when `source_seed` is reused.
    fn worker_with(&self, source_seed: &str, max_batch: usize) -> Worker<Stellar, ManualClock> {
        self.worker_with_sources(vec![SecretKey::from_strkey(source_seed).unwrap()], max_batch)
    }

    /// A worker holding several source accounts, sending from each free one.
    fn worker_with_sources(
        &self,
        sources: Vec<SecretKey>,
        max_batch: usize,
    ) -> Worker<Stellar, ManualClock> {
        let engine = Engine::new(
            self.worker_pool.clone(),
            self.stellar.clone(),
            self.clock.clone(),
            Network::Testnet,
            Keys::new(
                sources.into_iter().map(LocalSigner::arc).collect(),
                LocalSigner::arc(SecretKey::from_strkey(&self.fee_seed).unwrap()),
            )
            .unwrap(),
            Policy {
                fees: FeePolicy::new(100, 100_000, FeePercentile::P90).unwrap(),
                resource_fee_margin_percent: 10,
                validity: VALIDITY,
                max_clock_skew: Duration::from_secs(20),
                max_buyer_resource_fee:
                    fermah_pay_stellar_gateway::submission::DEFAULT_MAX_BUYER_RESOURCE_FEE,
            },
        );
        Worker::new(
            engine,
            self.worker_pool.clone(),
            LocalSigner::arc(SecretKey::from_strkey(&self.operator_seed).unwrap()),
            Settings {
                operator_authorization_ledgers: OPERATOR_LEDGERS,
                retry_after: RETRY_AFTER,
                max_batch,
                fee_floor_stroops: FEE_FLOOR,
                ttl_threshold_ledgers: TTL_THRESHOLD,
                ttl_extend_to_ledgers: TTL_EXTEND_TO,
                ttl_check_every: TTL_CHECK_EVERY,
            },
        )
    }

    fn worker(&self) -> Worker<Stellar, ManualClock> {
        self.worker_with(&self.source_seed, 100)
    }

    /// A worker that also holds the treasury's key, so it sends withdrawals.
    fn paying_worker(&self) -> Worker<Stellar, ManualClock> {
        self.worker().with_treasury(LocalSigner::arc(treasury_key()))
    }

    /// Prepares a withdrawal and returns it with the buyer's signed entry.
    async fn prepared_withdrawal(
        &self,
        buyer: &TestBuyer,
        amount: i64,
        key: &str,
    ) -> Result<(Withdrawal, String), tonic::Status> {
        let request = PrepareWithdrawalRequest {
            buyer_id: buyer.id.clone(),
            amount,
            destination: String::new(),
            idempotency_key: key.to_owned(),
        };
        let withdrawal = self
            .ledger()
            .await
            .prepare_withdrawal(authed(request, &self.tenant.token))
            .await?
            .into_inner()
            .withdrawal
            .unwrap();
        let entry = SorobanAuthorizationEntry::from_xdr_base64(
            &withdrawal.authorization_entry_xdr,
            Limits::none(),
        )
        .unwrap();
        let signed = sign_entry(&entry, network_id(Network::Testnet), &[&buyer.key]).unwrap();
        Ok((withdrawal, signed.to_xdr_base64(Limits::none()).unwrap()))
    }

    async fn submit_withdrawal(
        &self,
        withdrawal: &Withdrawal,
        signed: &str,
    ) -> Result<Withdrawal, tonic::Status> {
        let request = SubmitWithdrawalRequest {
            withdrawal_id: withdrawal.withdrawal_id.clone(),
            signed_authorization_entry_xdr: signed.to_owned(),
        };
        Ok(self
            .ledger()
            .await
            .submit_withdrawal(authed(request, &self.tenant.token))
            .await?
            .into_inner()
            .withdrawal
            .unwrap())
    }

    /// A withdrawal to the buyer's wallet, signed and held.
    async fn withdraw(&self, buyer: &TestBuyer, amount: i64, key: &str) -> Withdrawal {
        let (withdrawal, signed) = self.prepared_withdrawal(buyer, amount, key).await.unwrap();
        self.submit_withdrawal(&withdrawal, &signed).await.unwrap()
    }

    async fn get_withdrawal(&self, id: &str) -> Withdrawal {
        self.ledger()
            .await
            .get_withdrawal(authed(
                GetWithdrawalRequest { withdrawal_id: id.to_owned() },
                &self.tenant.token,
            ))
            .await
            .unwrap()
            .into_inner()
            .withdrawal
            .unwrap()
    }

    /// What the API reports held for withdrawals not yet final.
    async fn withdrawing(&self, buyer: &TestBuyer) -> i64 {
        self.ledger()
            .await
            .get_balance(authed(
                GetBalanceRequest { buyer_id: buyer.id.clone() },
                &self.tenant.token,
            ))
            .await
            .unwrap()
            .into_inner()
            .pending_withdrawals
    }

    async fn ledger(&self) -> LedgerServiceClient<tonic::transport::Channel> {
        LedgerServiceClient::new(self.h.channel().await)
    }

    async fn buyer(&self, name: &str, usdc: i128) -> TestBuyer {
        let key = SecretKey::generate().unwrap();
        let request = CreateBuyerRequest {
            external_ref: name.to_owned(),
            wallet_address: key.address().to_string(),
        };
        let created = BuyerServiceClient::new(self.h.channel().await)
            .create_buyer(authed(request, &self.tenant.token))
            .await
            .unwrap()
            .into_inner();
        self.stellar.with(|n| n.state.usdc.insert(key.address(), usdc));
        TestBuyer { id: created.buyer.unwrap().buyer_id, key }
    }

    /// Prepares a deposit and returns it with the buyer's signed entry.
    async fn prepared_deposit(
        &self,
        buyer: &TestBuyer,
        amount: i64,
        key: &str,
    ) -> (Deposit, String) {
        let request = PrepareDepositRequest {
            buyer_id: buyer.id.clone(),
            amount,
            idempotency_key: key.to_owned(),
        };
        let deposit = self
            .ledger()
            .await
            .prepare_deposit(authed(request, &self.tenant.token))
            .await
            .unwrap()
            .into_inner()
            .deposit
            .unwrap();
        let entry = SorobanAuthorizationEntry::from_xdr_base64(
            &deposit.authorization_entry_xdr,
            Limits::none(),
        )
        .unwrap();
        let signed = sign_entry(&entry, network_id(Network::Testnet), &[&buyer.key]).unwrap();
        (deposit, signed.to_xdr_base64(Limits::none()).unwrap())
    }

    async fn deposit(&self, buyer: &TestBuyer, amount: i64, key: &str) -> Deposit {
        let (deposit, signed) = self.prepared_deposit(buyer, amount, key).await;
        let request = SubmitDepositRequest {
            deposit_id: deposit.deposit_id,
            signed_authorization_entry_xdr: signed,
        };
        self.ledger()
            .await
            .submit_deposit(authed(request, &self.tenant.token))
            .await
            .unwrap()
            .into_inner()
            .deposit
            .unwrap()
    }

    async fn get_deposit(&self, id: &str) -> Deposit {
        self.ledger()
            .await
            .get_deposit(authed(
                GetDepositRequest { deposit_id: id.to_owned() },
                &self.tenant.token,
            ))
            .await
            .unwrap()
            .into_inner()
            .deposit
            .unwrap()
    }

    async fn charge(&self, buyer: &TestBuyer, amount: i64, key: &str) -> Charge {
        let request = CreateChargeRequest {
            buyer_id: buyer.id.clone(),
            amount,
            idempotency_key: key.to_owned(),
        };
        self.ledger()
            .await
            .create_charge(authed(request, &self.tenant.token))
            .await
            .unwrap()
            .into_inner()
            .charge
            .unwrap()
    }

    async fn get_charge(&self, id: &str) -> Charge {
        self.ledger()
            .await
            .get_charge(authed(GetChargeRequest { charge_id: id.to_owned() }, &self.tenant.token))
            .await
            .unwrap()
            .into_inner()
            .charge
            .unwrap()
    }

    /// (available, pending charges) as the API reports them.
    async fn balance(&self, buyer: &TestBuyer) -> (i64, i64) {
        let reply = self
            .ledger()
            .await
            .get_balance(authed(
                GetBalanceRequest { buyer_id: buyer.id.clone() },
                &self.tenant.token,
            ))
            .await
            .unwrap()
            .into_inner();
        (reply.available, reply.pending_charges)
    }

    /// Steps until the worker has nothing left to do.
    async fn settle(&self, worker: &Worker<Stellar, ManualClock>) {
        for _ in 0..20 {
            if worker.step().await.unwrap() == Step::Idle {
                return;
            }
        }
        panic!("worker never went idle");
    }

    /// A buyer with `amount` confirmed on-chain and credited.
    async fn funded(&self, name: &str, amount: i64) -> TestBuyer {
        let buyer = self.buyer(name, i128::from(amount)).await;
        let deposit = self.deposit(&buyer, amount, &format!("fund-{name}")).await;
        self.settle(&self.worker()).await;
        assert_eq!(self.get_deposit(&deposit.deposit_id).await.state(), DepositState::Confirmed);
        buyer
    }

    async fn last_error(&self, table: &str, id: &str) -> Option<String> {
        let query = match table {
            "deposits" => "SELECT last_error FROM pay_stellar.deposits WHERE id = $1::uuid",
            "charges" => "SELECT last_error FROM pay_stellar.charges WHERE id = $1::uuid",
            "withdrawals" => "SELECT last_error FROM pay_stellar.withdrawals WHERE id = $1::uuid",
            other => panic!("no table {other}"),
        };
        sqlx::query_scalar(query).bind(id).fetch_one(&self.h.owner).await.unwrap()
    }
}

// ---- deposits -------------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_not_included_is_sent_again_and_credited_once(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let deposit = w.deposit(&buyer, 100, "d-1").await;
    let worker = w.worker();
    // Lost on the first send and on the resend that recovery makes.
    w.stellar.with(|n| n.drop_sends = 2);
    assert!(matches!(worker.step().await.unwrap(), Step::Submitted(_)));
    assert_eq!(worker.step().await.unwrap(), Step::InFlight);

    // The window closes with the source sequence untouched: proven not
    // included, while the buyer's signature is still valid.
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.settle(&worker).await;

    let settled = w.get_deposit(&deposit.deposit_id).await;
    assert_eq!(settled.state(), DepositState::Confirmed);
    assert_eq!(w.balance(&buyer).await, (100, 0));
    assert_eq!(w.stellar.account(&buyer.key.address()), Some(100));
    // The same signed entry went out again in a new transaction.
    let sent = w.stellar.sent();
    assert_eq!(sent.len(), 3);
    assert_ne!(sent[0], sent[2]);
    assert_eq!(invocation(&sent[0]).1, invocation(&sent[2]).1);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_included_elsewhere_is_credited_once_from_the_contract_marker(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let deposit = w.deposit(&buyer, 100, "d-1").await;
    let worker = w.worker();
    w.stellar.with(|n| n.drop_sends = 1);
    worker.step().await.unwrap();
    // A copy of the broadcast authorization lands in someone else's
    // transaction; ours never does.
    w.stellar.include_elsewhere(&w.stellar.sent()[0]);
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.settle(&worker).await;

    // Resending is refused (the nonce is spent), so the deposit waits.
    let waiting = w.get_deposit(&deposit.deposit_id).await;
    assert_eq!(waiting.state(), DepositState::Signed);
    assert_eq!(w.balance(&buyer).await, (0, 0));

    w.stellar.set_latest(deposit.expiration_ledger + 1);
    w.settle(&worker).await;
    let settled = w.get_deposit(&deposit.deposit_id).await;
    assert_eq!(settled.state(), DepositState::Confirmed);
    assert_eq!(w.balance(&buyer).await, (100, 0));
    assert_eq!(w.stellar.account(&buyer.key.address()), Some(100));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_included_as_failed_is_decided_only_after_its_authorization_lapses(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let failed = w.deposit(&buyer, 60, "d-1").await;
    let worker = w.worker();
    w.stellar.with(|n| n.fail_inclusions = 1);
    w.settle(&worker).await;
    // Included as failed, but the signed entry could still be included
    // elsewhere: no verdict yet, and nothing is sent again.
    assert_eq!(w.get_deposit(&failed.deposit_id).await.state(), DepositState::Submitted);
    assert_eq!(w.stellar.sent().len(), 1);

    // The network is past the expiration, but the node serving the marker
    // read is not: an absent marker there proves nothing yet.
    w.stellar.set_latest(failed.expiration_ledger + 1);
    w.stellar.with(|n| n.entries_behind = 1);
    w.settle(&worker).await;
    assert_eq!(w.get_deposit(&failed.deposit_id).await.state(), DepositState::Submitted);

    w.stellar.with(|n| n.entries_behind = 0);
    w.settle(&worker).await;
    let settled = w.get_deposit(&failed.deposit_id).await;
    assert_eq!(settled.state(), DepositState::Failed);
    assert_eq!(w.balance(&buyer).await, (0, 0));
    assert_eq!(w.stellar.account(&buyer.key.address()), None);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_refused_in_simulation_is_retried_after_the_pause(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 0).await;
    let deposit = w.deposit(&buyer, 100, "d-1").await;
    let worker = w.worker();
    assert_eq!(worker.step().await.unwrap(), Step::Idle);
    assert_eq!(w.get_deposit(&deposit.deposit_id).await.state(), DepositState::Signed);
    let error = w.last_error("deposits", &deposit.deposit_id).await.unwrap();
    assert!(error.contains("USDC balance too low"), "{error}");

    // Funded now, but still set aside until the pause ends.
    w.stellar.with(|n| n.state.usdc.insert(buyer.key.address(), 100));
    assert_eq!(worker.step().await.unwrap(), Step::Idle);
    w.clock.advance(RETRY_AFTER + Duration::from_secs(1));
    w.settle(&worker).await;
    assert_eq!(w.get_deposit(&deposit.deposit_id).await.state(), DepositState::Confirmed);
    assert_eq!(w.balance(&buyer).await, (100, 0));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_row_changed_after_signing_is_not_sent(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 1_000).await;
    let deposit = w.deposit(&buyer, 100, "d-1").await;
    // No runtime role may change an amount; the database owner can.
    sqlx::query("UPDATE pay_stellar.deposits SET amount = 1000").execute(&w.h.owner).await.unwrap();
    assert_eq!(w.worker().step().await.unwrap(), Step::Idle);
    assert!(w.stellar.sent().is_empty());
    assert_eq!(w.get_deposit(&deposit.deposit_id).await.state(), DepositState::Signed);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_lapsed_deposit_expires_unless_the_buyer_included_it_themselves(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer("alice", 100).await;
    let bob = w.buyer("bob", 100).await;
    let (abandoned, _) = w.prepared_deposit(&alice, 100, "d-a").await;
    let (self_sent, signed) = w.prepared_deposit(&bob, 100, "d-b").await;
    // Bob never calls SubmitDeposit; his wallet includes the entry itself.
    let entry = SorobanAuthorizationEntry::from_xdr_base64(&signed, Limits::none()).unwrap();
    let call = InvokeContractArgs {
        contract_address: ScAddress::Contract(ContractId(Hash(CONTRACT))),
        function_name: ScSymbol("deposit".try_into().unwrap()),
        args: match &entry.root_invocation.function {
            fermah_pay_stellar_chain::stellar_xdr::SorobanAuthorizedFunction::ContractFn(args) => {
                args.args.clone()
            }
            _ => unreachable!(),
        },
    };
    let operator = w.stellar.operator.clone();
    w.stellar.with(|n| {
        let (state, _, _) = n.execute(&call, &[entry], &operator).unwrap();
        n.state = state;
    });

    let worker = w.worker();
    w.settle(&worker).await;
    assert_eq!(w.get_deposit(&abandoned.deposit_id).await.state(), DepositState::AwaitingSignature);

    w.stellar.set_latest(abandoned.expiration_ledger.max(self_sent.expiration_ledger) + 1);
    w.settle(&worker).await;
    assert_eq!(w.get_deposit(&abandoned.deposit_id).await.state(), DepositState::Expired);
    assert_eq!(w.balance(&alice).await, (0, 0));
    assert_eq!(w.get_deposit(&self_sent.deposit_id).await.state(), DepositState::Confirmed);
    assert_eq!(w.balance(&bob).await, (100, 0));
    assert!(w.stellar.sent().is_empty());
}

// ---- charges --------------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_one_batch_charges_refunds_refusals_and_matches_the_contract(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let y = w.funded("y", 100).await;
    let z = w.funded("z", 100).await;
    // Z's on-chain balance drops below the gateway's view, as after a
    // withdrawal the gateway did not see.
    w.stellar.with(|n| *n.state.accounts.get_mut(&z.key.address()).unwrap() = 10);
    let charged = w.charge(&x, 30, "c-x").await;
    let above = w.charge(&y, MAX_CHARGE as i64 + 10, "c-y").await;
    let short = w.charge(&z, 20, "c-z").await;
    let sends_before = w.stellar.sent().len();
    w.settle(&w.worker()).await;

    assert_eq!(w.stellar.sent().len(), sends_before + 1, "one transaction settles the batch");
    let settled = [
        w.get_charge(&charged.charge_id).await,
        w.get_charge(&above.charge_id).await,
        w.get_charge(&short.charge_id).await,
    ];
    let states: Vec<_> = settled.iter().map(|c| (c.state(), c.outcome.as_str())).collect();
    assert_eq!(
        states,
        [
            (ChargeState::Charged, "charged"),
            (ChargeState::Refused, "above_limit"),
            (ChargeState::Refused, "insufficient_balance"),
        ]
    );
    assert!(settled.iter().all(|c| c.transaction_hash == settled[0].transaction_hash));
    assert_eq!(w.balance(&x).await, (70, 0));
    assert_eq!(w.balance(&y).await, (100, 0));
    assert_eq!(w.balance(&z).await, (100, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some(70));
    assert_eq!(w.stellar.account(&y.key.address()), Some(100));
    assert_eq!(w.stellar.account(&z.key.address()), Some(10));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_duplicate_answer_is_settled_from_the_charge_record(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 10, "c-x1").await;
    // The charge already landed through someone else's transaction: the
    // contract debited it and recorded it as charged.
    let id = hash_of(&charge.contract_charge_id);
    w.stellar.with(|n| {
        *n.state.accounts.get_mut(&x.key.address()).unwrap() -= 10;
        n.state
            .records
            .insert((x.key.address(), id), (0, charge.last_ledger + CHARGE_RECORD_GRACE, n.latest));
    });
    w.settle(&w.worker()).await;
    let settled = w.get_charge(&charge.charge_id).await;
    assert_eq!((settled.state(), settled.outcome.as_str()), (ChargeState::Charged, "charged"));
    assert_eq!(w.balance(&x).await, (90, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some(90));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_unknown_account_answer_is_quarantined_without_blocking_others(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let y = w.funded("y", 100).await;
    // Y's account is gone from the contract although a deposit created it.
    w.stellar.with(|n| n.state.accounts.remove(&y.key.address()));
    let lost = w.charge(&y, 10, "c-y1").await;
    let fine = w.charge(&x, 10, "c-x1").await;
    let worker = w.worker();
    w.settle(&worker).await;
    let quarantined = w.get_charge(&lost.charge_id).await;
    assert_eq!(
        (quarantined.state(), quarantined.outcome.as_str()),
        (ChargeState::Quarantined, "unknown_account")
    );
    // The amount stays debited while an operator reviews; X is unaffected,
    // and so are Y's other charges once the account is back.
    assert_eq!(w.balance(&y).await, (90, 0));
    assert_eq!(w.get_charge(&fine.charge_id).await.state(), ChargeState::Charged);
    w.stellar.with(|n| n.state.accounts.insert(y.key.address(), 100));
    let next = w.charge(&y, 10, "c-y2").await;
    w.settle(&worker).await;
    assert_eq!(w.get_charge(&next.charge_id).await.state(), ChargeState::Charged);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_batch_not_included_is_requeued_only_after_the_operator_authorization_lapses(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let worker = w.worker();
    w.stellar.with(|n| n.drop_sends = 1);
    let sends_before = w.stellar.sent().len();
    worker.step().await.unwrap();
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.settle(&worker).await;
    // Not included, but its authorization could still be: it waits.
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Submitted);
    assert_eq!(w.stellar.sent().len(), sends_before + 1);

    // Past the horizon on the network, not yet on the node read.
    w.stellar.set_latest(START_LEDGER + OPERATOR_LEDGERS + 1);
    w.stellar.with(|n| n.entries_behind = 1);
    w.settle(&worker).await;
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Submitted);

    w.stellar.with(|n| n.entries_behind = 0);
    w.settle(&worker).await;
    let settled = w.get_charge(&charge.charge_id).await;
    assert_eq!(settled.state(), ChargeState::Charged);
    assert_eq!(settled.contract_charge_id, charge.contract_charge_id);
    assert_eq!(w.balance(&x).await, (70, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some(70));
}

/// The inclusion bid per operation of a sent fee bump: its fee less the
/// inner resource fee, over the inner operation and the fee bump.
fn bid_of(envelope: &TransactionEnvelope) -> i64 {
    let TransactionEnvelope::TxFeeBump(bump) = envelope else { panic!("not a fee bump") };
    let TransactionExt::V1(data) = &inner(envelope).ext else { panic!("not assembled") };
    (bump.tx.fee - data.resource_fee) / 2
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_batch_rebuilt_after_its_envelope_expired_bids_twice_as_much(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let worker = w.worker();
    w.stellar.with(|n| n.drop_sends = 1);
    let sends_before = w.stellar.sent().len();
    worker.step().await.unwrap();
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.settle(&worker).await;
    // Requeued once the batch's authorization lapsed, and sent again.
    w.stellar.set_latest(START_LEDGER + OPERATOR_LEDGERS + 1);
    w.settle(&worker).await;
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Charged);
    // The next batch, after one that landed, bids from the market again.
    let later = w.charge(&x, 10, "c-2").await;
    w.settle(&worker).await;
    assert_eq!(w.get_charge(&later.charge_id).await.state(), ChargeState::Charged);
    let bids: Vec<i64> = w.stellar.sent()[sends_before..].iter().map(bid_of).collect();
    assert_eq!(bids, [100, 200, 100]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_batch_applied_elsewhere_is_settled_from_its_records_not_charged_again(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let worker = w.worker();
    w.stellar.with(|n| n.drop_sends = 1);
    worker.step().await.unwrap();
    w.stellar.include_elsewhere(w.stellar.sent().last().unwrap());
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.stellar.set_latest(START_LEDGER + OPERATOR_LEDGERS + 1);
    w.settle(&worker).await;

    let settled = w.get_charge(&charge.charge_id).await;
    assert_eq!((settled.state(), settled.outcome.as_str()), (ChargeState::Charged, "charged"));
    assert_eq!(w.balance(&x).await, (70, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some(70));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_unreadable_batch_answer_is_settled_from_the_records(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let first = w.charge(&x, 10, "c-1").await;
    let above = w.charge(&x, MAX_CHARGE as i64 + 10, "c-2").await;
    w.stellar.with(|n| n.truncate_outcomes = true);
    w.settle(&w.worker()).await;
    let first = w.get_charge(&first.charge_id).await;
    let above = w.get_charge(&above.charge_id).await;
    assert_eq!((first.state(), first.outcome.as_str()), (ChargeState::Charged, "charged"));
    assert_eq!((above.state(), above.outcome.as_str()), (ChargeState::Refused, "above_limit"));
    assert_eq!(w.balance(&x).await, (90, 0));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_buyer_transaction_above_the_resource_fee_cap_is_not_sent(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    use fermah_pay_stellar_gateway::submission::DEFAULT_MAX_BUYER_RESOURCE_FEE;
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 10, "c-1").await;
    let alice = w.buyer("alice", 100).await;
    let deposit = w.deposit(&alice, 50, "d-1").await;
    // Every call now simulates above what a buyer's transaction may cost,
    // as a contract-account wallet's own code can make it.
    w.stellar.with(|n| n.resource_fee = DEFAULT_MAX_BUYER_RESOURCE_FEE + 1);
    let before = w.stellar.sent().len();
    w.settle(&w.worker()).await;
    // The operator's batch is sent and settles; the buyer's deposit is not
    // sent, and waits until its authorization lapses.
    assert_eq!(w.stellar.sent().len(), before + 1);
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Charged);
    assert_eq!(w.get_deposit(&deposit.deposit_id).await.state(), DepositState::Signed);
    w.stellar.with(|n| n.resource_fee = DEFAULT_MAX_BUYER_RESOURCE_FEE);
    w.clock.advance(Duration::from_secs(3_600));
    w.settle(&w.worker()).await;
    assert_eq!(w.get_deposit(&deposit.deposit_id).await.state(), DepositState::Confirmed);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_an_unreadable_batch_answer_waits_for_a_read_at_its_inclusion(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 10, "c-1").await;
    w.stellar.with(|n| {
        n.truncate_outcomes = true;
        n.entries_behind = 1;
    });
    let worker = w.worker();
    w.settle(&worker).await;
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Submitted);
    w.stellar.with(|n| n.entries_behind = 0);
    w.settle(&worker).await;
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Charged);
    assert_eq!(w.balance(&x).await, (90, 0));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_applied_batch_without_records_is_quarantined(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 10, "c-1").await;
    // Included and successful, unreadable, and no record: the evidence
    // contradicts itself, so nothing is guessed.
    w.stellar.with(|n| {
        n.truncate_outcomes = true;
        n.skip_records = true;
    });
    w.settle(&w.worker()).await;
    let quarantined = w.get_charge(&charge.charge_id).await;
    assert_eq!(quarantined.state(), ChargeState::Quarantined);
    assert_eq!(w.balance(&x).await, (90, 0));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_batch_of_unknown_fate_is_decided_from_the_charge_records(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let worker = w.worker();
    w.stellar.with(|n| n.drop_sends = 2);
    worker.step().await.unwrap();
    // The source sequence moves past the batch although the batch is not
    // found: the engine quarantines the submission. The contract holds no
    // record of the charge, so it is sent again, and settled once.
    w.stellar.with(|n| n.source_sequence += 1);
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.stellar.set_latest(START_LEDGER + OPERATOR_LEDGERS + 1);
    w.settle(&worker).await;
    let quarantined: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pay_stellar.submissions WHERE state = 'quarantined'",
    )
    .fetch_one(&w.h.owner)
    .await
    .unwrap();
    assert_eq!(quarantined, 1);
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Charged);
    assert_eq!(w.balance(&x).await, (70, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some(70));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_charge_not_sent_before_its_last_ledger_is_refunded(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let sends = w.stellar.sent().len();
    // Too close to its last ledger to be sure of landing in time: not sent,
    // not yet refunded.
    w.stellar.set_latest(charge.last_ledger - 1);
    w.settle(&w.worker()).await;
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Admitted);
    assert_eq!(w.stellar.sent().len(), sends);
    w.stellar.set_latest(charge.last_ledger + 1);
    w.settle(&w.worker()).await;
    let expired = w.get_charge(&charge.charge_id).await;
    assert_eq!((expired.state(), expired.outcome.as_str()), (ChargeState::Refused, "expired"));
    assert_eq!(w.balance(&x).await, (100, 0));
    assert_eq!(w.stellar.sent().len(), sends);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_batch_not_included_past_the_charges_last_ledger_is_refunded(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let worker = w.worker();
    w.stellar.with(|n| n.drop_sends = 2);
    worker.step().await.unwrap();
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    // Past its last ledger while a record would still live: never applied.
    w.stellar.set_latest(charge.last_ledger + 1);
    w.settle(&worker).await;
    let expired = w.get_charge(&charge.charge_id).await;
    assert_eq!((expired.state(), expired.outcome.as_str()), (ChargeState::Refused, "expired"));
    assert_eq!(w.balance(&x).await, (100, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some(100));
}

/// Sends a charge's batch, which the network never includes, and moves past
/// the ledger up to which the charge's record would have lived: the record no
/// longer tells whether the batch applied it.
async fn decided_after_records_lapse(
    w: &World,
    worker: &Worker<Stellar, ManualClock>,
    charge: &Charge,
    applied_elsewhere: bool,
) {
    w.stellar.with(|n| n.drop_sends = 2);
    worker.step().await.unwrap();
    if applied_elsewhere {
        w.stellar.include_elsewhere(w.stellar.sent().last().unwrap());
    }
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.stellar.set_latest(charge.last_ledger + CHARGE_RECORD_GRACE + 1);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_batch_never_applied_and_decided_late_is_refunded_from_the_events(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let worker = w.worker();
    decided_after_records_lapse(&w, &worker, &charge, false).await;
    w.settle(&worker).await;
    let refunded = w.get_charge(&charge.charge_id).await;
    assert_eq!((refunded.state(), refunded.outcome.as_str()), (ChargeState::Refused, "expired"));
    assert_eq!(w.balance(&x).await, (100, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some(100));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_batch_applied_elsewhere_and_decided_late_is_settled_from_its_event(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let worker = w.worker();
    decided_after_records_lapse(&w, &worker, &charge, true).await;
    w.settle(&worker).await;
    let settled = w.get_charge(&charge.charge_id).await;
    assert_eq!((settled.state(), settled.outcome.as_str()), (ChargeState::Charged, "charged"));
    assert_eq!(w.balance(&x).await, (70, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some(70));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_an_expired_answer_before_the_settlement_does_not_refund_the_charge(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let worker = w.worker();
    w.stellar.with(|n| n.drop_sends = 2);
    worker.step().await.unwrap();
    let batch = w.stellar.sent().last().unwrap().clone();
    // An `expired` answer debits nothing and records nothing; the batch
    // applied afterwards is the charge's settlement.
    w.stellar.log_expired_answer(&batch);
    w.stellar.include_elsewhere(&batch);
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.stellar.set_latest(charge.last_ledger + CHARGE_RECORD_GRACE + 1);
    w.settle(&worker).await;
    let settled = w.get_charge(&charge.charge_id).await;
    assert_eq!((settled.state(), settled.outcome.as_str()), (ChargeState::Charged, "charged"));
    assert_eq!(w.balance(&x).await, (70, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some(70));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_late_decision_without_the_whole_event_range_stays_quarantined(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let worker = w.worker();
    decided_after_records_lapse(&w, &worker, &charge, false).await;
    // The node no longer holds the ledger at which the batch was authorized.
    w.stellar.with(|n| n.oldest = START_LEDGER + 1);
    w.settle(&worker).await;
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Quarantined);
    assert_eq!(w.balance(&x).await, (70, 0));
    let reason = w.last_error("charges", &charge.charge_id).await.unwrap();
    assert!(
        reason.contains(&format!(
            "retains events only from ledger {}, so ledgers {START_LEDGER} to {START_LEDGER} cannot be searched",
            START_LEDGER + 1
        )),
        "{reason}"
    );
}

/// A charge quarantined because the node the worker read had pruned the
/// events of its batch, whose authorization was applied elsewhere or not;
/// with the latest ledger when the batch was signed.
async fn quarantined_late(
    w: &World,
    x: &TestBuyer,
    key: &str,
    applied_elsewhere: bool,
) -> (Charge, u32) {
    let charge = w.charge(x, 30, key).await;
    let worker = w.worker();
    let signed_at = w.stellar.latest();
    decided_after_records_lapse(w, &worker, &charge, applied_elsewhere).await;
    let oldest = w.stellar.with(|n| std::mem::replace(&mut n.oldest, n.latest));
    w.settle(&worker).await;
    w.stellar.with(|n| n.oldest = oldest);
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Quarantined);
    (charge, signed_at)
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_events_resolve_a_quarantined_charge_as_settled_or_expired(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let applied = quarantined_late(&w, &x, "c-1", true).await;
    let never = quarantined_late(&w, &x, "c-2", false).await;
    assert_eq!(w.balance(&x).await, (40, 0));

    for ((charge, signed_at), expected, state, balance) in [
        (&applied, Resolution::Settled(Outcome::Charged), ChargeState::Charged, 40),
        (&never, Resolution::Expired, ChargeState::Refused, 70),
    ] {
        let q = quarantined(&w, &charge.charge_id).await;
        let from = quarantine::authorization_ledger(&w.operator_pool, q.id).await.unwrap();
        assert_eq!(from, Some(*signed_at), "the ledger the operator signed at");
        let (resolution, evidence) =
            quarantine::prove_from_events(&w.stellar, &q, from).await.unwrap();
        assert_eq!(resolution, expected);
        assert!(
            evidence.contains(&format!("in ledgers {signed_at} to {}", charge.last_ledger)),
            "{evidence}"
        );
        quarantine::resolve(&w.operator_pool, q.id, resolution, &evidence).await.unwrap();
        assert_eq!(w.get_charge(&charge.charge_id).await.state(), state);
        assert_eq!(w.balance(&x).await, (balance, 0));
    }
    assert_eq!(w.stellar.account(&x.key.address()), Some(70));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_events_that_do_not_cover_the_charge_resolve_nothing(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let (charge, signed_at) = quarantined_late(&w, &x, "c-1", false).await;
    assert_eq!(signed_at, START_LEDGER);
    let q = quarantined(&w, &charge.charge_id).await;
    let from = quarantine::authorization_ledger(&w.operator_pool, q.id).await.unwrap();

    // Pruned history.
    w.stellar.with(|n| n.oldest = START_LEDGER + 1);
    let error = quarantine::prove_from_events(&w.stellar, &q, from).await.unwrap_err();
    assert!(matches!(error, QuarantineError::EventsPruned { from: START_LEDGER, .. }), "{error:?}");
    w.stellar.with(|n| n.oldest = 1);
    // A node not yet past the charge's last ledger, when it could still land.
    let latest = w.stellar.latest();
    w.stellar.set_latest(charge.last_ledger - 1);
    let error = quarantine::prove_from_events(&w.stellar, &q, from).await.unwrap_err();
    assert!(matches!(error, QuarantineError::EventsBeforeLastLedger { .. }), "{error:?}");
    w.stellar.set_latest(latest);
    // No known authorization ledger: nothing bounds the search.
    let error = quarantine::prove_from_events(&w.stellar, &q, None).await.unwrap_err();
    assert!(matches!(error, QuarantineError::NoAuthorizationLedger { .. }), "{error:?}");
    // Control: the same charge against the whole range resolves.
    let (resolution, _) = quarantine::prove_from_events(&w.stellar, &q, from).await.unwrap();
    assert_eq!(resolution, Resolution::Expired);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_archived_buyer_account_is_restored_and_the_batch_settles(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let idle = w.funded("idle", 100).await;
    let active = w.funded("active", 100).await;
    // The idle buyer's entry outlived its TTL and was archived.
    w.stellar.with(|n| n.archived.insert(idle.key.address()));
    let from_idle = w.charge(&idle, 10, "c-idle").await;
    let from_active = w.charge(&active, 10, "c-active").await;
    let worker = w.worker();
    w.settle(&worker).await;
    // The restore landed; the refused batch waits out the pause that keeps a
    // failing restore from being resent every step.
    assert!(w.stellar.with(|n| n.archived.is_empty()));
    assert_eq!(w.get_charge(&from_active.charge_id).await.state(), ChargeState::Admitted);
    w.clock.advance(RETRY_AFTER + Duration::from_secs(1));
    w.settle(&worker).await;

    for charge in [&from_idle, &from_active] {
        assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Charged);
    }
    assert!(w.stellar.with(|n| n.archived.is_empty()));
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM pay_stellar.submissions WHERE kind <> 'deposit' ORDER BY created_at",
    )
    .fetch_all(&w.h.owner)
    .await
    .unwrap();
    assert_eq!(kinds, ["restore", "charge_batch"]);
    assert_eq!(w.stellar.account(&idle.key.address()), Some(90));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_restarted_worker_resends_the_recorded_batch_and_settles_it(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let source = SecretKey::generate().unwrap().to_strkey().to_string();
    w.stellar.with(|n| n.drop_sends = 1);
    let sends_before = w.stellar.sent().len();
    assert!(matches!(w.worker_with(&source, 100).step().await.unwrap(), Step::Submitted(_)));

    // A new process with the same source finds the recorded envelope.
    w.settle(&w.worker_with(&source, 100)).await;
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Charged);
    let sent = w.stellar.sent();
    assert_eq!(sent.len(), sends_before + 2);
    assert_eq!(sent[sends_before], sent[sends_before + 1], "the same bytes are resent");
    assert_eq!(w.stellar.account(&x.key.address()), Some(70));
}

/// A second source account, with its own sequence on the network.
fn extra_source(w: &World) -> SecretKey {
    let key = SecretKey::generate().unwrap();
    w.stellar.with(|n| n.sequences.insert(key.address(), 900));
    key
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_free_sources_each_send_a_batch_in_one_round(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charges = [
        w.charge(&x, 10, "c-1").await,
        w.charge(&x, 20, "c-2").await,
        w.charge(&x, 30, "c-3").await,
    ];
    let primary = SecretKey::from_strkey(&w.source_seed).unwrap();
    let worker = w.worker_with_sources(vec![primary, extra_source(&w)], 1);
    let states = || async {
        let mut states = Vec::new();
        for charge in &charges {
            states.push(w.get_charge(&charge.charge_id).await.state());
        }
        states
    };

    // Neither envelope is included at once, nor when resent in the next
    // round: each source holds one in flight.
    w.stellar.with(|n| n.drop_sends = 4);
    let Step::Submitted(sent) = worker.step().await.unwrap() else { panic!("nothing sent") };
    assert_eq!(sent.len(), 2, "one batch per source");
    let sources: HashSet<AccountAddress> = w
        .stellar
        .sent()
        .iter()
        .map(|envelope| {
            let MuxedAccount::Ed25519(Uint256(key)) = inner(envelope).source_account else {
                panic!("muxed source")
            };
            AccountAddress::from_public_key(key)
        })
        .collect();
    assert_eq!(sources.len(), 2);
    assert_eq!(
        states().await,
        [ChargeState::Submitted, ChargeState::Submitted, ChargeState::Admitted]
    );
    assert_eq!(worker.step().await.unwrap(), Step::InFlight, "no source is free");

    w.settle(&worker).await;
    assert_eq!(states().await, [ChargeState::Charged; 3]);
    assert_eq!(w.stellar.account(&x.key.address()), Some(40));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_stuck_source_does_not_hold_back_the_others(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let primary = SecretKey::from_strkey(&w.source_seed).unwrap();
    w.stellar.with(|n| n.stuck.insert(primary.address()));
    let stuck = w.charge(&x, 10, "c-1").await;
    let single = w.worker_with(&w.source_seed, 1);
    single.step().await.unwrap();
    assert_eq!(w.get_charge(&stuck.charge_id).await.state(), ChargeState::Submitted);

    // With its only source stuck, a worker sends nothing more.
    let waiting = w.charge(&x, 20, "c-2").await;
    assert_eq!(single.step().await.unwrap(), Step::InFlight);
    assert_eq!(w.get_charge(&waiting.charge_id).await.state(), ChargeState::Admitted);

    // With a second source, the next batch goes out and settles.
    let pooled = w.worker_with_sources(vec![primary, extra_source(&w)], 1);
    assert!(matches!(pooled.step().await.unwrap(), Step::Submitted(sent) if sent.len() == 1));
    assert_eq!(w.get_charge(&waiting.charge_id).await.state(), ChargeState::Charged);
    assert_eq!(w.get_charge(&stuck.charge_id).await.state(), ChargeState::Submitted);
    assert_eq!(w.stellar.account(&x.key.address()), Some(80));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_at_the_fee_floor_work_in_flight_finishes_and_nothing_new_is_built(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let worker = w.worker_with(&w.source_seed, 1);
    // Spendable is the balance less two base reserves.
    let at_floor = FEE_FLOOR + 2 * i64::from(BASE_RESERVE);

    // A batch goes out and is not yet included.
    let first = w.charge(&x, 10, "c-1").await;
    w.stellar.with(|n| n.drop_sends = 1);
    worker.step().await.unwrap();
    assert_eq!(w.get_charge(&first.charge_id).await.state(), ChargeState::Submitted);

    // One stroop short of the floor: the batch in flight still settles, and
    // the next charge waits.
    w.stellar.with(|n| n.fee_balance = at_floor - 1);
    let second = w.charge(&x, 20, "c-2").await;
    let sends = w.stellar.sent().len();
    w.settle(&worker).await;
    assert_eq!(w.get_charge(&first.charge_id).await.state(), ChargeState::Charged);
    assert_eq!(w.get_charge(&second.charge_id).await.state(), ChargeState::Admitted);
    assert_eq!(w.stellar.sent().len(), sends + 1, "only the resend of the batch in flight");

    // The positive control: at the floor, the waiting charge goes out.
    w.stellar.with(|n| n.fee_balance = at_floor);
    w.settle(&worker).await;
    assert_eq!(w.get_charge(&second.charge_id).await.state(), ChargeState::Charged);
    assert_eq!(w.stellar.account(&x.key.address()), Some(70));
}

/// Envelopes sent that extend entries' life, with their footprints.
fn extensions_sent(w: &World) -> Vec<Vec<LedgerKey>> {
    w.stellar
        .sent()
        .iter()
        .filter(|envelope| extension(envelope).is_some())
        .map(|envelope| {
            let TransactionExt::V1(data) = &inner(envelope).ext else { panic!("unassembled") };
            data.resources.footprint.read_only.to_vec()
        })
        .collect()
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_an_idle_contract_is_extended_before_it_could_be_archived(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let worker = w.worker();
    let latest = w.stellar.with(|n| n.latest);

    // The positive control: exactly at the threshold, nothing is sent.
    w.stellar.with(|n| {
        n.instance_live_until = Some(latest + TTL_THRESHOLD);
        n.code_live_until = latest + TTL_EXTEND_TO;
    });
    worker.step().await.unwrap();
    assert!(extensions_sent(&w).is_empty());

    // One ledger less, at the next check: instance and code are extended.
    w.stellar.with(|n| n.instance_live_until = Some(latest + TTL_THRESHOLD - 1));
    worker.step().await.unwrap();
    assert!(extensions_sent(&w).is_empty(), "not checked again before the interval");
    w.clock.advance(TTL_CHECK_EVERY);
    worker.step().await.unwrap();
    let code_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: Hash(WASM) });
    let sent = extensions_sent(&w);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains(&w.stellar.deployment.instance_key()) && sent[0].contains(&code_key));
    assert_eq!(w.stellar.with(|n| n.instance_live_until), Some(latest + TTL_EXTEND_TO));

    // Extended, it is left alone at the next check.
    w.clock.advance(TTL_CHECK_EVERY);
    worker.step().await.unwrap();
    assert_eq!(extensions_sent(&w).len(), 1);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_batches_hold_at_most_the_configured_number_of_charges(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let y = w.funded("y", 100).await;
    let mut charges = Vec::new();
    for i in 0..3 {
        charges.push(w.charge(&x, 10, &format!("x-{i}")).await);
    }
    charges.push(w.charge(&y, 10, "y-0").await);
    w.settle(&w.worker_with(&w.source_seed, 2)).await;
    for charge in &charges {
        assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Charged);
    }
    let batches: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pay_stellar.submissions WHERE kind = 'charge_batch'",
    )
    .fetch_one(&w.h.owner)
    .await
    .unwrap();
    assert_eq!(batches, 2);
    assert_eq!(w.stellar.account(&x.key.address()), Some(70));
    assert_eq!(w.stellar.account(&y.key.address()), Some(90));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_two_workers_racing_for_one_batch_submit_it_once(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 30, "c-1").await;
    let first = w.worker_with(&SecretKey::generate().unwrap().to_strkey(), 100);
    let second = w.worker_with(&SecretKey::generate().unwrap().to_strkey(), 100);
    // Both simulate the same batch before either links it.
    w.stellar.with(|n| n.simulation_barrier = Some(Arc::new(Barrier::new(2))));
    let (a, b) = tokio::join!(first.step(), second.step());
    w.stellar.with(|n| n.simulation_barrier = None);
    let submitted =
        [a.unwrap(), b.unwrap()].iter().filter(|s| matches!(s, Step::Submitted(_))).count();
    assert_eq!(submitted, 1);
    let batches: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pay_stellar.submissions WHERE kind = 'charge_batch'",
    )
    .fetch_one(&w.h.owner)
    .await
    .unwrap();
    assert_eq!(batches, 1);
    w.settle(&first).await;
    w.settle(&second).await;
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Charged);
    assert_eq!(w.stellar.account(&x.key.address()), Some(70));
}

// ---- end to end -------------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_three_charges_and_a_retried_charge_end_in_matching_balances(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.funded("alice", 100).await;
    let mut ids = Vec::new();
    for (i, amount) in [10, 20, 30].into_iter().enumerate() {
        ids.push(w.charge(&buyer, amount, &format!("c-{i}")).await.charge_id);
    }
    let retried = w.charge(&buyer, 10, "c-0").await;
    assert_eq!(retried.charge_id, ids[0]);
    w.settle(&w.worker()).await;
    for id in &ids {
        assert_eq!(w.get_charge(id).await.state(), ChargeState::Charged);
    }
    assert_eq!(w.balance(&buyer).await, (40, 0));
    assert_eq!(w.stellar.account(&buyer.key.address()), Some(40));
}

// ---- resolving quarantine ---------------------------------------------------

fn hash_of(hex: &str) -> [u8; 32] {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    bytes.try_into().unwrap()
}

async fn quarantined(w: &World, id: &str) -> QuarantinedCharge {
    quarantine::quarantined_charges(&w.operator_pool, Some(uuid::Uuid::parse_str(id).unwrap()))
        .await
        .unwrap()
        .pop()
        .expect("charge is quarantined")
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_quarantined_charges_are_resolved_from_the_contract_event(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charged = w.charge(&x, 10, "c-1").await;
    let above = w.charge(&x, MAX_CHARGE as i64 + 10, "c-2").await;
    // The return value is unusable and no record was left, so both are
    // quarantined, still debited; the batch's event still shows what the
    // contract did.
    w.stellar.with(|n| {
        n.truncate_outcomes = true;
        n.skip_records = true;
    });
    w.settle(&w.worker()).await;
    assert_eq!(w.balance(&x).await, (100 - 10 - (MAX_CHARGE as i64 + 10), 0));
    let batch = w.get_charge(&charged.charge_id).await.transaction_hash;

    for (charge, expected) in [
        (&charged, Resolution::Settled(Outcome::Charged)),
        (&above, Resolution::Settled(Outcome::AboveLimit)),
    ] {
        let q = quarantined(&w, &charge.charge_id).await;
        let (resolution, evidence) =
            quarantine::prove_from_transaction(&w.stellar, &q, &hash_of(&batch)).await.unwrap();
        assert_eq!(resolution, expected);
        assert!(evidence.contains(&batch), "{evidence}");
        quarantine::resolve(&w.operator_pool, q.id, resolution, &evidence).await.unwrap();
    }
    let settled = w.get_charge(&charged.charge_id).await;
    let refused = w.get_charge(&above.charge_id).await;
    assert_eq!((settled.state(), settled.outcome.as_str()), (ChargeState::Charged, "charged"));
    assert_eq!((refused.state(), refused.outcome.as_str()), (ChargeState::Refused, "above_limit"));
    // Only the refused amount came back.
    assert_eq!(w.balance(&x).await, (90, 0));
    let audit: Vec<(String, String)> = sqlx::query_as(
        "SELECT resolution, resolved_by FROM pay_stellar.charge_resolutions ORDER BY resolved_at",
    )
    .fetch_all(&w.h.owner)
    .await
    .unwrap();
    assert_eq!(audit.len(), 2);

    // A resolved charge cannot be resolved again, and its buyer settles again.
    let again = quarantine::resolve(
        &w.operator_pool,
        uuid::Uuid::parse_str(&above.charge_id).unwrap(),
        Resolution::Settled(Outcome::AboveLimit),
        "twice",
    )
    .await
    .unwrap_err();
    assert!(matches!(again, QuarantineError::NotQuarantined(_)), "{again:?}");
    assert_eq!(w.balance(&x).await, (90, 0));
    w.stellar.with(|n| {
        n.truncate_outcomes = false;
        n.skip_records = false;
    });
    let next = w.charge(&x, 5, "c-3").await;
    w.settle(&w.worker()).await;
    assert_eq!(w.get_charge(&next.charge_id).await.state(), ChargeState::Charged);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_evidence_that_does_not_settle_the_charge_is_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let y = w.funded("y", 100).await;
    w.stellar.with(|n| n.state.accounts.remove(&y.key.address()));
    let lost = w.charge(&y, 10, "c-y").await;
    let fine = w.charge(&x, 10, "c-x").await;
    w.settle(&w.worker()).await;
    let q = quarantined(&w, &lost.charge_id).await;
    let batch = hash_of(&w.get_charge(&lost.charge_id).await.transaction_hash);

    // The contract answered unknown_account, recorded as such: neither the
    // batch nor the record settles the charge.
    let error = quarantine::prove_from_transaction(&w.stellar, &q, &batch).await.unwrap_err();
    assert!(
        matches!(error, QuarantineError::NotSettled { outcome: "unknown_account", .. }),
        "{error:?}"
    );
    w.stellar.set_latest(q.authorization_horizon + 1);
    let error = quarantine::prove_from_record(&w.stellar, &q).await.unwrap_err();
    assert!(
        matches!(error, QuarantineError::RecordUnresolvable { outcome: "unknown_account", .. }),
        "{error:?}"
    );
    // An entry for the right account and charge but another amount proves
    // nothing either: X's charge settled for 10, not 11.
    assert_eq!(w.get_charge(&fine.charge_id).await.state(), ChargeState::Charged);
    let wrong_amount = QuarantinedCharge {
        owner: x.key.address().into(),
        charge_id: hash_of(&fine.contract_charge_id),
        amount: 11,
        ..q.clone()
    };
    let error =
        quarantine::prove_from_transaction(&w.stellar, &wrong_amount, &batch).await.unwrap_err();
    assert!(matches!(error, QuarantineError::AmountMismatch { .. }), "{error:?}");
}

/// A stored envelope whose only authorization expires at `horizon`.
fn envelope_with_horizon(horizon: u32) -> String {
    let call = InvokeContractArgs {
        contract_address: ScAddress::Contract(ContractId(Hash([7; 32]))),
        function_name: ScSymbol("charge_batch".try_into().unwrap()),
        args: VecM::default(),
    };
    let entry = SorobanAuthorizationEntry {
        credentials: SorobanCredentials::Address(SorobanAddressCredentials {
            address: ScAddress::Account(account_id(&treasury())),
            nonce: 1,
            signature_expiration_ledger: horizon,
            signature: ScVal::Void,
        }),
        root_invocation: SorobanAuthorizedInvocation {
            function: SorobanAuthorizedFunction::ContractFn(call.clone()),
            sub_invocations: VecM::default(),
        },
    };
    let operation = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(call),
            auth: vec![entry].try_into().unwrap(),
        }),
    };
    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([1; 32])),
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![operation].try_into().unwrap(),
            ext: TransactionExt::V0,
        },
        signatures: VecM::default(),
    })
    .to_xdr_base64(Limits::none())
    .unwrap()
}

/// Quarantines an admitted charge the contract never saw, as a contradiction
/// elsewhere would: the charge is linked to a finished submission whose
/// authorization lapsed a ledger ago.
async fn quarantine_unsent(w: &World, charge_id: &str) {
    let horizon = w.stellar.with(|n| n.latest) - 1;
    quarantine_unsent_until(w, charge_id, horizon).await;
}

async fn quarantine_unsent_until(w: &World, charge_id: &str, horizon: u32) {
    let submission = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO pay_stellar.submissions
             (id, network, kind, state, source_address, fee_source_address, sequence,
              valid_until, inner_hash, outer_hash, envelope_xdr, resolved_at, last_error)
         VALUES ($1, 'stellar:testnet', 'charge_batch', 'quarantined', $2, $2, 1, now(),
                 $3, $4, $5, now(), 'test')",
    )
    .bind(submission)
    .bind(treasury().as_str())
    .bind(vec![3_u8; 32])
    .bind(submission.as_bytes().repeat(2))
    .bind(envelope_with_horizon(horizon))
    .execute(&w.h.owner)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE pay_stellar.charges
         SET state = 'quarantined', submission_id = $2, batch_index = 0, settled_at = now()
         WHERE id = $1::uuid",
    )
    .bind(charge_id)
    .bind(submission)
    .execute(&w.h.owner)
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_record_evidence_readmits_expires_or_refuses(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let readmit = w.charge(&x, 10, "c-1").await;
    let expire = w.charge(&x, 20, "c-2").await;
    for charge in [&readmit, &expire] {
        quarantine_unsent(&w, &charge.charge_id).await;
    }

    // No record and still within its last ledger: sent again, charged once.
    let q = quarantined(&w, &readmit.charge_id).await;
    let (resolution, evidence) = quarantine::prove_from_record(&w.stellar, &q).await.unwrap();
    assert_eq!(resolution, Resolution::Readmitted);
    quarantine::resolve(&w.operator_pool, q.id, resolution, &evidence).await.unwrap();
    w.settle(&w.worker()).await;
    assert_eq!(w.get_charge(&readmit.charge_id).await.state(), ChargeState::Charged);
    assert_eq!(w.stellar.account(&x.key.address()), Some(90));

    // Past its last ledger while a record would still live: refunded.
    let q = quarantined(&w, &expire.charge_id).await;
    w.stellar.set_latest(expire.last_ledger + CHARGE_RECORD_GRACE + 1);
    let error = quarantine::prove_from_record(&w.stellar, &q).await.unwrap_err();
    assert!(matches!(error, QuarantineError::RecordGone { .. }), "{error:?}");
    w.stellar.set_latest(expire.last_ledger + 1);
    let (resolution, evidence) = quarantine::prove_from_record(&w.stellar, &q).await.unwrap();
    assert_eq!(resolution, Resolution::Expired);
    quarantine::resolve(&w.operator_pool, q.id, resolution, &evidence).await.unwrap();
    let refunded = w.get_charge(&expire.charge_id).await;
    assert_eq!((refunded.state(), refunded.outcome.as_str()), (ChargeState::Refused, "expired"));
    assert_eq!(w.balance(&x).await, (90, 0));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_record_evidence_needs_a_read_past_the_batch_authorization(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 10, "c-1").await;
    let latest = w.stellar.with(|n| n.latest);
    quarantine_unsent_until(&w, &charge.charge_id, latest + 5).await;
    let q = quarantined(&w, &charge.charge_id).await;
    assert_eq!(q.authorization_horizon, latest + 5);

    // The batch's authorization could still land, or a lagging node may not
    // show where it already did: no record proves nothing.
    w.stellar.set_latest(latest + 5);
    let error = quarantine::prove_from_record(&w.stellar, &q).await.unwrap_err();
    assert!(
        matches!(error, QuarantineError::ReadBeforeHorizon { ledger, horizon, .. }
            if ledger == latest + 5 && horizon == latest + 5),
        "{error:?}"
    );
    // A node at the horizon but serving entries from a ledger behind it:
    // refused likewise.
    w.stellar.set_latest(latest + 6);
    w.stellar.with(|n| n.entries_behind = 1);
    let error = quarantine::prove_from_record(&w.stellar, &q).await.unwrap_err();
    assert!(matches!(error, QuarantineError::ReadBeforeHorizon { .. }), "{error:?}");
    // Past it: the absent record readmits the charge.
    w.stellar.with(|n| n.entries_behind = 0);
    let (resolution, _) = quarantine::prove_from_record(&w.stellar, &q).await.unwrap();
    assert_eq!(resolution, Resolution::Readmitted);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_only_the_resolution_function_leaves_quarantine(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 10, "c-1").await;
    w.stellar.with(|n| {
        n.truncate_outcomes = true;
        n.skip_records = true;
    });
    w.settle(&w.worker()).await;
    let id = uuid::Uuid::parse_str(&charge.charge_id).unwrap();

    // The worker, which may otherwise move charges, cannot move this one.
    let error = sqlx::query(
        "UPDATE pay_stellar.charges SET state = 'admitted', outcome = NULL, submission_id = NULL,
             batch_index = NULL, settled_at = NULL WHERE id = $1",
    )
    .bind(id)
    .execute(&w.worker_pool)
    .await
    .unwrap_err();
    assert!(error.to_string().contains("is already quarantined"), "{error}");
    // No runtime role but the operator may call the function, and the
    // operator may not bypass it.
    for pool in [&w.worker_pool, &w.h.api, &w.h.issuer] {
        let error =
            sqlx::query("SELECT pay_stellar.resolve_quarantined_charge($1, 'charged', 'x')")
                .bind(id)
                .execute(pool)
                .await
                .unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().unwrap(), "42501", "{error}");
    }
    for statement in [
        "UPDATE pay_stellar.charges SET state = 'charged' WHERE id = $1",
        "INSERT INTO pay_stellar.charge_resolutions (charge_id, resolution, evidence) VALUES ($1, 'charged', 'x')",
    ] {
        let error = sqlx::query(statement).bind(id).execute(&w.operator_pool).await.unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().unwrap(), "42501", "{error}");
    }
    assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Quarantined);

    // The audit trail cannot be rewritten, even by the owner.
    let q = quarantined(&w, &charge.charge_id).await;
    let batch = hash_of(&w.get_charge(&charge.charge_id).await.transaction_hash);
    let (resolution, evidence) =
        quarantine::prove_from_transaction(&w.stellar, &q, &batch).await.unwrap();
    quarantine::resolve(&w.operator_pool, q.id, resolution, &evidence).await.unwrap();
    for statement in [
        "UPDATE pay_stellar.charge_resolutions SET evidence = 'rewritten'",
        "DELETE FROM pay_stellar.charge_resolutions",
    ] {
        let error = sqlx::query(statement).execute(&w.h.owner).await.unwrap_err();
        assert!(error.to_string().contains("append-only"), "{error}");
    }
}

// ---- leadership -------------------------------------------------------------

async fn lease_holder(w: &World, name: &str) -> Option<uuid::Uuid> {
    sqlx::query_scalar("SELECT holder FROM pay_stellar.leases WHERE name = $1")
        .bind(name)
        .fetch_optional(&w.h.owner)
        .await
        .unwrap()
}

/// Waits until `done` holds, failing the test after ten seconds.
async fn eventually<F: std::future::Future<Output = bool>>(what: &str, done: impl FnMut() -> F) {
    within(Duration::from_secs(10), what, done).await;
}

/// Waits until `done` holds, failing the test after `limit`.
async fn within<F: std::future::Future<Output = bool>>(
    limit: Duration,
    what: &str,
    mut done: impl FnMut() -> F,
) {
    let wait = async {
        while !done().await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(limit, wait).await.unwrap_or_else(|_| panic!("{what} never happened"));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_lease_is_held_by_one_process_until_it_lapses_or_is_released(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let name = "worker:test";
    let first = Lease::new(w.worker_pool.clone(), name.to_owned(), Duration::from_secs(30));
    let second = Lease::new(w.worker_pool.clone(), name.to_owned(), Duration::from_secs(30));

    assert!(first.try_hold().await.unwrap());
    assert!(!second.try_hold().await.unwrap(), "another process holds it");
    assert!(first.try_hold().await.unwrap(), "the holder renews it");
    assert_eq!(lease_holder(&w, name).await, Some(first.holder()));

    // Once it lapses, the other process takes it and the former holder can
    // no longer renew it.
    sqlx::query(
        "UPDATE pay_stellar.leases SET acquired_at = now() - interval '1 hour', \
         expires_at = now() - interval '1 second'",
    )
    .execute(&w.h.owner)
    .await
    .unwrap();
    assert!(second.try_hold().await.unwrap());
    assert!(!first.try_hold().await.unwrap());
    assert_eq!(lease_holder(&w, name).await, Some(second.holder()));

    // Releasing a lease held by another process changes nothing; the
    // holder's release frees it at once.
    first.release().await.unwrap();
    assert_eq!(lease_holder(&w, name).await, Some(second.holder()));
    second.release().await.unwrap();
    assert_eq!(lease_holder(&w, name).await, None);
    assert!(first.try_hold().await.unwrap());
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_standby_worker_takes_over_when_the_leader_stops(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.buyer("x", 100).await;
    let name = "worker:test";
    let first_key = SecretKey::from_strkey(&w.source_seed).unwrap();
    let second_key = extra_source(&w);
    let (first_source, second_source) = (first_key.address(), second_key.address());
    let spawn = |key: SecretKey, shutdown: tokio::sync::oneshot::Receiver<()>| {
        let worker = w.worker_with_sources(vec![key], 100);
        let lease = Lease::new(w.worker_pool.clone(), name.to_owned(), Duration::from_secs(1));
        let holder = lease.holder();
        let task = tokio::spawn(async move {
            let shutdown = async {
                let _ = shutdown.await;
            };
            lease::lead(&lease, "worker", shutdown, |stop| {
                worker.run(Duration::from_millis(20), Duration::from_millis(50), stop.wait())
            })
            .await;
        });
        (task, holder)
    };
    let sent_from = |id: String| {
        let owner = w.h.owner.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT s.source_address FROM pay_stellar.deposits d \
                 JOIN pay_stellar.submissions s ON s.id = d.submission_id WHERE d.id = $1::uuid",
            )
            .bind(id)
            .fetch_one(&owner)
            .await
            .unwrap()
        }
    };
    let submissions_from = |source: String| {
        let owner = w.h.owner.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pay_stellar.submissions WHERE source_address = $1",
            )
            .bind(source)
            .fetch_one(&owner)
            .await
            .unwrap()
        }
    };

    let (_stop_first, first_stopped) = tokio::sync::oneshot::channel();
    let (first, first_holder) = spawn(first_key, first_stopped);
    eventually("the first worker leading", || async {
        lease_holder(&w, name).await == Some(first_holder)
    })
    .await;
    let (stop_second, second_stopped) = tokio::sync::oneshot::channel();
    let (second, second_holder) = spawn(second_key, second_stopped);

    // The leader settles; the standby sends nothing.
    let d1 = w.deposit(&x, 40, "d-1").await;
    eventually("the first deposit confirmed", || async {
        w.get_deposit(&d1.deposit_id).await.state() == DepositState::Confirmed
    })
    .await;
    assert_eq!(sent_from(d1.deposit_id.clone()).await, first_source.to_string());
    assert_eq!(submissions_from(second_source.to_string()).await, 0);
    assert_eq!(lease_holder(&w, name).await, Some(first_holder));

    // The leader dies without releasing its lease: the standby takes over
    // once it lapses and settles the next deposit.
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let d2 = w.deposit(&x, 50, "d-2").await;
    eventually("the second deposit confirmed", || async {
        w.get_deposit(&d2.deposit_id).await.state() == DepositState::Confirmed
    })
    .await;
    assert_eq!(sent_from(d2.deposit_id.clone()).await, second_source.to_string());
    assert_eq!(lease_holder(&w, name).await, Some(second_holder));
    assert_eq!(w.stellar.account(&x.key.address()), Some(90));

    // A worker that shuts down releases its lease.
    stop_second.send(()).unwrap();
    second.await.unwrap();
    assert_eq!(lease_holder(&w, name).await, None);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_leader_whose_lease_is_taken_stops_and_resumes_once_it_is_free(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    use std::sync::atomic::{AtomicU32, Ordering};

    let w = world(opts, connect).await;
    let name = "observer:test";
    // Renewed every 3 seconds; it would lapse 9 seconds after the last one.
    let lease = Lease::new(w.worker_pool.clone(), name.to_owned(), Duration::from_secs(9));
    let (started, stopped) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
    let task = {
        let (started, stopped) = (Arc::clone(&started), Arc::clone(&stopped));
        tokio::spawn(async move {
            lease::lead(&lease, "observer", std::future::pending(), |stop| {
                let (started, stopped) = (Arc::clone(&started), Arc::clone(&stopped));
                async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    stop.wait().await;
                    stopped.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await;
        })
    };
    eventually("the work starting", || async { started.load(Ordering::SeqCst) == 1 }).await;

    // Another process holds the lease now: the next renewal is refused and
    // the work is told to stop, well before the lease would have lapsed.
    sqlx::query(
        "UPDATE pay_stellar.leases SET holder = gen_random_uuid(), \
         expires_at = now() + interval '1 hour'",
    )
    .execute(&w.h.owner)
    .await
    .unwrap();
    within(Duration::from_secs(5), "the work stopping", || async {
        stopped.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(started.load(Ordering::SeqCst), 1, "it does not act while another holds the lease");

    // Once that holder releases it, this process takes it back.
    sqlx::query("DELETE FROM pay_stellar.leases").execute(&w.h.owner).await.unwrap();
    eventually("the work starting again", || async { started.load(Ordering::SeqCst) == 2 }).await;
    task.abort();
}

// ---- withdrawals ------------------------------------------------------------

fn usdc_of(w: &World, owner: &AccountAddress) -> i128 {
    w.stellar.with(|n| n.state.usdc.get(owner).copied().unwrap_or(0))
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_withdrawal_holds_its_amount_and_pays_it_out_once(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let wallet = x.key.address();
    assert_eq!(usdc_of(&w, &wallet), 0);
    let (prepared, signed) = w.prepared_withdrawal(&x, 40, "w-1").await.unwrap();
    // Nothing is held until the buyer's signature is stored.
    assert_eq!(prepared.state(), WithdrawalState::AwaitingSignature);
    assert_eq!(w.balance(&x).await, (100, 0));

    let held = w.submit_withdrawal(&prepared, &signed).await.unwrap();
    assert_eq!(held.state(), WithdrawalState::Signed);
    assert_eq!(w.balance(&x).await, (60, 0));
    assert_eq!(w.withdrawing(&x).await, 40);
    // Submitting the same signed entry again changes nothing.
    w.submit_withdrawal(&prepared, &signed).await.unwrap();
    assert_eq!(w.balance(&x).await, (60, 0));

    w.settle(&w.paying_worker()).await;
    let paid = w.get_withdrawal(&held.withdrawal_id).await;
    assert_eq!(paid.state(), WithdrawalState::Confirmed);
    assert!(!paid.transaction_hash.is_empty());
    assert_eq!(w.balance(&x).await, (60, 0));
    assert_eq!(w.withdrawing(&x).await, 0);
    assert_eq!(w.stellar.account(&wallet), Some(60));
    assert_eq!(usdc_of(&w, &wallet), 40);
    assert_eq!(w.stellar.with(|n| n.state.treasury_usdc), 60);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_withdrawal_above_the_available_balance_holds_nothing(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    w.charge(&x, 50, "c-1").await;
    let refused = w.prepared_withdrawal(&x, 60, "w-1").await.unwrap_err();
    assert_refused(&refused, tonic::Code::FailedPrecondition, "insufficient_balance");

    // Covered when prepared, but a charge admitted before the signature is
    // stored leaves too little: the signature is refused and nothing held.
    let (prepared, signed) = w.prepared_withdrawal(&x, 40, "w-2").await.unwrap();
    w.charge(&x, 20, "c-2").await;
    let refused = w.submit_withdrawal(&prepared, &signed).await.unwrap_err();
    assert_refused(&refused, tonic::Code::FailedPrecondition, "insufficient_balance");
    assert_eq!(w.balance(&x).await, (30, 70));
    assert_eq!(
        w.get_withdrawal(&prepared.withdrawal_id).await.state(),
        WithdrawalState::AwaitingSignature
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_withdrawal_nobody_can_send_is_returned_once_its_authorization_lapses(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let held = w.withdraw(&x, 40, "w-1").await;
    // Prepared and never signed: it holds nothing, so it returns nothing.
    let (unsigned, _) = w.prepared_withdrawal(&x, 30, "w-2").await.unwrap();
    // A worker without the treasury's key sends nothing.
    let worker = w.worker();
    w.settle(&worker).await;
    assert_eq!(w.get_withdrawal(&held.withdrawal_id).await.state(), WithdrawalState::Signed);
    assert_eq!(w.balance(&x).await, (60, 0));

    w.stellar.set_latest(held.expiration_ledger);
    w.settle(&worker).await;
    assert_eq!(w.get_withdrawal(&held.withdrawal_id).await.state(), WithdrawalState::Signed);
    w.stellar.set_latest(held.expiration_ledger + 1);
    w.settle(&worker).await;
    assert_eq!(w.get_withdrawal(&held.withdrawal_id).await.state(), WithdrawalState::Expired);
    assert_eq!(w.get_withdrawal(&unsigned.withdrawal_id).await.state(), WithdrawalState::Expired);
    assert_eq!(w.balance(&x).await, (100, 0));
    assert_eq!(w.withdrawing(&x).await, 0);
    assert_eq!(w.stellar.account(&x.key.address()), Some(100));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_withdrawal_refused_while_the_treasury_is_short_is_sent_once_it_is_funded(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let held = w.withdraw(&x, 40, "w-1").await;
    w.stellar.with(|n| n.state.treasury_usdc = 10);
    let worker = w.paying_worker();
    assert_eq!(worker.step().await.unwrap(), Step::Idle);
    assert_eq!(w.get_withdrawal(&held.withdrawal_id).await.state(), WithdrawalState::Signed);
    let error = w.last_error("withdrawals", &held.withdrawal_id).await.unwrap();
    assert!(error.contains("USDC balance too low"), "{error}");

    w.stellar.with(|n| n.state.treasury_usdc = 100);
    w.clock.advance(RETRY_AFTER + Duration::from_secs(1));
    w.settle(&worker).await;
    assert_eq!(w.get_withdrawal(&held.withdrawal_id).await.state(), WithdrawalState::Confirmed);
    assert_eq!(usdc_of(&w, &x.key.address()), 40);
    assert_eq!(w.balance(&x).await, (60, 0));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_withdrawal_included_elsewhere_is_confirmed_from_the_marker_and_not_returned(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let held = w.withdraw(&x, 40, "w-1").await;
    let worker = w.paying_worker();
    let sent_before = w.stellar.sent().len();
    w.stellar.with(|n| n.drop_sends = 1);
    worker.step().await.unwrap();
    // A copy of the broadcast authorizations lands in someone else's
    // transaction; ours never does.
    w.stellar.include_elsewhere(&w.stellar.sent()[sent_before]);
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.settle(&worker).await;
    // Sent again with a fresh treasury authorization, which the contract
    // refuses: the buyer's nonce is spent and the marker exists.
    assert_eq!(w.get_withdrawal(&held.withdrawal_id).await.state(), WithdrawalState::Signed);
    assert_eq!(w.balance(&x).await, (60, 0));

    w.stellar.set_latest(held.expiration_ledger + 1);
    w.settle(&worker).await;
    assert_eq!(w.get_withdrawal(&held.withdrawal_id).await.state(), WithdrawalState::Confirmed);
    assert_eq!(w.balance(&x).await, (60, 0));
    assert_eq!(usdc_of(&w, &x.key.address()), 40);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_withdrawal_included_as_failed_is_returned_only_after_its_authorization_lapses(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let held = w.withdraw(&x, 40, "w-1").await;
    let worker = w.paying_worker();
    w.stellar.with(|n| n.fail_inclusions = 1);
    w.settle(&worker).await;
    // The buyer's signed entry could still be included elsewhere: the
    // amount stays held.
    assert_eq!(w.get_withdrawal(&held.withdrawal_id).await.state(), WithdrawalState::Submitted);
    assert_eq!(w.balance(&x).await, (60, 0));

    w.stellar.set_latest(held.expiration_ledger + 1);
    w.stellar.with(|n| n.entries_behind = 1);
    w.settle(&worker).await;
    assert_eq!(w.get_withdrawal(&held.withdrawal_id).await.state(), WithdrawalState::Submitted);

    w.stellar.with(|n| n.entries_behind = 0);
    w.settle(&worker).await;
    assert_eq!(w.get_withdrawal(&held.withdrawal_id).await.state(), WithdrawalState::Failed);
    assert_eq!(w.balance(&x).await, (100, 0));
    assert_eq!(usdc_of(&w, &x.key.address()), 0);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_withdrawal_requests_repeat_by_key_and_refuse_what_they_cannot_honour(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world_with(opts, connect, true).await;
    let x = w.funded("x", 100).await;
    let world = &w;
    let prepare = |amount: i64, destination: String, key: &str| {
        let request = PrepareWithdrawalRequest {
            buyer_id: x.id.clone(),
            amount,
            destination,
            idempotency_key: key.to_owned(),
        };
        let token = world.tenant.token.clone();
        async move {
            world
                .ledger()
                .await
                .prepare_withdrawal(authed(request, &token))
                .await
                .map(tonic::Response::into_inner)
        }
    };
    let other = SecretKey::generate().unwrap().address();

    let first = prepare(40, other.to_string(), "w-1").await.unwrap();
    assert!(first.created);
    assert_eq!(first.withdrawal.as_ref().unwrap().destination, other.to_string());
    let again = prepare(40, other.to_string(), "w-1").await.unwrap();
    assert!(!again.created);
    assert_eq!(again.withdrawal, first.withdrawal);
    // The same key for another amount or destination is another request.
    let conflict = prepare(41, other.to_string(), "w-1").await.unwrap_err();
    assert_refused(&conflict, tonic::Code::AlreadyExists, "idempotency_conflict");
    let conflict = prepare(40, String::new(), "w-1").await.unwrap_err();
    assert_refused(&conflict, tonic::Code::AlreadyExists, "idempotency_conflict");

    // An empty destination is the buyer's wallet; anything else must be an
    // account address.
    let own = prepare(10, String::new(), "w-2").await.unwrap();
    assert_eq!(own.withdrawal.unwrap().destination, x.key.address().to_string());
    let invalid = prepare(10, "CAAA".to_owned(), "w-3").await.unwrap_err();
    assert_refused(&invalid, tonic::Code::InvalidArgument, "invalid_destination");

    let missing = w
        .ledger()
        .await
        .get_withdrawal(authed(
            GetWithdrawalRequest { withdrawal_id: uuid::Uuid::now_v7().to_string() },
            &w.tenant.token,
        ))
        .await
        .unwrap_err();
    assert_refused(&missing, tonic::Code::NotFound, "withdrawal_not_found");
    // Nothing prepared holds anything.
    assert_eq!(w.balance(&x).await, (100, 0));
}

// ---- the cold reserve ---------------------------------------------------------

/// A reserve account with a USDC trustline.
fn reserve_account(w: &World) -> AccountAddress {
    let cold = SecretKey::generate().unwrap().address();
    w.stellar.with(|n| n.state.lines.insert(cold.clone()));
    cold
}

/// Sweeps above 60 down to 30; below 10 the treasury needs topping up.
fn sweeping_worker(w: &World, cold: &AccountAddress) -> Worker<Stellar, ManualClock> {
    w.paying_worker().with_reserve(Reserve::new(cold.clone(), 10, 30, 60).unwrap())
}

async fn sweeps(w: &World) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM pay_stellar.submissions WHERE kind = 'sweep'")
        .fetch_one(&w.h.owner)
        .await
        .unwrap()
}

fn treasury_usdc(w: &World) -> i128 {
    w.stellar.with(|n| n.state.treasury_usdc)
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_the_surplus_above_the_ceiling_is_swept_down_to_the_target(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.funded("x", 100).await;
    assert_eq!(treasury_usdc(&w), 100);
    let cold = reserve_account(&w);
    let worker = sweeping_worker(&w, &cold);
    w.settle(&worker).await;
    assert_eq!((treasury_usdc(&w), usdc_of(&w, &cold)), (30, 70));
    assert_eq!(sweeps(&w).await, 1);

    // At or below the ceiling nothing more moves, however often it is read,
    // once the first sweep's authorization no longer holds anything back.
    w.funded("y", 30).await;
    w.stellar.set_latest(w.stellar.latest() + OPERATOR_LEDGERS + 1);
    w.clock.advance(Duration::from_secs(61));
    w.settle(&worker).await;
    assert_eq!((treasury_usdc(&w), usdc_of(&w, &cold)), (60, 70));
    assert_eq!(sweeps(&w).await, 1);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_sweep_leaves_what_held_withdrawals_need(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let held = w.withdraw(&x, 50, "w-1").await;
    let cold = reserve_account(&w);
    w.settle(&sweeping_worker(&w, &cold)).await;
    // 50 stays for the withdrawal, more than the target of 30.
    assert_eq!(usdc_of(&w, &cold), 50);
    assert_eq!(w.get_withdrawal(&held.withdrawal_id).await.state(), WithdrawalState::Confirmed);
    assert_eq!(usdc_of(&w, &x.key.address()), 50);
    assert_eq!(treasury_usdc(&w), 0);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_reserve_without_a_trustline_receives_nothing_until_it_has_one(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.funded("x", 100).await;
    let cold = SecretKey::generate().unwrap().address();
    let worker = sweeping_worker(&w, &cold);
    w.settle(&worker).await;
    assert_eq!((treasury_usdc(&w), sweeps(&w).await), (100, 0));

    w.stellar.with(|n| n.state.lines.insert(cold.clone()));
    w.settle(&worker).await;
    assert_eq!(sweeps(&w).await, 0, "read again only after the check interval");
    w.clock.advance(Duration::from_secs(61));
    w.settle(&worker).await;
    assert_eq!((treasury_usdc(&w), usdc_of(&w, &cold)), (30, 70));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_sweep_not_included_is_followed_only_once_its_authorization_lapses(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.funded("x", 100).await;
    let cold = reserve_account(&w);
    let worker = sweeping_worker(&w, &cold);
    w.stellar.with(|n| n.drop_sends = 1);
    worker.step().await.unwrap();
    assert_eq!(sweeps(&w).await, 1);
    // The envelope expires unincluded, but its treasury authorization could
    // still be included by someone else: no new sweep yet.
    w.clock.advance(VALIDITY + Duration::from_secs(61));
    w.settle(&worker).await;
    assert_eq!((sweeps(&w).await, treasury_usdc(&w)), (1, 100));

    w.stellar.set_latest(w.stellar.latest() + OPERATOR_LEDGERS + 1);
    w.clock.advance(Duration::from_secs(61));
    w.settle(&worker).await;
    assert_eq!(sweeps(&w).await, 2);
    assert_eq!((treasury_usdc(&w), usdc_of(&w, &cold)), (30, 70));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_two_workers_sweeping_at_once_send_one_sweep(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.funded("x", 100).await;
    let cold = reserve_account(&w);
    let sweeper = |key: SecretKey| {
        w.worker_with_sources(vec![key], 100)
            .with_treasury(LocalSigner::arc(treasury_key()))
            .with_reserve(Reserve::new(cold.clone(), 10, 30, 60).unwrap())
    };
    // Each from its own source account, as a worker finishing its round
    // after losing its lease and the one taking over would.
    let (first, second) = (sweeper(extra_source(&w)), sweeper(extra_source(&w)));
    w.stellar.with(|n| n.simulation_barrier = Some(Arc::new(Barrier::new(2))));
    let (a, b) = tokio::join!(first.step(), second.step());
    w.stellar.with(|n| n.simulation_barrier = None);
    a.unwrap();
    b.unwrap();
    assert_eq!(sweeps(&w).await, 1);
    w.settle(&first).await;
    w.settle(&second).await;
    assert_eq!((treasury_usdc(&w), usdc_of(&w, &cold)), (30, 70));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_sweep_from_a_trailing_balance_leaves_the_target(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let paid = w.withdraw(&x, 20, "w-1").await;
    w.settle(&w.paying_worker()).await;
    assert_eq!(w.get_withdrawal(&paid.withdrawal_id).await.state(), WithdrawalState::Confirmed);
    assert_eq!(treasury_usdc(&w), 80);
    // The node serving the balance has not seen the withdrawal's ledger yet.
    w.stellar.with(|n| {
        n.entries_behind = 1;
        n.stale_treasury = Some(100);
    });
    let cold = reserve_account(&w);
    sweeping_worker(&w, &cold).step().await.unwrap();
    w.stellar.with(|n| {
        n.entries_behind = 0;
        n.stale_treasury = None;
    });
    assert_eq!((treasury_usdc(&w), usdc_of(&w, &cold)), (30, 50));
}

/// The API stores a withdrawal's signature with the buyer row locked first
/// and the withdrawal row second; closing a lapsed withdrawal must take them
/// in the same order, or the two deadlock.
#[sqlx::test(migrations = "../../db/migrations")]
async fn test_closing_a_withdrawal_locks_the_buyer_before_the_withdrawal(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    use sqlx::{Connection as _, Executor as _};

    let w = world(opts, connect.clone()).await;
    let x = w.funded("x", 100).await;
    let (unsigned, _) = w.prepared_withdrawal(&x, 30, "w-1").await.unwrap();
    w.stellar.set_latest(unsigned.expiration_ledger + 1);

    // As the API storing a signature: the buyer row is locked.
    let mut api = sqlx::PgConnection::connect_with(&connect).await.unwrap();
    api.execute("BEGIN").await.unwrap();
    sqlx::query("SELECT id FROM pay_stellar.buyers WHERE id = $1::uuid FOR UPDATE")
        .bind(&x.id)
        .execute(&mut api)
        .await
        .unwrap();
    let worker = w.worker();
    let closing = tokio::spawn(async move { worker.step().await });
    eventually("the worker waiting on a lock", || async {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&w.h.owner)
        .await
        .unwrap();
        waiting >= 1
    })
    .await;
    // The API then reaches the withdrawal row: the worker must not hold it.
    sqlx::query("SELECT id FROM pay_stellar.withdrawals WHERE id = $1::uuid FOR UPDATE NOWAIT")
        .bind(&unsigned.withdrawal_id)
        .execute(&mut api)
        .await
        .expect("the worker holds the withdrawal row while it waits for the buyer");
    api.execute("ROLLBACK").await.unwrap();
    closing.await.unwrap().unwrap();
    assert_eq!(w.get_withdrawal(&unsigned.withdrawal_id).await.state(), WithdrawalState::Expired);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_charge_past_the_daily_limit_is_refused_and_returned(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let y = w.funded("y", 100).await;
    w.stellar.with(|n| n.over_daily.insert(x.key.address()));
    let refused = w.charge(&x, 30, "c-1").await;
    let charged = w.charge(&y, 30, "c-2").await;
    w.settle(&w.worker()).await;
    let refused = w.get_charge(&refused.charge_id).await;
    assert_eq!(
        (refused.state(), refused.outcome.as_str()),
        (ChargeState::Refused, "above_daily_limit")
    );
    assert_eq!(w.get_charge(&charged.charge_id).await.state(), ChargeState::Charged);
    assert_eq!(w.balance(&x).await, (100, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some(100));
}

// ---- randomized fault simulation ---------------------------------------------
//
// Seeded scenarios: charges, deposits and withdrawals through the API,
// mixed with lost sends, failed inclusions, a node that trails the network,
// authorizations included by someone else (also copies of envelopes the
// worker has already decided on), time passing and worker restarts. After
// everything drains, the database and the contract must agree to the unit.
// `DST_SEEDS` runs more seeds (5 by default) from `DST_SEED_START` (1), and
// `DST_SEED` one seed, to reproduce a failure.

/// xorshift64*: a fixed, dependency-free generator, so a seed replays the
/// same scenario.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Someone else includes the call and authorizations of `envelope`, if the
/// contract accepts them now; restores, extensions and sweeps are skipped.
fn maybe_include_elsewhere(w: &World, envelope: &TransactionEnvelope) {
    let tx = inner(envelope);
    let OperationBody::InvokeHostFunction(op) = &tx.operations[0].body else { return };
    let HostFunction::InvokeContract(call) = &op.host_function else { return };
    if call.contract_address != ScAddress::Contract(ContractId(Hash(CONTRACT))) {
        return;
    }
    let operator = w.stellar.operator.clone();
    w.stellar.with(|n| {
        if let Ok((state, _, event)) = n.execute(call, &op.auth, &operator) {
            n.state = state;
            if let Some((name, data)) = event {
                n.log_event(name, data);
            }
        }
    });
}

async fn simulate(w: &World, seed: u64) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let buyers = [w.funded("a", 150).await, w.funded("b", 150).await, w.funded("c", 150).await];
    // USDC left in each wallet for the deposits to come.
    for buyer in &buyers {
        w.stellar.with(|n| n.state.usdc.insert(buyer.key.address(), 1_000));
    }
    let mut worker = w.paying_worker();
    let mut serial = 0_u32;
    let mut key = |kind: &str| {
        serial += 1;
        format!("{kind}-{serial}")
    };
    for _ in 0..80 {
        let buyer = &buyers[usize::try_from(rng.below(3)).unwrap()];
        match rng.below(12) {
            0..=3 => {
                // Up to above the contract's per-charge limit, which the API
                // does not check.
                let request = CreateChargeRequest {
                    buyer_id: buyer.id.clone(),
                    amount: i64::try_from(1 + rng.below(60)).unwrap(),
                    idempotency_key: key("c"),
                };
                // Refused when the balance is short: part of the scenario.
                let _ = w.ledger().await.create_charge(authed(request, &w.tenant.token)).await;
            }
            4 => {
                let amount = i64::try_from(10 + rng.below(30)).unwrap();
                let (deposit, signed) = w.prepared_deposit(buyer, amount, &key("d")).await;
                let request = SubmitDepositRequest {
                    deposit_id: deposit.deposit_id,
                    signed_authorization_entry_xdr: signed,
                };
                w.ledger().await.submit_deposit(authed(request, &w.tenant.token)).await.unwrap();
            }
            5 => {
                let amount = i64::try_from(1 + rng.below(15)).unwrap();
                if let Ok((withdrawal, signed)) =
                    w.prepared_withdrawal(buyer, amount, &key("w")).await
                {
                    let _ = w.submit_withdrawal(&withdrawal, &signed).await;
                }
            }
            6 => match rng.below(5) {
                0 => w.stellar.with(|n| n.drop_sends = usize::try_from(1 + rng.below(3)).unwrap()),
                1 => w.stellar.with(|n| n.fail_inclusions = 1),
                2 => w.stellar.with(|n| n.entries_behind = u32::try_from(rng.below(3)).unwrap()),
                3 => {
                    let owner = buyer.key.address();
                    w.stellar.with(|n| {
                        if !n.over_daily.remove(&owner) {
                            n.over_daily.insert(owner);
                        }
                    });
                }
                _ => w.stellar.with(|n| n.drop_sends = 1),
            },
            7..=8 => {
                w.clock.advance(Duration::from_secs(10 + rng.below(90)));
                let ledgers = u32::try_from(1 + rng.below(40)).unwrap();
                w.stellar.set_latest(w.stellar.latest() + ledgers);
            }
            // Long enough for charges waiting in the queue to pass their
            // last ledger and for authorizations to lapse.
            9 => {
                w.clock.advance(Duration::from_secs(600));
                w.stellar.set_latest(w.stellar.latest() + 800);
            }
            10 => worker = w.paying_worker(),
            _ => {}
        }
        // Someone else includes a copy of an envelope sent earlier, if its
        // authorizations still hold: possibly after the worker has already
        // decided that envelope's outcome.
        if rng.below(3) == 0 {
            let sent = w.stellar.sent();
            if !sent.is_empty() {
                let pick = usize::try_from(rng.below(sent.len() as u64)).unwrap();
                maybe_include_elsewhere(w, &sent[pick]);
            }
        }
        // Some rounds pass without the worker, so work queues up.
        if rng.below(4) != 0 {
            worker.step().await.unwrap();
            // A copy of something just sent, included right after the worker
            // acted on it, while its authorizations are fresh.
            if rng.below(2) == 0 {
                let sent = w.stellar.sent();
                let recent = sent.len().saturating_sub(4);
                if let Some(pick) = sent.get(recent..).and_then(|tail| {
                    (!tail.is_empty())
                        .then(|| recent + usize::try_from(rng.below(tail.len() as u64)).unwrap())
                }) {
                    maybe_include_elsewhere(w, &sent[pick]);
                }
            }
        }
    }

    // Drain: no more faults, and time enough for every authorization and
    // every charge's last ledger to lapse.
    w.stellar.with(|n| {
        n.drop_sends = 0;
        n.fail_inclusions = 0;
        n.entries_behind = 0;
        n.over_daily.clear();
    });
    for _ in 0..12 {
        w.clock.advance(VALIDITY + Duration::from_secs(1));
        w.stellar.set_latest(w.stellar.latest() + 400);
        w.settle(&worker).await;
    }
}

async fn assert_books_agree(w: &World, seed: u64) {
    let open: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM pay_stellar.charges WHERE state NOT IN ('charged', 'refused'))
              + (SELECT count(*) FROM pay_stellar.deposits
                 WHERE state NOT IN ('confirmed', 'failed', 'expired'))
              + (SELECT count(*) FROM pay_stellar.withdrawals
                 WHERE state NOT IN ('confirmed', 'failed', 'expired'))",
    )
    .fetch_one(&w.h.owner)
    .await
    .unwrap();
    assert_eq!(open, 0, "seed {seed}: rows left open or quarantined");

    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT wallet_address, available FROM pay_stellar.buyers WHERE seller_deployment_id = $1",
    )
    .bind(w.tenant.deployment_id)
    .fetch_all(&w.h.owner)
    .await
    .unwrap();
    for (wallet, available) in rows {
        let account = w.stellar.account(&wallet.parse().unwrap()).unwrap_or(0);
        assert_eq!(i128::from(available), account, "seed {seed}: {wallet} available");
    }

    // A charge is `charged` exactly when the contract's record says so.
    let charges: Vec<(String, Vec<u8>, String, i64)> = sqlx::query_as(
        "SELECT b.wallet_address, c.charge_id, c.state, c.amount FROM pay_stellar.charges c
         JOIN pay_stellar.buyers b ON b.id = c.buyer_id",
    )
    .fetch_all(&w.h.owner)
    .await
    .unwrap();
    let mut charged = 0_i128;
    for (wallet, id, state, amount) in charges {
        let owner: AccountAddress = wallet.parse().unwrap();
        let id: [u8; 32] = id.try_into().unwrap();
        let recorded =
            w.stellar.with(|n| n.state.records.get(&(owner, id)).map(|(code, ..)| *code));
        assert_eq!(state == "charged", recorded == Some(0), "seed {seed}: charge {state}");
        if state == "charged" {
            charged += i128::from(amount);
        }
    }
    let (accounts, treasury) =
        w.stellar.with(|n| (n.state.accounts.values().sum::<i128>(), n.state.treasury_usdc));
    assert_eq!(treasury, accounts + charged, "seed {seed}: the treasury holds what is owed");
    let outcomes: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT 'charge', state, count(*) FROM pay_stellar.charges GROUP BY state
         UNION ALL SELECT 'deposit', state, count(*) FROM pay_stellar.deposits GROUP BY state
         UNION ALL SELECT 'withdrawal', state, count(*) FROM pay_stellar.withdrawals GROUP BY state
         UNION ALL SELECT 'submission', state, count(*) FROM pay_stellar.submissions GROUP BY state
         ORDER BY 1, 2",
    )
    .fetch_all(&w.h.owner)
    .await
    .unwrap();
    eprintln!("seed {seed}: {outcomes:?}");
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../db/migrations");

/// A new, migrated database on the same server, so each seed starts from
/// nothing: one contract binds to one deployment per database.
async fn fresh_database(connect: &PgConnectOptions, name: &str) -> PgConnectOptions {
    use sqlx::{Connection as _, Executor as _};

    let mut server = sqlx::PgConnection::connect_with(connect).await.unwrap();
    server
        .execute(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")))
        .await
        .unwrap();
    server.execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).await.unwrap();
    let target = connect.clone().database(name);
    let pool = PgPoolOptions::new().max_connections(1).connect_with(target.clone()).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    pool.close().await;
    target
}

async fn drop_database(connect: &PgConnectOptions, name: &str) {
    use sqlx::{Connection as _, Executor as _};

    let mut server = sqlx::PgConnection::connect_with(connect).await.unwrap();
    server
        .execute(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")))
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_random_faults_leave_the_database_and_the_contract_in_agreement(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let setting = |name: &str, default: u64| {
        std::env::var(name).ok().and_then(|value| value.parse().ok()).unwrap_or(default)
    };
    let seeds: Vec<u64> = match std::env::var("DST_SEED") {
        Ok(seed) => vec![seed.parse().unwrap()],
        Err(_) => {
            let start = setting("DST_SEED_START", 1);
            (start..start + setting("DST_SEEDS", 5)).collect()
        }
    };
    for seed in seeds {
        let name = format!("pay_stellar_dst_{}_{seed}", std::process::id());
        let target = fresh_database(&connect, &name).await;
        let w = world(opts.clone(), target).await;
        simulate(&w, seed).await;
        assert_books_agree(&w, seed).await;
        drop(w);
        drop_database(&connect, &name).await;
    }
}

// ---- recurring charges ------------------------------------------------------

/// A period short enough for a test to cross several.
const PERIOD: u64 = 120;

impl World {
    fn sign(&self, buyer: &TestBuyer, entry_xdr: &str) -> String {
        let entry = SorobanAuthorizationEntry::from_xdr_base64(entry_xdr, Limits::none()).unwrap();
        let signed = sign_entry(&entry, network_id(Network::Testnet), &[&buyer.key]).unwrap();
        signed.to_xdr_base64(Limits::none()).unwrap()
    }

    /// A mandate of `cycles` periods of up to `amount`, signed and stored.
    async fn mandate(&self, buyer: &TestBuyer, amount: i64, cycles: u32, key: &str) -> Mandate {
        let request = PrepareMandateRequest {
            buyer_id: buyer.id.clone(),
            amount,
            period_secs: PERIOD,
            cycles,
            idempotency_key: key.to_owned(),
        };
        let mut ledger = self.ledger().await;
        let prepared = ledger
            .prepare_mandate(authed(request, &self.tenant.token))
            .await
            .unwrap()
            .into_inner()
            .mandate
            .unwrap();
        let request = SubmitMandateRequest {
            mandate_id: prepared.mandate_id.clone(),
            signed_authorization_entry_xdr: self.sign(buyer, &prepared.authorization_entry_xdr),
        };
        ledger
            .submit_mandate(authed(request, &self.tenant.token))
            .await
            .unwrap()
            .into_inner()
            .mandate
            .unwrap()
    }

    async fn get_mandate(&self, id: &str) -> Mandate {
        self.ledger()
            .await
            .get_mandate(authed(
                GetMandateRequest { mandate_id: id.to_owned() },
                &self.tenant.token,
            ))
            .await
            .unwrap()
            .into_inner()
            .mandate
            .unwrap()
    }

    async fn revoke(&self, buyer: &TestBuyer, key: &str) -> Revocation {
        let request = PrepareRevocationRequest {
            buyer_id: buyer.id.clone(),
            idempotency_key: key.to_owned(),
        };
        let mut ledger = self.ledger().await;
        let prepared = ledger
            .prepare_revocation(authed(request, &self.tenant.token))
            .await
            .unwrap()
            .into_inner()
            .revocation
            .unwrap();
        let request = SubmitRevocationRequest {
            revocation_id: prepared.revocation_id.clone(),
            signed_authorization_entry_xdr: self.sign(buyer, &prepared.authorization_entry_xdr),
        };
        ledger
            .submit_revocation(authed(request, &self.tenant.token))
            .await
            .unwrap()
            .into_inner()
            .revocation
            .unwrap()
    }

    async fn get_revocation(&self, id: &str) -> Revocation {
        self.ledger()
            .await
            .get_revocation(authed(
                GetRevocationRequest { revocation_id: id.to_owned() },
                &self.tenant.token,
            ))
            .await
            .unwrap()
            .into_inner()
            .revocation
            .unwrap()
    }

    async fn charge_period(
        &self,
        mandate: &Mandate,
        amount: i64,
        key: &str,
    ) -> Result<RecurringCharge, tonic::Status> {
        let request = CreateRecurringChargeRequest {
            mandate_id: mandate.mandate_id.clone(),
            amount,
            idempotency_key: key.to_owned(),
        };
        Ok(self
            .ledger()
            .await
            .create_recurring_charge(authed(request, &self.tenant.token))
            .await?
            .into_inner()
            .recurring_charge
            .unwrap())
    }

    async fn get_recurring(&self, id: &str) -> RecurringCharge {
        self.ledger()
            .await
            .get_recurring_charge(authed(
                GetRecurringChargeRequest { recurring_charge_id: id.to_owned() },
                &self.tenant.token,
            ))
            .await
            .unwrap()
            .into_inner()
            .recurring_charge
            .unwrap()
    }

    fn wallet(&self, buyer: &TestBuyer) -> i128 {
        self.stellar.with(|n| n.state.usdc.get(&buyer.key.address()).copied().unwrap_or(0))
    }

    fn treasury_usdc(&self) -> i128 {
        self.stellar.with(|n| n.state.treasury_usdc)
    }

    /// Moves ledger time `secs` ahead, and the ledger with it.
    fn later(&self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
        self.stellar.with(|n| n.latest += u32::try_from(secs / 5).unwrap());
    }

    /// An active mandate of `cycles` periods of up to `amount`.
    async fn active_mandate(&self, buyer: &TestBuyer, amount: i64, cycles: u32) -> Mandate {
        let mandate = self.mandate(buyer, amount, cycles, &format!("m-{}", buyer.id)).await;
        self.settle(&self.worker()).await;
        let active = self.get_mandate(&mandate.mandate_id).await;
        assert_eq!(active.state(), MandateState::Active);
        active
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_mandate_is_activated_and_each_period_charged_once_from_the_wallet(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.active_mandate(&buyer, 10, 3).await;
    assert!(mandate.starts_at > 0, "the contract's start is recorded");
    assert_eq!(w.stellar.with(|n| n.state.allowances[&buyer.key.address()].0), 30);

    let first = w.charge_period(&mandate, 10, "c-1").await.unwrap();
    assert_eq!((first.cycle, first.state()), (0, RecurringChargeState::Admitted));
    // The period is taken while its charge is in flight or charged.
    let again = w.charge_period(&mandate, 10, "c-2").await.unwrap_err();
    assert_refused(&again, tonic::Code::AlreadyExists, "period_already_charged");
    w.settle(&w.worker()).await;
    let charged = w.get_recurring(&first.recurring_charge_id).await;
    assert_eq!(
        (charged.state(), charged.outcome.as_str()),
        (RecurringChargeState::Charged, "charged")
    );

    w.later(PERIOD);
    let second = w.charge_period(&mandate, 7, "c-3").await.unwrap();
    assert_eq!(second.cycle, 1);
    w.settle(&w.worker()).await;
    assert_eq!(
        w.get_recurring(&second.recurring_charge_id).await.state(),
        RecurringChargeState::Charged
    );
    // From the wallet to the treasury; the prepaid balance is untouched.
    assert_eq!((w.wallet(&buyer), w.treasury_usdc(), w.balance(&buyer).await), (83, 17, (0, 0)));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_refused_period_can_be_attempted_again_while_it_lasts(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 5).await;
    let mandate = w.active_mandate(&buyer, 10, 3).await;
    let short = w.charge_period(&mandate, 10, "c-1").await.unwrap();
    w.settle(&w.worker()).await;
    let refused = w.get_recurring(&short.recurring_charge_id).await;
    assert_eq!(
        (refused.state(), refused.outcome.as_str()),
        (RecurringChargeState::Refused, "wallet_short")
    );
    w.stellar.with(|n| n.state.usdc.insert(buyer.key.address(), 20));
    let retried = w.charge_period(&mandate, 10, "c-2").await.unwrap();
    assert_eq!(retried.cycle, 0);
    w.settle(&w.worker()).await;
    assert_eq!(
        w.get_recurring(&retried.recurring_charge_id).await.state(),
        RecurringChargeState::Charged
    );
    assert_eq!(w.wallet(&buyer), 10);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_the_api_refuses_charges_the_mandate_does_not_allow(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let pending = w.mandate(&buyer, 10, 2, "m-1").await;
    let refused = w.charge_period(&pending, 5, "c-0").await.unwrap_err();
    assert_refused(&refused, tonic::Code::FailedPrecondition, "mandate_not_active");
    w.settle(&w.worker()).await;
    let mandate = w.get_mandate(&pending.mandate_id).await;
    let refused = w.charge_period(&mandate, 11, "c-1").await.unwrap_err();
    assert_refused(&refused, tonic::Code::InvalidArgument, "above_mandate");
    // Past both periods the mandate is over, for the API and the worker.
    w.later(2 * PERIOD);
    let refused = w.charge_period(&mandate, 5, "c-2").await.unwrap_err();
    assert_refused(&refused, tonic::Code::FailedPrecondition, "mandate_ended");
    w.settle(&w.worker()).await;
    assert_eq!(w.get_mandate(&mandate.mandate_id).await.state(), MandateState::Ended);
    assert_eq!(w.wallet(&buyer), 100);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_mandate_included_elsewhere_is_activated_from_the_contract_once_its_entry_lapses(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.mandate(&buyer, 10, 3, "m-1").await;
    let worker = w.worker();
    w.stellar.with(|n| n.drop_sends = 1);
    worker.step().await.unwrap();
    w.stellar.include_elsewhere(&w.stellar.sent()[0]);
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.settle(&worker).await;
    // Sending it again is refused (the nonce is spent); it waits.
    assert_eq!(w.get_mandate(&mandate.mandate_id).await.state(), MandateState::Signed);
    w.stellar.set_latest(mandate.expiration_ledger + 1);
    w.settle(&worker).await;
    assert_eq!(w.get_mandate(&mandate.mandate_id).await.state(), MandateState::Active);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_mandate_never_included_expires_once_its_entry_lapses(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.mandate(&buyer, 10, 3, "m-1").await;
    let worker = w.worker();
    w.stellar.with(|n| n.fail_inclusions = 1);
    worker.step().await.unwrap();
    w.settle(&worker).await;
    assert_eq!(w.get_mandate(&mandate.mandate_id).await.state(), MandateState::Submitted);
    w.stellar.set_latest(mandate.expiration_ledger + 1);
    w.settle(&worker).await;
    assert_eq!(w.get_mandate(&mandate.mandate_id).await.state(), MandateState::Failed);
    assert!(w.stellar.with(|n| n.state.mandates.is_empty()));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_successful_mandate_is_not_decided_from_a_read_older_than_its_ledger(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.mandate(&buyer, 10, 3, "m-1").await;
    w.stellar.with(|n| n.entries_behind = 5);
    let worker = w.worker();
    w.settle(&worker).await;
    assert_eq!(w.get_mandate(&mandate.mandate_id).await.state(), MandateState::Submitted);
    w.stellar.with(|n| n.entries_behind = 0);
    w.settle(&worker).await;
    assert_eq!(w.get_mandate(&mandate.mandate_id).await.state(), MandateState::Active);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_new_mandate_replaces_the_active_one_and_its_charges_are_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let first = w.active_mandate(&buyer, 10, 3).await;
    // A charge admitted for the old mandate, then a new mandate lands first.
    let stale = w.charge_period(&first, 10, "c-1").await.unwrap();
    let second = w.mandate(&buyer, 4, 2, "m-2").await;
    let worker = w.worker();
    w.stellar.with(|n| n.drop_sends = 0);
    w.settle(&worker).await;
    assert_eq!(
        (
            w.get_mandate(&first.mandate_id).await.state(),
            w.get_mandate(&second.mandate_id).await.state()
        ),
        (MandateState::Replaced, MandateState::Active)
    );
    let refused = w.get_recurring(&stale.recurring_charge_id).await;
    assert_eq!(
        (refused.state(), refused.outcome.as_str()),
        (RecurringChargeState::Refused, "no_mandate")
    );
    assert_eq!(w.stellar.with(|n| n.state.allowances[&buyer.key.address()].0), 8);
    assert_eq!(w.wallet(&buyer), 100);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_revocation_ends_the_mandate_and_zeroes_the_allowance(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.active_mandate(&buyer, 10, 3).await;
    let revocation = w.revoke(&buyer, "r-1").await;
    w.settle(&w.worker()).await;
    assert_eq!(
        w.get_revocation(&revocation.revocation_id).await.state(),
        RevocationState::Confirmed
    );
    assert_eq!(w.get_mandate(&mandate.mandate_id).await.state(), MandateState::Revoked);
    assert_eq!(w.stellar.with(|n| n.state.allowances[&buyer.key.address()].0), 0);
    let refused = w.charge_period(&mandate, 10, "c-1").await.unwrap_err();
    assert_refused(&refused, tonic::Code::FailedPrecondition, "mandate_not_active");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_failed_revocation_is_not_confirmed_from_a_read_older_than_the_mandate(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.active_mandate(&buyer, 10, 3).await;
    let since = w.stellar.with(|n| n.state.mandates[&buyer.key.address()].since);
    let revocation = w.revoke(&buyer, "r-1").await;
    // The revocation fails on-chain, and the node read afterwards trails
    // the ledger that recorded the mandate: the mandate looks absent.
    w.stellar.with(|n| n.fail_inclusions = 1);
    let worker = w.worker();
    worker.step().await.unwrap();
    w.stellar.with(|n| n.entries_behind = n.latest - since + 1);
    w.settle(&worker).await;
    assert_eq!(
        (
            w.get_revocation(&revocation.revocation_id).await.state(),
            w.get_mandate(&mandate.mandate_id).await.state()
        ),
        (RevocationState::Submitted, MandateState::Active)
    );
    // A current read shows the mandate still held, as it is: the revocation
    // ends failed once its authorization lapses, the mandate stays active.
    w.stellar.with(|n| {
        n.entries_behind = 0;
        n.latest += common::AUTHORIZATION_VALIDITY_LEDGERS + 1;
    });
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.settle(&worker).await;
    assert_eq!(
        (
            w.get_revocation(&revocation.revocation_id).await.state(),
            w.get_mandate(&mandate.mandate_id).await.state()
        ),
        (RevocationState::Failed, MandateState::Active)
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_an_unreadable_recurring_answer_waits_for_a_read_at_its_inclusion(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.active_mandate(&buyer, 10, 3).await;
    let charge = w.charge_period(&mandate, 10, "c-1").await.unwrap();
    // Included and successful, its answer unreadable, and the record not yet
    // visible on the node read: not evidence of anything yet.
    w.stellar.with(|n| {
        n.truncate_outcomes = true;
        n.entries_behind = 1;
    });
    let worker = w.worker();
    w.settle(&worker).await;
    assert_eq!(
        w.get_recurring(&charge.recurring_charge_id).await.state(),
        RecurringChargeState::Submitted
    );
    w.stellar.with(|n| n.entries_behind = 0);
    w.settle(&worker).await;
    assert_eq!(
        w.get_recurring(&charge.recurring_charge_id).await.state(),
        RecurringChargeState::Charged
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_mandate_revoked_outside_the_gateway_is_closed_by_the_refused_charge(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.active_mandate(&buyer, 10, 3).await;
    // The buyer revoked through another client.
    w.stellar.with(|n| n.state.mandates.remove(&buyer.key.address()));
    let charge = w.charge_period(&mandate, 10, "c-1").await.unwrap();
    w.settle(&w.worker()).await;
    let refused = w.get_recurring(&charge.recurring_charge_id).await;
    assert_eq!(refused.outcome, "no_mandate");
    assert_eq!(w.get_mandate(&mandate.mandate_id).await.state(), MandateState::Revoked);
    assert_eq!(w.wallet(&buyer), 100);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_recurring_batch_applied_elsewhere_is_settled_from_its_record_not_charged_again(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.active_mandate(&buyer, 10, 3).await;
    let charge = w.charge_period(&mandate, 10, "c-1").await.unwrap();
    let worker = w.worker();
    w.stellar.with(|n| n.drop_sends = 2);
    worker.step().await.unwrap();
    w.stellar.include_elsewhere(&w.stellar.sent().last().unwrap().clone());
    // After the operator's authorization lapses the record decides.
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.stellar.with(|n| n.latest += OPERATOR_LEDGERS + 1);
    w.settle(&worker).await;
    let settled = w.get_recurring(&charge.recurring_charge_id).await;
    assert_eq!(settled.state(), RecurringChargeState::Charged);
    assert_eq!((w.wallet(&buyer), w.treasury_usdc()), (90, 10));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_recurring_batch_never_applied_is_sent_again_within_its_last_ledger(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.active_mandate(&buyer, 10, 3).await;
    let charge = w.charge_period(&mandate, 10, "c-1").await.unwrap();
    let worker = w.worker();
    w.stellar.with(|n| n.drop_sends = 2);
    worker.step().await.unwrap();
    w.clock.advance(VALIDITY + Duration::from_secs(1));
    w.stellar.with(|n| n.latest += OPERATOR_LEDGERS + 1);
    w.settle(&worker).await;
    let settled = w.get_recurring(&charge.recurring_charge_id).await;
    assert_eq!(settled.state(), RecurringChargeState::Charged);
    assert_eq!(w.wallet(&buyer), 90, "charged once");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_recurring_charge_not_sent_before_its_last_ledger_expires(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.active_mandate(&buyer, 10, 3).await;
    let charge = w.charge_period(&mandate, 10, "c-1").await.unwrap();
    w.stellar.with(|n| n.latest += common::CHARGE_VALIDITY_LEDGERS + 1);
    w.settle(&w.worker()).await;
    let expired = w.get_recurring(&charge.recurring_charge_id).await;
    assert_eq!(
        (expired.state(), expired.outcome.as_str()),
        (RecurringChargeState::Refused, "expired")
    );
    assert_eq!(w.wallet(&buyer), 100);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_quarantined_recurring_charge_is_resolved_from_the_contract_events_only(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    use fermah_pay_stellar_gateway::quarantine::recurring::{
        prove_recurring, quarantined_recurring, resolve_recurring,
    };
    let w = world(opts, connect).await;
    let buyer = w.buyer("alice", 100).await;
    let mandate = w.active_mandate(&buyer, 10, 3).await;
    let charge = w.charge_period(&mandate, 10, "c-1").await.unwrap();
    // Applied, unreadable, and no record: nothing is guessed.
    w.stellar.with(|n| {
        n.truncate_outcomes = true;
        n.skip_records = true;
    });
    w.settle(&w.worker()).await;
    assert_eq!(
        w.get_recurring(&charge.recurring_charge_id).await.state(),
        RecurringChargeState::Quarantined
    );
    // The period stays taken until it is resolved.
    let taken = w.charge_period(&mandate, 10, "c-2").await.unwrap_err();
    assert_refused(&taken, tonic::Code::AlreadyExists, "period_already_charged");
    // The worker's role cannot take it out of quarantine.
    let error = sqlx::query(
        "UPDATE pay_stellar.recurring_charges SET state = 'charged', outcome = 'charged'
         WHERE state = 'quarantined'",
    )
    .execute(&w.worker_pool)
    .await
    .unwrap_err();
    assert_eq!(error.as_database_error().unwrap().code().unwrap(), "23514");

    let quarantined = quarantined_recurring(&w.operator_pool, None).await.unwrap();
    let q = &quarantined[0];
    // Within its last ledger, nothing is established yet.
    assert!(prove_recurring(&w.stellar, q).await.is_err());
    w.stellar.set_latest(q.last_ledger + CHARGE_RECORD_GRACE + 1);
    let (outcome, evidence) = prove_recurring(&w.stellar, q).await.unwrap();
    assert_eq!(outcome, fermah_pay_stellar_chain::prepaid::RecurringOutcome::Charged);
    assert!(evidence.contains("recurring event"), "{evidence}");
    resolve_recurring(&w.operator_pool, q.id, outcome, &evidence).await.unwrap();
    let resolved = w.get_recurring(&charge.recurring_charge_id).await;
    assert_eq!(
        (resolved.state(), resolved.outcome.as_str()),
        (RecurringChargeState::Charged, "charged")
    );
    assert!(resolve_recurring(&w.operator_pool, q.id, outcome, &evidence).await.is_err());
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_quarantined_charge_resolved_as_refused_for_its_mandate_closes_the_mandate(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    use fermah_pay_stellar_chain::prepaid::RecurringOutcome;
    use fermah_pay_stellar_gateway::quarantine::recurring::{
        quarantined_recurring, resolve_recurring,
    };
    let w = world(opts, connect).await;
    for (name, outcome, closed, refusal) in [
        ("alice", RecurringOutcome::NoMandate, MandateState::Revoked, "mandate_not_active"),
        ("bob", RecurringOutcome::MandateExpired, MandateState::Ended, "mandate_ended"),
    ] {
        let buyer = w.buyer(name, 100).await;
        let mandate = w.active_mandate(&buyer, 10, 3).await;
        w.charge_period(&mandate, 10, &format!("c-{name}")).await.unwrap();
        w.stellar.with(|n| {
            n.truncate_outcomes = true;
            n.skip_records = true;
        });
        w.settle(&w.worker()).await;
        w.stellar.with(|n| {
            n.truncate_outcomes = false;
            n.skip_records = false;
        });
        let quarantined = quarantined_recurring(&w.operator_pool, None).await.unwrap();
        let [q] = quarantined.as_slice() else { panic!("one quarantined charge") };
        resolve_recurring(&w.operator_pool, q.id, outcome, "the contract's answer").await.unwrap();
        // The contract no longer charges this mandate: the gateway stops
        // admitting charges for it too.
        assert_eq!(w.get_mandate(&mandate.mandate_id).await.state(), closed);
        let refused = w.charge_period(&mandate, 10, &format!("c-{name}-2")).await.unwrap_err();
        assert_refused(&refused, tonic::Code::FailedPrecondition, refusal);
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_withdrawals_pay_only_the_buyers_wallet_unless_the_operator_allows_others(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let request = |destination: String, key: &str| PrepareWithdrawalRequest {
        buyer_id: x.id.clone(),
        amount: 10,
        destination,
        idempotency_key: key.to_owned(),
    };
    let other = SecretKey::generate().unwrap().address();
    let refused = w
        .ledger()
        .await
        .prepare_withdrawal(authed(request(other.to_string(), "w-1"), &w.tenant.token))
        .await
        .unwrap_err();
    assert_refused(&refused, tonic::Code::FailedPrecondition, "destination_not_allowed");
    // The wallet itself, named or implied, is always allowed.
    for (destination, key) in [(x.key.address().to_string(), "w-2"), (String::new(), "w-3")] {
        let prepared = w
            .ledger()
            .await
            .prepare_withdrawal(authed(request(destination, key), &w.tenant.token))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(prepared.withdrawal.unwrap().destination, x.key.address().to_string());
    }
}
