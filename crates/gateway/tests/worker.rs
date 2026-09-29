//! Settlement worker against real PostgreSQL, with the API and the worker
//! under their production roles, and a scripted network that applies the
//! prepaid contract's rules: per-account charge sequences, per-owner deposit
//! markers, single-use authorization nonces and expiring signatures. The
//! script can lose a send, include a transaction as failed, or include a
//! broadcast authorization through someone else's transaction.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Harness, Ledger, Tenant, authed, pool_as, start_with};
use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{Outcome, PrepaidDeployment};
use fermah_pay_stellar_chain::rpc::{
    IncludedTransaction, LedgerEntries, LedgerEntryRecord, NodeView, RpcError, SendOutcome,
    Simulation, SimulationOutcome, TransactionStatus,
};
use fermah_pay_stellar_chain::soroban::fee_bump_hash;
use fermah_pay_stellar_chain::stellar_xdr::{
    ContractDataDurability, ContractDataEntry, ContractEvent, ContractEventBody, ContractEventType,
    ContractEventV0, ContractId, ExtensionPoint, FeeBumpTransactionInnerTx, Hash, HostFunction,
    Int128Parts, InvokeContractArgs, LedgerEntryChanges, LedgerEntryData, LedgerEntryExt,
    LedgerFootprint, LedgerKey, LedgerKeyContractData, Limits, OperationBody, OperationMetaV2,
    ReadXdr, ScAddress, ScMap, ScMapEntry, ScSymbol, ScVal, ScVec, SorobanAuthorizationEntry,
    SorobanCredentials, SorobanResources, SorobanTransactionData, SorobanTransactionDataExt,
    SorobanTransactionMetaExt, SorobanTransactionMetaV2, Transaction, TransactionEnvelope,
    TransactionExt, TransactionMeta, TransactionMetaV4, TransactionResult, TransactionResultExt,
    TransactionResultResult, VecM, WriteXdr,
};
use fermah_pay_stellar_chain::transaction::address_of;
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_gateway::ledger::LatestLedger;
use fermah_pay_stellar_gateway::quarantine::{
    self, QuarantineError, QuarantinedCharge, Resolution,
};
use fermah_pay_stellar_gateway::submission::{Chain, Clock, Engine, Keys, Policy, SourceSequence};
use fermah_pay_stellar_gateway::worker::{Settings, Step, Worker};
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    Charge, ChargeState, CreateBuyerRequest, CreateChargeRequest, Deposit, DepositState,
    GetBalanceRequest, GetChargeRequest, GetDepositRequest, PrepareDepositRequest,
    SubmitDepositRequest,
};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use time::OffsetDateTime;
use tokio::sync::Barrier;

const CONTRACT: [u8; 32] = [7; 32];
const MAX_CHARGE: i128 = 50;
const START_LEDGER: u32 = 1_000;
const OPERATOR_LEDGERS: u32 = 12;
const VALIDITY: Duration = Duration::from_secs(60);
const RETRY_AFTER: Duration = Duration::from_secs(30);

// ---- the network --------------------------------------------------------

#[derive(Clone, Default)]
struct ContractState {
    /// owner -> (balance, last consumed charge sequence)
    accounts: HashMap<AccountAddress, (i128, u64)>,
    deposits: HashSet<(AccountAddress, [u8; 32])>,
    /// USDC held by each wallet, which a deposit moves into the treasury.
    usdc: HashMap<AccountAddress, i128>,
    used_nonces: HashSet<(AccountAddress, i64)>,
}

struct Net {
    latest: u32,
    source_sequence: i64,
    state: ContractState,
    included: HashMap<[u8; 32], TransactionStatus>,
    sent: Vec<TransactionEnvelope>,
    /// The next sends are accepted but never included.
    drop_sends: usize,
    /// The next included transactions fail without effect.
    fail_inclusions: usize,
    /// `charge_batch` returns one outcome fewer than it settled.
    truncate_outcomes: bool,
    simulation_barrier: Option<Arc<Barrier>>,
    /// Buyer accounts whose contract entry is archived: any call touching one
    /// needs a restore first.
    archived: HashSet<AccountAddress>,
    /// Ledger-entry reads trail the latest ledger by this many ledgers.
    entries_behind: u32,
    clock: ManualClock,
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
    /// Archived buyer accounts the call would read or write.
    fn touched_archived(&self, call: &InvokeContractArgs) -> Vec<AccountAddress> {
        let args = call.args.as_slice();
        let owners: Vec<AccountAddress> = match call.function_name.0.as_slice() {
            b"deposit" => vec![owner_of(&args[0])],
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
    ) -> Result<(ContractState, ScVal, Option<ScVal>), String> {
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
                state.accounts.entry(owner).or_insert((0, 0)).0 += amount;
                Ok((state, ScVal::Void, None))
            }
            b"charge_batch" => {
                if !authorized.contains(operator) {
                    return Err("operator did not authorize".to_owned());
                }
                let ScVal::Vec(Some(ScVec(charges))) = &args[0] else { panic!("charges") };
                let mut outcomes = Vec::new();
                let mut settled = Vec::new();
                for charge in charges.iter() {
                    let ScVal::Vec(Some(ScVec(fields))) = charge else { panic!("charge") };
                    let owner = owner_of(&fields[0]);
                    let ScVal::U64(seq) = fields[1] else { panic!("seq") };
                    let amount = i128_of(&fields[2]);
                    let code = match state.accounts.get_mut(&owner) {
                        None => 5,
                        Some((_, consumed)) if seq <= *consumed => 3,
                        Some((_, consumed)) if seq != *consumed + 1 => 4,
                        Some((balance, consumed)) => {
                            *consumed = seq;
                            if amount > MAX_CHARGE {
                                2
                            } else if amount > *balance {
                                1
                            } else {
                                *balance -= amount;
                                0
                            }
                        }
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
                Ok((state, ScVal::Vec(Some(ScVec(outcomes.try_into().unwrap()))), Some(event)))
            }
            other => panic!("unexpected call {}", String::from_utf8_lossy(other)),
        }
    }
}

fn outer_hash(envelope: &TransactionEnvelope) -> [u8; 32] {
    let TransactionEnvelope::TxFeeBump(bump) = envelope else { panic!("not a fee bump") };
    fee_bump_hash(&bump.tx, Network::Testnet).unwrap()
}

/// The operation meta holding the contract's `charges` event.
fn charges_event(data: ScVal) -> OperationMetaV2 {
    OperationMetaV2 {
        ext: ExtensionPoint::V0,
        changes: LedgerEntryChanges(VecM::default()),
        events: vec![ContractEvent {
            ext: ExtensionPoint::V0,
            contract_id: Some(ContractId(Hash(CONTRACT))),
            type_: ContractEventType::Contract,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: vec![symbol("charges")].try_into().unwrap(),
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

    fn account(&self, owner: &AccountAddress) -> Option<(i128, u64)> {
        self.with(|n| n.state.accounts.get(owner).copied())
    }

    fn sent(&self) -> Vec<TransactionEnvelope> {
        self.with(|n| n.sent.clone())
    }

    /// Someone else includes the call and authorizations of `envelope` in a
    /// transaction of their own: the contract state changes, this gateway's
    /// source sequence does not.
    fn include_elsewhere(&self, envelope: &TransactionEnvelope) {
        let (call, auth, _) = invocation(envelope);
        let operator = self.operator.clone();
        self.with(|n| {
            let (state, _, _) = n.execute(&call, &auth, &operator).unwrap();
            n.state = state;
        });
    }
}

impl Chain for Stellar {
    async fn account_sequence(&self, _: &AccountAddress) -> Result<SourceSequence, RpcError> {
        Ok(self.with(|n| SourceSequence {
            sequence: Some(n.source_sequence),
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
                min_resource_fee: 1_000,
                auth: vec![],
                result: None,
                latest_ledger,
            })),
        })
    }

    async fn send(&self, envelope: &TransactionEnvelope) -> Result<SendOutcome, RpcError> {
        let hash = outer_hash(envelope);
        let sequence = inner(envelope).seq_num.0;
        let operator = self.operator.clone();
        let deployment = self.deployment.clone();
        self.with(|n| {
            n.sent.push(envelope.clone());
            if n.included.contains_key(&hash) {
                return Ok(SendOutcome::Duplicate { hash });
            }
            if sequence != n.source_sequence + 1 {
                return Ok(SendOutcome::Rejected {
                    hash,
                    result: Box::new(TransactionResult {
                        fee_charged: 0,
                        result: TransactionResultResult::TxBadSeq,
                        ext: TransactionResultExt::V0,
                    }),
                });
            }
            if n.drop_sends > 0 {
                n.drop_sends -= 1;
                return Ok(SendOutcome::Pending { hash });
            }
            n.source_sequence = sequence;
            let status = if n.fail_inclusions > 0 {
                n.fail_inclusions -= 1;
                TransactionStatus::Failed(included(envelope, n.latest, false, None))
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
                            let mut tx = included(envelope, n.latest, true, Some(value));
                            if let (Some(data), Some(TransactionMeta::V4(meta))) =
                                (event, tx.meta.as_mut())
                            {
                                meta.operations = vec![charges_event(data)].try_into().unwrap();
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
            let LedgerKey::ContractData(LedgerKeyContractData { contract, key: data_key, .. }) =
                key
            else {
                unreachable!()
            };
            LedgerEntryRecord {
                key: key.clone(),
                data: LedgerEntryData::ContractData(ContractDataEntry {
                    ext: ExtensionPoint::V0,
                    contract: contract.clone(),
                    key: data_key.clone(),
                    durability: ContractDataDurability::Persistent,
                    val,
                }),
                ext: LedgerEntryExt::V0,
                last_modified_ledger: 1,
            }
        };
        let state = self.with(|n| n.state.clone());
        let mut existing: HashMap<LedgerKey, ScVal> = HashMap::new();
        for (owner, (balance, seq)) in &state.accounts {
            let value = ScVal::Map(Some(ScMap(
                vec![
                    ScMapEntry {
                        key: symbol("balance"),
                        val: ScVal::I128(Int128Parts {
                            hi: 0,
                            lo: u64::try_from(*balance).unwrap(),
                        }),
                    },
                    ScMapEntry { key: symbol("charge_seq"), val: ScVal::U64(*seq) },
                ]
                .try_into()
                .unwrap(),
            )));
            existing.insert(self.deployment.account_key(owner), value);
        }
        for (owner, id) in &state.deposits {
            existing.insert(self.deployment.deposit_key(owner, id), ScVal::Void);
        }
        Ok(LedgerEntries {
            entries: keys
                .iter()
                .filter_map(|key| existing.get(key).map(|val| contract_data(key, val.clone())))
                .collect(),
            latest_ledger: self.with(|n| n.latest - n.entries_behind),
        })
    }
}

impl LatestLedger for Stellar {
    async fn latest_ledger(&self) -> Result<u32, RpcError> {
        Ok(self.latest())
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

fn treasury() -> AccountAddress {
    AccountAddress::from_public_key([11; 32])
}

async fn world(opts: PgPoolOptions, connect: PgConnectOptions) -> World {
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
            state: ContractState::default(),
            included: HashMap::new(),
            sent: Vec::new(),
            drop_sends: 0,
            fail_inclusions: 0,
            truncate_outcomes: false,
            simulation_barrier: None,
            archived: HashSet::new(),
            entries_behind: 0,
            clock: clock.clone(),
        })),
        deployment: PrepaidDeployment { contract: CONTRACT, usdc, treasury: treasury() },
        operator: operator.address(),
    };
    let h = start_with(opts, connect.clone(), Network::Testnet, stellar.clone(), Ledger::default())
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
        let engine = Engine::new(
            self.worker_pool.clone(),
            self.stellar.clone(),
            self.clock.clone(),
            Network::Testnet,
            Keys {
                source: SecretKey::from_strkey(source_seed).unwrap(),
                fee_source: SecretKey::from_strkey(&self.fee_seed).unwrap(),
            },
            Policy { inclusion_fee: 100, resource_fee_margin_percent: 10, validity: VALIDITY },
        );
        Worker::new(
            engine,
            self.worker_pool.clone(),
            SecretKey::from_strkey(&self.operator_seed).unwrap(),
            Settings {
                operator_authorization_ledgers: OPERATOR_LEDGERS,
                retry_after: RETRY_AFTER,
                max_batch,
            },
        )
    }

    fn worker(&self) -> Worker<Stellar, ManualClock> {
        self.worker_with(&self.source_seed, 100)
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
    assert_eq!(w.stellar.account(&buyer.key.address()), Some((100, 0)));
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
    assert_eq!(w.stellar.account(&buyer.key.address()), Some((100, 0)));
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
    w.stellar.with(|n| n.state.accounts.get_mut(&z.key.address()).unwrap().0 = 10);
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
    assert_eq!(w.stellar.account(&x.key.address()), Some((70, 1)));
    assert_eq!(w.stellar.account(&y.key.address()), Some((100, 1)));
    assert_eq!(w.stellar.account(&z.key.address()), Some((10, 1)));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_contradicting_answer_quarantines_and_blocks_only_that_buyer(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let y = w.funded("y", 100).await;
    // X's sequence 1 is already consumed on-chain.
    w.stellar.with(|n| n.state.accounts.get_mut(&x.key.address()).unwrap().1 = 1);
    let duplicate = w.charge(&x, 10, "c-x1").await;
    let fine = w.charge(&y, 10, "c-y1").await;
    let worker = w.worker();
    w.settle(&worker).await;
    let quarantined = w.get_charge(&duplicate.charge_id).await;
    assert_eq!(
        (quarantined.state(), quarantined.outcome.as_str()),
        (ChargeState::Quarantined, "duplicate")
    );
    assert_eq!(w.get_charge(&fine.charge_id).await.state(), ChargeState::Charged);
    // The amount stays debited while an operator reviews.
    assert_eq!(w.balance(&x).await, (90, 0));

    let blocked = w.charge(&x, 10, "c-x2").await;
    let next = w.charge(&y, 10, "c-y2").await;
    w.settle(&worker).await;
    assert_eq!(w.get_charge(&blocked.charge_id).await.state(), ChargeState::Admitted);
    assert_eq!(w.get_charge(&next.charge_id).await.state(), ChargeState::Charged);
    assert_eq!(w.stellar.account(&x.key.address()), Some((100, 1)));
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
    assert_eq!(settled.sequence, 1);
    assert_eq!(w.balance(&x).await, (70, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some((70, 1)));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_batch_applied_elsewhere_is_quarantined_not_charged_again(
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
    assert_eq!((settled.state(), settled.outcome.as_str()), (ChargeState::Quarantined, ""));
    let error = w.last_error("charges", &charge.charge_id).await.unwrap();
    assert!(error.contains("consumed sequence 1"), "{error}");
    assert_eq!(w.balance(&x).await, (70, 0));
    assert_eq!(w.stellar.account(&x.key.address()), Some((70, 1)));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_batch_answer_of_the_wrong_length_quarantines_every_charge(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let first = w.charge(&x, 10, "c-1").await;
    let second = w.charge(&x, 10, "c-2").await;
    w.stellar.with(|n| n.truncate_outcomes = true);
    w.settle(&w.worker()).await;
    for charge in [first, second] {
        assert_eq!(w.get_charge(&charge.charge_id).await.state(), ChargeState::Quarantined);
    }
    assert_eq!(w.balance(&x).await, (80, 0));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_batch_of_unknown_fate_is_decided_from_the_account_sequence(
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
    // found: the engine quarantines the submission. The contract still has
    // the charge's sequence free, so the charge is settled again, once.
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
    assert_eq!(w.stellar.account(&x.key.address()), Some((70, 1)));
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
    assert_eq!(w.stellar.account(&idle.key.address()), Some((90, 1)));
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
    assert_eq!(w.stellar.account(&x.key.address()), Some((70, 1)));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_small_batches_keep_each_buyers_sequences_in_order(
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
    assert_eq!(w.stellar.account(&x.key.address()), Some((70, 3)));
    assert_eq!(w.stellar.account(&y.key.address()), Some((90, 1)));
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
    assert_eq!(w.stellar.account(&x.key.address()), Some((70, 1)));
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
    assert_eq!(w.stellar.account(&buyer.key.address()), Some((40, 3)));
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
    // The return value is unusable, so both are quarantined, still debited.
    w.stellar.with(|n| n.truncate_outcomes = true);
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
    w.stellar.with(|n| n.truncate_outcomes = false);
    let next = w.charge(&x, 5, "c-3").await;
    w.settle(&w.worker()).await;
    let next = w.get_charge(&next.charge_id).await;
    assert_eq!((next.state(), next.sequence), (ChargeState::Charged, 3));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_evidence_that_does_not_settle_the_charge_is_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let y = w.funded("y", 100).await;
    // X's sequence 1 is already consumed: the contract answers duplicate.
    w.stellar.with(|n| n.state.accounts.get_mut(&x.key.address()).unwrap().1 = 1);
    let duplicate = w.charge(&x, 10, "c-x").await;
    let other = w.charge(&y, 10, "c-y").await;
    w.settle(&w.worker()).await;
    let q = quarantined(&w, &duplicate.charge_id).await;
    let batch = hash_of(&w.get_charge(&duplicate.charge_id).await.transaction_hash);

    // The batch answered `duplicate` for X: it consumed nothing, so it proves
    // no outcome; and X's sequence is consumed, so readmission is refused.
    let error = quarantine::prove_from_transaction(&w.stellar, &q, &batch).await.unwrap_err();
    assert!(
        matches!(error, QuarantineError::NotConsumed { outcome: "duplicate", .. }),
        "{error:?}"
    );
    let error = quarantine::prove_readmission(&w.stellar, &q).await.unwrap_err();
    assert!(matches!(error, QuarantineError::SequenceConsumed { consumed: 1, .. }), "{error:?}");
    // An entry for the right account and sequence but another amount proves
    // nothing either: Y's charge settled for 10, not 11.
    assert_eq!(w.get_charge(&other.charge_id).await.state(), ChargeState::Charged);
    let wrong_amount = QuarantinedCharge { owner: y.key.address(), amount: 11, ..q.clone() };
    let error =
        quarantine::prove_from_transaction(&w.stellar, &wrong_amount, &batch).await.unwrap_err();
    assert!(matches!(error, QuarantineError::AmountMismatch { .. }), "{error:?}");

    // Once the sequence is free again (here, set by the test), readmission
    // is proven and the charge settles in the next batch.
    w.stellar.with(|n| n.state.accounts.get_mut(&x.key.address()).unwrap().1 = 0);
    let (resolution, evidence) = quarantine::prove_readmission(&w.stellar, &q).await.unwrap();
    quarantine::resolve(&w.operator_pool, q.id, resolution, &evidence).await.unwrap();
    assert_eq!(w.get_charge(&duplicate.charge_id).await.state(), ChargeState::Admitted);
    w.settle(&w.worker()).await;
    assert_eq!(w.get_charge(&duplicate.charge_id).await.state(), ChargeState::Charged);
    assert_eq!(w.stellar.account(&x.key.address()), Some((90, 1)));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_only_the_resolution_function_leaves_quarantine(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let x = w.funded("x", 100).await;
    let charge = w.charge(&x, 10, "c-1").await;
    w.stellar.with(|n| n.truncate_outcomes = true);
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
