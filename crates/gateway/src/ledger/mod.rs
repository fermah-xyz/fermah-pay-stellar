//! `LedgerService` gRPC handlers: deposits, charges, withdrawals and
//! balances.
//!
//! The API process never signs or submits a transaction. It prepares what the
//! buyer signs, verifies what the buyer returns, and admits charges against
//! the database balance; the worker settles both on-chain.

mod mandates;
pub mod store;
mod withdrawals;

/// How a prepared withdrawal's destination is counted: the buyer's own
/// wallet, or another account.
pub const WITHDRAWAL_DESTINATIONS: [&str; 2] = ["own", "other"];

use std::future::Future;
use std::sync::Arc;

use fermah_pay_stellar_chain::authorization::{
    SignedEntryRefusal, signature_payload, verify_contract_entry, verify_signed_entry,
};
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{DepositIntent, PrepaidDeployment};
use fermah_pay_stellar_chain::rpc::{AuthMode, RpcClient, RpcError, SimulationOutcome, hex_lower};
use fermah_pay_stellar_chain::stellar_xdr::{
    InvokeContractArgs, Limits, ReadXdr, ScVal, SorobanAddressCredentials,
    SorobanAuthorizationEntry, SorobanAuthorizedFunction, SorobanCredentials, WriteXdr,
};
use fermah_pay_stellar_chain::transaction::ScAddressOf;
use fermah_pay_stellar_domain::{AccountAddress, ChainAddress, IdempotencyKey, Network};
use fermah_pay_stellar_proto::v1::ledger_service_server::LedgerService;
use fermah_pay_stellar_proto::v1::{
    Charge, ChargeState as WireChargeState, CreateChargeRequest, CreateChargeResponse, Deposit,
    DepositState as WireDepositState, GetBalanceRequest, GetBalanceResponse, GetChargeRequest,
    GetChargeResponse, GetDepositRequest, GetDepositResponse, GetWithdrawalRequest,
    GetWithdrawalResponse, PrepareDepositRequest, PrepareDepositResponse, PrepareWithdrawalRequest,
    PrepareWithdrawalResponse, SubmitDepositRequest, SubmitDepositResponse,
    SubmitWithdrawalRequest, SubmitWithdrawalResponse,
};
use fermah_pay_stellar_proto::v1::{
    CreateRecurringChargeRequest, CreateRecurringChargeResponse, GetMandateRequest,
    GetMandateResponse, GetRecurringChargeRequest, GetRecurringChargeResponse,
    GetRevocationRequest, GetRevocationResponse, PrepareMandateRequest, PrepareMandateResponse,
    PrepareRevocationRequest, PrepareRevocationResponse, SubmitMandateRequest,
    SubmitMandateResponse, SubmitRevocationRequest, SubmitRevocationResponse,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use self::store::{
    Admission, ChargeRecord, ChargeState, DepositRecord, DepositState, Insertion, NewDeposit,
};
use crate::auth::scope_of;
use crate::refusal::Refusal;
use crate::scope::Scope;
use crate::store::{Store, StoreError};

/// The network reads the API needs: the current ledger, which bounds how
/// long a buyer's signature stays valid, its close time, the ledger time by
/// which the contract decides a mandate's period, and whether the network
/// accepts a contract account's authorization.
pub trait LatestLedger: Send + Sync + 'static {
    fn latest_ledger(&self) -> impl Future<Output = Result<u32, RpcError>> + Send;

    /// Unix close time of the latest ledger.
    fn latest_close_time(&self) -> impl Future<Output = Result<i64, RpcError>> + Send;

    /// Simulates `call` from `source` with the signed entries in `auth`, in
    /// enforcing mode: each entry must satisfy its address, a contract
    /// account through its `__check_auth`. Nothing is sent.
    fn simulate_buyer_call(
        &self,
        source: &AccountAddress,
        call: InvokeContractArgs,
        auth: Vec<SorobanAuthorizationEntry>,
    ) -> impl Future<Output = Result<BuyerCall, RpcError>> + Send;
}

/// What the network answered to a simulated buyer call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuyerCall {
    /// It would succeed, at this resource fee in stroops.
    Accepted { resource_fee: i64 },
    /// It would fail, for the network's reason.
    Refused(String),
    /// Archived state must be restored before the call can run at all.
    RestoreRequired,
}

impl BuyerCall {
    /// Whether the network refused the call because an authorization did not
    /// hold, rather than for the call's own reasons (a balance, a pause).
    #[must_use]
    pub fn refused_authorization(&self) -> bool {
        matches!(self, Self::Refused(reason) if reason.contains("Error(Auth,"))
    }
}

impl LatestLedger for RpcClient {
    async fn latest_ledger(&self) -> Result<u32, RpcError> {
        self.get_latest_ledger().await
    }

    async fn latest_close_time(&self) -> Result<i64, RpcError> {
        Ok(self.get_latest_ledger_info().await?.close_time)
    }

    async fn simulate_buyer_call(
        &self,
        source: &AccountAddress,
        call: InvokeContractArgs,
        auth: Vec<SorobanAuthorizationEntry>,
    ) -> Result<BuyerCall, RpcError> {
        Ok(match self.simulate_call(source, call, auth, AuthMode::Enforce).await? {
            SimulationOutcome::Succeeded(simulation) => {
                BuyerCall::Accepted { resource_fee: simulation.min_resource_fee }
            }
            SimulationOutcome::Failed { error, .. } => BuyerCall::Refused(error),
            SimulationOutcome::RestoreRequired { .. } => BuyerCall::RestoreRequired,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LedgerPolicy {
    /// Ledgers a buyer's deposit or withdrawal authorization stays valid
    /// after it is prepared. It covers the time a person takes to approve in
    /// a wallet; the worker can resubmit the same signed entry until it
    /// lapses. A withdrawal's amount stays held until then if it cannot be
    /// sent.
    pub authorization_validity_ledgers: u32,
    /// Ledgers after admission during which a charge may still be settled.
    /// Past them the contract refuses it and its amount is returned; the
    /// contract accepts at most about a day, and the record of each charge
    /// costs rent for as long as this.
    pub charge_validity_ledgers: u32,
    /// Shortest period a mandate may have, in seconds.
    pub min_mandate_period_secs: u64,
    /// Furthest ahead, in ledgers, a mandate's last ledger may be. The
    /// network refuses an allowance beyond its longest entry lifetime, about
    /// six months on testnet and mainnet.
    pub max_mandate_ledgers: u32,
    /// Whether a withdrawal may pay an account other than the buyer's
    /// wallet. Off, a stolen buyer key can only return the buyer's own
    /// credit to the buyer's own wallet.
    pub withdrawals_to_other_accounts: bool,
    /// Largest resource fee, in stroops, a contract-account buyer's deposit
    /// or withdrawal may simulate at; the worker refuses to send one above.
    pub max_buyer_resource_fee: i64,
}

pub struct LedgerApi<L> {
    store: Store,
    ledger: Arc<L>,
    network: Network,
    policy: LedgerPolicy,
}

impl<L> Clone for LedgerApi<L> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            ledger: Arc::clone(&self.ledger),
            network: self.network,
            policy: self.policy,
        }
    }
}

impl<L> LedgerApi<L> {
    pub fn new(store: Store, ledger: L, network: Network, policy: LedgerPolicy) -> Self {
        Self { store, ledger: Arc::new(ledger), network, policy }
    }
}

fn internal(error: &StoreError) -> Status {
    tracing::error!(error = %error, source = ?std::error::Error::source(error), "store failure");
    Refusal::Internal.into()
}

fn corrupt(what: &'static str) -> Status {
    tracing::error!(what, "stored ledger row does not decode");
    Refusal::Internal.into()
}

fn network_unavailable(error: &RpcError) -> Status {
    tracing::warn!(error = %error, "reading the latest ledger");
    Refusal::NetworkUnavailable.into()
}

fn parse_id(raw: &str, refusal: Refusal) -> Result<Uuid, Refusal> {
    Uuid::parse_str(raw).map_err(|_| refusal)
}

fn parse_amount(amount: i64) -> Result<i64, Refusal> {
    if amount > 0 { Ok(amount) } else { Err(Refusal::InvalidAmount) }
}

fn parse_key(raw: &str) -> Result<IdempotencyKey, Refusal> {
    raw.parse().map_err(|_| Refusal::InvalidIdempotencyKey)
}

fn timestamp(at: OffsetDateTime) -> Result<String, Status> {
    at.format(&Rfc3339).map_err(|error| {
        tracing::error!(error = %error, "formatting timestamp");
        Status::from(Refusal::Internal)
    })
}

fn wire_ledger(ledger: Option<i32>) -> u32 {
    ledger.and_then(|l| u32::try_from(l).ok()).unwrap_or(0)
}

fn random<const N: usize>() -> Result<[u8; N], Status> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|error| {
        tracing::error!(error = %error, "operating system randomness unavailable");
        Status::from(Refusal::Internal)
    })?;
    Ok(bytes)
}

fn decode_entry(xdr: &str) -> Option<SorobanAuthorizationEntry> {
    SorobanAuthorizationEntry::from_xdr_base64(xdr, Limits::none()).ok()
}

/// `entry` with its signature cleared: what the buyer agreed to, which must
/// equal the prepared entry field for field.
fn unsigned(entry: &SorobanAuthorizationEntry) -> SorobanAuthorizationEntry {
    let clear = |creds: &SorobanAddressCredentials| SorobanAddressCredentials {
        signature: ScVal::Void,
        ..creds.clone()
    };
    let credentials = match &entry.credentials {
        SorobanCredentials::Address(creds) => SorobanCredentials::Address(clear(creds)),
        SorobanCredentials::AddressV2(creds) => SorobanCredentials::AddressV2(clear(creds)),
        other => other.clone(),
    };
    SorobanAuthorizationEntry { credentials, root_invocation: entry.root_invocation.clone() }
}

/// Why a buyer's returned entry was refused, before the caller names what
/// an expired one means for its kind of request.
enum EntryRefusal {
    Entry(SignedEntryRefusal),
    Network(Refusal),
    Failed(Status),
}

impl EntryRefusal {
    fn expired_as(self, expired: Refusal) -> Status {
        match self {
            Self::Entry(refusal) => signature_refusal(&refusal, expired).into(),
            Self::Network(refusal) => refusal.into(),
            Self::Failed(status) => status,
        }
    }
}

impl<L: LatestLedger> LedgerApi<L> {
    /// Checks the entry a buyer returned for `prepared`. A classic
    /// account's is verified here, signature included. A contract account
    /// authorizes through its own `__check_auth`, with a credential only it
    /// interprets: its entry is checked here for everything else, then the
    /// call is simulated with it in enforcing mode, as the network will run
    /// it when the worker sends it. The simulation is sent from the treasury,
    /// which gives the authorizations `treasury_entries` returns.
    ///
    /// Only what the simulation shows about the buyer refuses the entry: an
    /// authorization that does not hold, or a cost above the bound the worker
    /// applies too. Any other failure (a short balance, archived state) is
    /// the call's own, decided by the worker as for a classic account.
    async fn verify_buyer_entry(
        &self,
        scope: &Scope,
        wallet: &ChainAddress,
        signed: &SorobanAuthorizationEntry,
        prepared: &SorobanAuthorizationEntry,
        treasury_entries: impl FnOnce(&PrepaidDeployment) -> Vec<SorobanAuthorizationEntry>,
    ) -> Result<(), EntryRefusal> {
        let latest = self
            .ledger
            .latest_ledger()
            .await
            .map_err(|e| EntryRefusal::Failed(network_unavailable(&e)))?;
        let validity = self.policy.authorization_validity_ledgers;
        let contract = match wallet {
            ChainAddress::Account(account) => {
                return verify_signed_entry(
                    signed,
                    account,
                    &prepared.root_invocation,
                    network_id(self.network),
                    latest,
                    validity,
                )
                .map_err(EntryRefusal::Entry);
            }
            ChainAddress::Contract(contract) => contract,
        };
        verify_contract_entry(signed, contract, &prepared.root_invocation, latest, validity)
            .map_err(EntryRefusal::Entry)?;
        let SorobanAuthorizedFunction::ContractFn(call) = &prepared.root_invocation.function else {
            return Err(EntryRefusal::Failed(corrupt("prepared entry is not a contract call")));
        };
        let deployment = self
            .store
            .ledger_binding(scope)
            .await
            .map_err(|e| EntryRefusal::Failed(internal(&e)))?
            .ok_or(EntryRefusal::Network(Refusal::LedgerNotConfigured))?;
        let mut auth = vec![signed.clone()];
        auth.extend(treasury_entries(&deployment));
        let answer = self
            .ledger
            .simulate_buyer_call(&deployment.treasury, call.clone(), auth)
            .await
            .map_err(|e| EntryRefusal::Failed(network_unavailable(&e)))?;
        match answer {
            BuyerCall::Accepted { resource_fee }
                if resource_fee > self.policy.max_buyer_resource_fee =>
            {
                tracing::info!(resource_fee, "a contract account's call costs too much");
                Err(EntryRefusal::Network(Refusal::WalletTooCostly))
            }
            ref refused @ BuyerCall::Refused(ref reason) if refused.refused_authorization() => {
                tracing::info!(reason, "the network refused a contract account's authorization");
                Err(EntryRefusal::Network(Refusal::AuthorizationRefused))
            }
            BuyerCall::Refused(reason) => {
                tracing::info!(
                    reason,
                    "a contract account's call fails for now; the worker decides"
                );
                Ok(())
            }
            BuyerCall::Accepted { .. } | BuyerCall::RestoreRequired => Ok(()),
        }
    }
}

const fn signature_refusal(refusal: &SignedEntryRefusal, expired: Refusal) -> Refusal {
    match refusal {
        SignedEntryRefusal::Expired { .. } => expired,
        SignedEntryRefusal::BadSignature | SignedEntryRefusal::UnsupportedSignature => {
            Refusal::InvalidSignature
        }
        SignedEntryRefusal::NotAnAddressEntry
        | SignedEntryRefusal::WrongSigner
        | SignedEntryRefusal::InvocationMismatch
        | SignedEntryRefusal::ValidityTooLong { .. } => Refusal::AuthorizationMismatch,
    }
}

enum Signing {
    /// The deposit awaits a signature.
    Open,
    /// The deposit already holds exactly this signed entry: a retry of a
    /// submission that succeeded, which changes nothing.
    Replay,
    Closed(Refusal),
}

fn signing(record: &DepositRecord, signed: &SorobanAuthorizationEntry) -> Signing {
    let stored = record.signed_authorization_xdr.as_deref().and_then(decode_entry);
    match record.state {
        DepositState::AwaitingSignature => Signing::Open,
        _ if stored.as_ref() == Some(signed) => Signing::Replay,
        DepositState::Expired => Signing::Closed(Refusal::DepositExpired),
        _ => Signing::Closed(Refusal::DepositAlreadySigned),
    }
}

const fn deposit_state(state: DepositState) -> WireDepositState {
    match state {
        DepositState::AwaitingSignature => WireDepositState::AwaitingSignature,
        DepositState::Signed => WireDepositState::Signed,
        DepositState::Submitted => WireDepositState::Submitted,
        DepositState::Confirmed => WireDepositState::Confirmed,
        DepositState::Failed => WireDepositState::Failed,
        DepositState::Expired => WireDepositState::Expired,
    }
}

const fn charge_state(state: ChargeState) -> WireChargeState {
    match state {
        ChargeState::Admitted => WireChargeState::Admitted,
        ChargeState::Submitted => WireChargeState::Submitted,
        ChargeState::Charged => WireChargeState::Charged,
        ChargeState::Refused => WireChargeState::Refused,
        ChargeState::Quarantined => WireChargeState::Quarantined,
    }
}

impl<L> LedgerApi<L> {
    fn deposit_to_wire(&self, record: DepositRecord) -> Result<Deposit, Status> {
        let entry = decode_entry(&record.authorization_xdr)
            .ok_or_else(|| corrupt("authorization entry"))?;
        let payload =
            signature_payload(network_id(self.network), &entry.credentials, &entry.root_invocation)
                .map_err(|_| corrupt("authorization credentials"))?;
        Ok(Deposit {
            deposit_id: record.id.to_string(),
            buyer_id: record.buyer_id.to_string(),
            amount: record.amount,
            state: deposit_state(record.state).into(),
            authorization_entry_xdr: record.authorization_xdr,
            signature_payload: hex_lower(&payload),
            expiration_ledger: u32::try_from(record.expiration_ledger)
                .map_err(|_| corrupt("expiration ledger"))?,
            transaction_hash: record.transaction_hash.as_deref().map(hex_lower).unwrap_or_default(),
            ledger: wire_ledger(record.ledger),
            created_at: timestamp(record.created_at)?,
        })
    }
}

fn charge_to_wire(record: ChargeRecord) -> Result<Charge, Status> {
    Ok(Charge {
        charge_id: record.id.to_string(),
        buyer_id: record.buyer_id.to_string(),
        amount: record.amount,
        contract_charge_id: hex_lower(&record.charge_id),
        last_ledger: u32::try_from(record.last_ledger)
            .map_err(|_| corrupt("charge last ledger"))?,
        state: charge_state(record.state).into(),
        outcome: record.outcome.unwrap_or_default(),
        transaction_hash: record.transaction_hash.as_deref().map(hex_lower).unwrap_or_default(),
        ledger: wire_ledger(record.ledger),
        created_at: timestamp(record.created_at)?,
    })
}

/// The kinds of admitted charge counted by [`record_admission`].
pub const ADMISSION_KINDS: [&str; 2] = ["charge", "recurring"];

/// Counts an admitted charge of `kind` and its amount by deployment, the
/// series against which an unusual charge pattern shows.
fn record_admission(kind: &'static str, scope: &Scope, amount: i64) {
    let deployment = scope.seller_deployment_id().to_string();
    metrics::counter!(
        "pay_stellar_charges_admitted_total",
        "kind" => kind,
        "seller_deployment_id" => deployment.clone()
    )
    .increment(1);
    metrics::counter!(
        "pay_stellar_charges_admitted_usdc_total",
        "kind" => kind,
        "seller_deployment_id" => deployment
    )
    .increment(u64::try_from(amount).unwrap_or(0));
}

fn record_scope(scope: &Scope) {
    tracing::Span::current()
        .record("seller_deployment_id", tracing::field::display(scope.seller_deployment_id()));
}

impl<L: LatestLedger> LedgerApi<L> {
    /// Admits a charge, or answers a retry of one from its stored row
    /// whatever the network's state: only a new charge needs the current
    /// ledger. `true` when this call created the charge.
    pub(crate) async fn admit(
        &self,
        scope: &Scope,
        buyer_id: Uuid,
        amount: i64,
        key: &IdempotencyKey,
    ) -> Result<(ChargeRecord, bool), Refusal> {
        let store_failure = |error: StoreError| {
            tracing::error!(error = %error, source = ?std::error::Error::source(&error), "store failure");
            Refusal::Internal
        };
        let replayed = self
            .store
            .replayed_charge(scope, buyer_id, amount, key)
            .await
            .map_err(store_failure)?;
        let admission = match replayed {
            Some(admission) => admission,
            None => {
                let latest = self.ledger.latest_ledger().await.map_err(|error| {
                    tracing::warn!(error = %error, "reading the latest ledger");
                    Refusal::NetworkUnavailable
                })?;
                let last_ledger = latest
                    .checked_add(self.policy.charge_validity_ledgers)
                    .ok_or(Refusal::Internal)?;
                self.store
                    .admit_charge(scope, buyer_id, amount, key, last_ledger)
                    .await
                    .map_err(store_failure)?
            }
        };
        match admission {
            Admission::Admitted(record) => {
                record_admission("charge", scope, amount);
                Ok((record, true))
            }
            Admission::Replayed(record) => Ok((record, false)),
            Admission::Conflict => Err(Refusal::IdempotencyConflict),
            Admission::BuyerNotFound => Err(Refusal::BuyerNotFound),
            Admission::InsufficientBalance => Err(Refusal::InsufficientBalance),
        }
    }

    pub(crate) const fn store(&self) -> &Store {
        &self.store
    }

    pub(crate) const fn network(&self) -> Network {
        self.network
    }

    /// The existing deposit under `key`, if the request repeats it exactly.
    fn replay_deposit(
        &self,
        existing: DepositRecord,
        buyer_id: Uuid,
        amount: i64,
    ) -> Result<Response<PrepareDepositResponse>, Status> {
        if existing.buyer_id != buyer_id || existing.amount != amount {
            return Err(Refusal::IdempotencyConflict.into());
        }
        Ok(Response::new(PrepareDepositResponse {
            deposit: Some(self.deposit_to_wire(existing)?),
            created: false,
        }))
    }

    async fn existing_deposit(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<DepositRecord>, Status> {
        self.store.deposit(scope, id, key).await.map_err(|e| internal(&e))
    }
}

#[tonic::async_trait]
impl<L: LatestLedger> LedgerService for LedgerApi<L> {
    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn prepare_deposit(
        &self,
        request: Request<PrepareDepositRequest>,
    ) -> Result<Response<PrepareDepositResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let buyer_id = parse_id(&body.buyer_id, Refusal::InvalidBuyerId)?;
        let amount = parse_amount(body.amount)?;
        let key = parse_key(&body.idempotency_key)?;
        if let Some(existing) = self.existing_deposit(&scope, None, Some(&key)).await? {
            return self.replay_deposit(existing, buyer_id, amount);
        }

        let deployment = self
            .store
            .ledger_binding(&scope)
            .await
            .map_err(|e| internal(&e))?
            .ok_or(Refusal::LedgerNotConfigured)?;
        let wallet = self
            .store
            .buyer_wallet(&scope, buyer_id)
            .await
            .map_err(|e| internal(&e))?
            .ok_or(Refusal::BuyerNotFound)?;
        let latest = self.ledger.latest_ledger().await.map_err(|e| network_unavailable(&e))?;
        let expiration_ledger = latest
            .checked_add(self.policy.authorization_validity_ledgers)
            .ok_or(Refusal::Internal)?;
        let intent = DepositIntent {
            owner: wallet.clone(),
            amount: i128::from(amount),
            deposit_id: random()?,
        };
        // `AddressV2` credentials commit the signature to the buyer's address
        // as well as the call, so it cannot authorize another account that
        // shares the key.
        let entry = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
                address: wallet.sc_address(),
                nonce: i64::from_le_bytes(random()?),
                signature_expiration_ledger: expiration_ledger,
                signature: ScVal::Void,
            }),
            root_invocation: deployment.deposit_authorization(&intent),
        };
        let authorization_xdr =
            entry.to_xdr_base64(Limits::none()).map_err(|_| corrupt("new authorization entry"))?;

        let inserted = self
            .store
            .insert_deposit(
                &scope,
                &NewDeposit {
                    buyer_id,
                    key: &key,
                    amount,
                    deposit_id: intent.deposit_id,
                    authorization_xdr: &authorization_xdr,
                    expiration_ledger,
                },
            )
            .await
            .map_err(|e| internal(&e))?;
        let id = match inserted {
            Insertion::Created(id) => id,
            Insertion::QuotaExceeded => return Err(Refusal::DepositQuotaExceeded.into()),
            Insertion::DeploymentQuotaExceeded => {
                return Err(Refusal::DeploymentDepositQuotaExceeded.into());
            }
            // A concurrent request with the same key committed first.
            Insertion::KeyTaken => {
                let existing = self
                    .existing_deposit(&scope, None, Some(&key))
                    .await?
                    .ok_or(Refusal::Internal)?;
                return self.replay_deposit(existing, buyer_id, amount);
            }
        };
        let created =
            self.existing_deposit(&scope, Some(id), None).await?.ok_or(Refusal::Internal)?;
        Ok(Response::new(PrepareDepositResponse {
            deposit: Some(self.deposit_to_wire(created)?),
            created: true,
        }))
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn submit_deposit(
        &self,
        request: Request<SubmitDepositRequest>,
    ) -> Result<Response<SubmitDepositResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let id = parse_id(&body.deposit_id, Refusal::InvalidDepositId)?;
        let signed = decode_entry(&body.signed_authorization_entry_xdr)
            .ok_or(Refusal::InvalidAuthorizationEntry)?;
        let respond = |record: DepositRecord| -> Result<Response<SubmitDepositResponse>, Status> {
            Ok(Response::new(SubmitDepositResponse {
                deposit: Some(self.deposit_to_wire(record)?),
            }))
        };

        let record =
            self.existing_deposit(&scope, Some(id), None).await?.ok_or(Refusal::DepositNotFound)?;
        match signing(&record, &signed) {
            Signing::Open => {}
            Signing::Replay => return respond(record),
            Signing::Closed(refusal) => return Err(refusal.into()),
        }
        let prepared = decode_entry(&record.authorization_xdr)
            .ok_or_else(|| corrupt("authorization entry"))?;
        if unsigned(&signed) != prepared {
            return Err(Refusal::AuthorizationMismatch.into());
        }
        // The deposit's transfer moves the buyer's USDC to the treasury: the
        // buyer's entry is the only authorization the call needs.
        self.verify_buyer_entry(&scope, &record.wallet, &signed, &prepared, |_| vec![])
            .await
            .map_err(|refusal| refusal.expired_as(Refusal::DepositExpired))?;

        let signed_xdr = signed
            .to_xdr_base64(Limits::none())
            .map_err(|_| corrupt("signed authorization entry"))?;
        self.store.sign_deposit(&scope, id, &signed_xdr).await.map_err(|e| internal(&e))?;
        // Whether this request or a concurrent one stored the signature, the
        // stored row now decides the answer.
        let record =
            self.existing_deposit(&scope, Some(id), None).await?.ok_or(Refusal::Internal)?;
        match signing(&record, &signed) {
            Signing::Replay => respond(record),
            Signing::Closed(refusal) => Err(refusal.into()),
            Signing::Open => Err(corrupt("deposit still unsigned after signing")),
        }
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn get_deposit(
        &self,
        request: Request<GetDepositRequest>,
    ) -> Result<Response<GetDepositResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let id = parse_id(&request.into_inner().deposit_id, Refusal::InvalidDepositId)?;
        let record =
            self.existing_deposit(&scope, Some(id), None).await?.ok_or(Refusal::DepositNotFound)?;
        Ok(Response::new(GetDepositResponse { deposit: Some(self.deposit_to_wire(record)?) }))
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn create_charge(
        &self,
        request: Request<CreateChargeRequest>,
    ) -> Result<Response<CreateChargeResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let buyer_id = parse_id(&body.buyer_id, Refusal::InvalidBuyerId)?;
        let amount = parse_amount(body.amount)?;
        let key = parse_key(&body.idempotency_key)?;
        let (record, created) = self.admit(&scope, buyer_id, amount, &key).await?;
        Ok(Response::new(CreateChargeResponse { charge: Some(charge_to_wire(record)?), created }))
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn get_charge(
        &self,
        request: Request<GetChargeRequest>,
    ) -> Result<Response<GetChargeResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let id = parse_id(&request.into_inner().charge_id, Refusal::InvalidChargeId)?;
        let record = self
            .store
            .charge(&scope, id)
            .await
            .map_err(|e| internal(&e))?
            .ok_or(Refusal::ChargeNotFound)?;
        Ok(Response::new(GetChargeResponse { charge: Some(charge_to_wire(record)?) }))
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn get_balance(
        &self,
        request: Request<GetBalanceRequest>,
    ) -> Result<Response<GetBalanceResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let buyer_id = parse_id(&request.into_inner().buyer_id, Refusal::InvalidBuyerId)?;
        let balance = self
            .store
            .balance(&scope, buyer_id)
            .await
            .map_err(|e| internal(&e))?
            .ok_or(Refusal::BuyerNotFound)?;
        Ok(Response::new(GetBalanceResponse {
            available: balance.available,
            pending_charges: balance.pending_charges,
            pending_withdrawals: balance.pending_withdrawals,
        }))
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn prepare_withdrawal(
        &self,
        request: Request<PrepareWithdrawalRequest>,
    ) -> Result<Response<PrepareWithdrawalResponse>, Status> {
        self.prepare_withdrawal_request(request).await
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn submit_withdrawal(
        &self,
        request: Request<SubmitWithdrawalRequest>,
    ) -> Result<Response<SubmitWithdrawalResponse>, Status> {
        self.submit_withdrawal_request(request).await
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn get_withdrawal(
        &self,
        request: Request<GetWithdrawalRequest>,
    ) -> Result<Response<GetWithdrawalResponse>, Status> {
        self.get_withdrawal_request(request).await
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn prepare_mandate(
        &self,
        request: Request<PrepareMandateRequest>,
    ) -> Result<Response<PrepareMandateResponse>, Status> {
        self.prepare_mandate_request(request).await
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn submit_mandate(
        &self,
        request: Request<SubmitMandateRequest>,
    ) -> Result<Response<SubmitMandateResponse>, Status> {
        self.submit_mandate_request(request).await
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn get_mandate(
        &self,
        request: Request<GetMandateRequest>,
    ) -> Result<Response<GetMandateResponse>, Status> {
        self.get_mandate_request(request).await
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn prepare_revocation(
        &self,
        request: Request<PrepareRevocationRequest>,
    ) -> Result<Response<PrepareRevocationResponse>, Status> {
        self.prepare_revocation_request(request).await
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn submit_revocation(
        &self,
        request: Request<SubmitRevocationRequest>,
    ) -> Result<Response<SubmitRevocationResponse>, Status> {
        self.submit_revocation_request(request).await
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn get_revocation(
        &self,
        request: Request<GetRevocationRequest>,
    ) -> Result<Response<GetRevocationResponse>, Status> {
        self.get_revocation_request(request).await
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn create_recurring_charge(
        &self,
        request: Request<CreateRecurringChargeRequest>,
    ) -> Result<Response<CreateRecurringChargeResponse>, Status> {
        self.create_recurring_charge_request(request).await
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn get_recurring_charge(
        &self,
        request: Request<GetRecurringChargeRequest>,
    ) -> Result<Response<GetRecurringChargeResponse>, Status> {
        self.get_recurring_charge_request(request).await
    }
}
