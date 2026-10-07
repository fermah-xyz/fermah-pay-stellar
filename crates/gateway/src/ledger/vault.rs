//! A vault buyer's limit changes and exit requests: prepared here, signed by
//! the buyer's wallet, submitted by the worker. What can only make
//! admission stricter applies as soon as the signed entry is stored; the
//! contract's own events then confirm or correct it.

use fermah_pay_stellar_chain::authorization::signature_payload;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::stellar_xdr::{
    Limits, ScVal, SorobanAddressCredentials, SorobanAuthorizationEntry, SorobanCredentials,
    WriteXdr,
};
use fermah_pay_stellar_chain::transaction::ScAddressOf;
use fermah_pay_stellar_domain::IdempotencyKey;
use fermah_pay_stellar_proto::v1::{
    Exit, GetExitRequest, GetExitResponse, GetLimitChangeRequest, GetLimitChangeResponse,
    LimitChange, PrepareExitRequest, PrepareExitResponse, PrepareLimitChangeRequest,
    PrepareLimitChangeResponse, SubmitExitRequest, SubmitExitResponse, SubmitLimitChangeRequest,
    SubmitLimitChangeResponse, VaultRequestState as WireState,
};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::store::{
    Insertion, NewVaultRequest, VaultRequestKind, VaultRequestRecord, VaultRequestState,
};
use super::withdrawals::parse_destination;
use super::{
    LatestLedger, LedgerApi, corrupt, decode_entry, internal, network_unavailable, parse_amount,
    parse_id, parse_key, random, record_scope, timestamp, unsigned, wire_ledger,
};
use crate::auth::scope_of;
use crate::refusal::Refusal;
use crate::scope::Scope;

const fn wire_state(state: VaultRequestState) -> WireState {
    match state {
        VaultRequestState::AwaitingSignature => WireState::AwaitingSignature,
        VaultRequestState::Signed => WireState::Signed,
        VaultRequestState::Submitted => WireState::Submitted,
        VaultRequestState::Confirmed => WireState::Confirmed,
        VaultRequestState::Failed => WireState::Failed,
        VaultRequestState::Expired => WireState::Expired,
    }
}

/// The refusals one kind of request answers with.
struct Refusals {
    not_found: Refusal,
    expired: Refusal,
    already_signed: Refusal,
}

const LIMIT_CHANGE: Refusals = Refusals {
    not_found: Refusal::LimitChangeNotFound,
    expired: Refusal::LimitChangeExpired,
    already_signed: Refusal::LimitChangeAlreadySigned,
};

const EXIT: Refusals = Refusals {
    not_found: Refusal::ExitNotFound,
    expired: Refusal::ExitExpired,
    already_signed: Refusal::ExitAlreadySigned,
};

enum Signing {
    Open,
    Replay,
    Closed(Refusal),
}

fn signing(
    record: &VaultRequestRecord,
    signed: &SorobanAuthorizationEntry,
    refusals: &Refusals,
) -> Signing {
    let stored = record.signed_authorization_xdr.as_deref().and_then(decode_entry);
    if record.state == VaultRequestState::AwaitingSignature {
        Signing::Open
    } else if stored.as_ref() == Some(signed) {
        Signing::Replay
    } else if record.state == VaultRequestState::Expired {
        Signing::Closed(refusals.expired)
    } else {
        Signing::Closed(refusals.already_signed)
    }
}

impl<L> LedgerApi<L> {
    fn payload(&self, authorization_xdr: &str) -> Result<String, Status> {
        let entry =
            decode_entry(authorization_xdr).ok_or_else(|| corrupt("authorization entry"))?;
        let payload =
            signature_payload(network_id(self.network), &entry.credentials, &entry.root_invocation)
                .map_err(|_| corrupt("authorization credentials"))?;
        Ok(fermah_pay_stellar_chain::rpc::hex_lower(&payload))
    }

    fn limit_change_to_wire(&self, record: VaultRequestRecord) -> Result<LimitChange, Status> {
        let VaultRequestKind::SetCap { cap } = record.kind else {
            return Err(Refusal::LimitChangeNotFound.into());
        };
        Ok(LimitChange {
            limit_change_id: record.id.to_string(),
            buyer_id: record.buyer_id.to_string(),
            daily_limit: cap,
            state: wire_state(record.state).into(),
            signature_payload: self.payload(&record.authorization_xdr)?,
            authorization_entry_xdr: record.authorization_xdr,
            expiration_ledger: u32::try_from(record.expiration_ledger)
                .map_err(|_| corrupt("expiration ledger"))?,
            transaction_hash: record
                .transaction_hash
                .as_deref()
                .map(fermah_pay_stellar_chain::rpc::hex_lower)
                .unwrap_or_default(),
            ledger: wire_ledger(record.ledger),
            created_at: timestamp(record.created_at)?,
        })
    }

    fn exit_to_wire(&self, record: VaultRequestRecord) -> Result<Exit, Status> {
        let VaultRequestKind::RequestExit { amount, destination } = record.kind else {
            return Err(Refusal::ExitNotFound.into());
        };
        Ok(Exit {
            exit_id: record.id.to_string(),
            buyer_id: record.buyer_id.to_string(),
            amount,
            destination: destination.to_string(),
            state: wire_state(record.state).into(),
            signature_payload: self.payload(&record.authorization_xdr)?,
            authorization_entry_xdr: record.authorization_xdr,
            expiration_ledger: u32::try_from(record.expiration_ledger)
                .map_err(|_| corrupt("expiration ledger"))?,
            transaction_hash: record
                .transaction_hash
                .as_deref()
                .map(fermah_pay_stellar_chain::rpc::hex_lower)
                .unwrap_or_default(),
            ledger: wire_ledger(record.ledger),
            created_at: timestamp(record.created_at)?,
        })
    }

    /// The caller's request of the expected kind; one of the other kind is
    /// reported as not found.
    async fn existing_request(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
        exit: bool,
    ) -> Result<Option<VaultRequestRecord>, Status> {
        let record = self.store.vault_request(scope, id, key).await.map_err(|e| internal(&e))?;
        Ok(record.filter(|r| matches!(r.kind, VaultRequestKind::RequestExit { .. }) == exit))
    }
}

impl<L: LatestLedger> LedgerApi<L> {
    /// Prepares a request of `kind` for `buyer_id`, or answers a retry of
    /// one. `Ok(None)` for a key taken by another request.
    async fn prepare_vault_request(
        &self,
        scope: &Scope,
        buyer_id: Uuid,
        key: &IdempotencyKey,
        kind: impl FnOnce(&fermah_pay_stellar_domain::ChainAddress) -> Result<VaultRequestKind, Refusal>,
    ) -> Result<(VaultRequestRecord, bool), Status> {
        let deployment = self
            .store
            .ledger_binding(scope)
            .await
            .map_err(|e| internal(&e))?
            .ok_or(Refusal::LedgerNotConfigured)?;
        if !deployment.is_vault() {
            return Err(Refusal::NotAVault.into());
        }
        let wallet = self
            .store
            .buyer_wallet(scope, buyer_id)
            .await
            .map_err(|e| internal(&e))?
            .ok_or(Refusal::BuyerNotFound)?;
        let kind = kind(&wallet)?;
        let latest = self.ledger.latest_ledger().await.map_err(|e| network_unavailable(&e))?;
        let expiration_ledger = latest
            .checked_add(self.policy.authorization_validity_ledgers)
            .ok_or(Refusal::Internal)?;
        let invocation = match &kind {
            VaultRequestKind::SetCap { cap } => {
                deployment.set_cap_authorization(&wallet, i128::from(*cap))
            }
            VaultRequestKind::RequestExit { amount, destination } => {
                deployment.request_exit_authorization(&wallet, i128::from(*amount), destination)
            }
        };
        let entry = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
                address: wallet.sc_address(),
                nonce: i64::from_le_bytes(random()?),
                signature_expiration_ledger: expiration_ledger,
                signature: ScVal::Void,
            }),
            root_invocation: invocation,
        };
        let authorization_xdr =
            entry.to_xdr_base64(Limits::none()).map_err(|_| corrupt("new authorization entry"))?;
        let inserted = self
            .store
            .insert_vault_request(
                scope,
                &NewVaultRequest {
                    buyer_id,
                    key,
                    kind,
                    authorization_xdr: &authorization_xdr,
                    expiration_ledger,
                },
            )
            .await
            .map_err(|e| internal(&e))?;
        let id = match inserted {
            Insertion::Created(id) => id,
            Insertion::QuotaExceeded => return Err(Refusal::MandateQuotaExceeded.into()),
            Insertion::DeploymentQuotaExceeded => {
                return Err(Refusal::DeploymentMandateQuotaExceeded.into());
            }
            Insertion::KeyTaken => {
                let existing = self
                    .store
                    .vault_request(scope, None, Some(key))
                    .await
                    .map_err(|e| internal(&e))?
                    .ok_or(Refusal::Internal)?;
                return Ok((existing, false));
            }
        };
        let created = self
            .store
            .vault_request(scope, Some(id), None)
            .await
            .map_err(|e| internal(&e))?
            .ok_or(Refusal::Internal)?;
        Ok((created, true))
    }

    /// Verifies and stores the buyer's signed entry for request `id`.
    async fn submit_vault_request(
        &self,
        scope: &Scope,
        id: Uuid,
        signed_xdr: &str,
        exit: bool,
        refusals: &Refusals,
    ) -> Result<VaultRequestRecord, Status> {
        let signed = decode_entry(signed_xdr).ok_or(Refusal::InvalidAuthorizationEntry)?;
        let record =
            self.existing_request(scope, Some(id), None, exit).await?.ok_or(refusals.not_found)?;
        match signing(&record, &signed, refusals) {
            Signing::Open => {}
            Signing::Replay => return Ok(record),
            Signing::Closed(refusal) => return Err(refusal.into()),
        }
        let prepared = decode_entry(&record.authorization_xdr)
            .ok_or_else(|| corrupt("authorization entry"))?;
        if unsigned(&signed) != prepared {
            return Err(Refusal::AuthorizationMismatch.into());
        }
        self.verify_buyer_entry(scope, &record.wallet, &signed, &prepared, |_| vec![])
            .await
            .map_err(|refusal| refusal.expired_as(refusals.expired))?;
        let signed_xdr = signed
            .to_xdr_base64(Limits::none())
            .map_err(|_| corrupt("signed authorization entry"))?;
        self.store.sign_vault_request(scope, id, &signed_xdr).await.map_err(|e| internal(&e))?;
        let record =
            self.existing_request(scope, Some(id), None, exit).await?.ok_or(Refusal::Internal)?;
        match signing(&record, &signed, refusals) {
            Signing::Replay => Ok(record),
            Signing::Closed(refusal) => Err(refusal.into()),
            Signing::Open => Err(corrupt("vault request still unsigned after signing")),
        }
    }

    pub(super) async fn prepare_limit_change_request(
        &self,
        request: Request<PrepareLimitChangeRequest>,
    ) -> Result<Response<PrepareLimitChangeResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let buyer_id = parse_id(&body.buyer_id, Refusal::InvalidBuyerId)?;
        let key = parse_key(&body.idempotency_key)?;
        if body.daily_limit < 0 {
            return Err(Refusal::InvalidLimit.into());
        }
        let cap = body.daily_limit;
        let replay = |existing: VaultRequestRecord| {
            if existing.buyer_id != buyer_id || existing.kind != (VaultRequestKind::SetCap { cap })
            {
                return Err(Status::from(Refusal::IdempotencyConflict));
            }
            Ok(Response::new(PrepareLimitChangeResponse {
                limit_change: Some(self.limit_change_to_wire(existing)?),
                created: false,
            }))
        };
        if let Some(existing) =
            self.store.vault_request(&scope, None, Some(&key)).await.map_err(|e| internal(&e))?
        {
            return replay(existing);
        }
        let (record, created) = self
            .prepare_vault_request(&scope, buyer_id, &key, |_| Ok(VaultRequestKind::SetCap { cap }))
            .await?;
        if !created {
            return replay(record);
        }
        Ok(Response::new(PrepareLimitChangeResponse {
            limit_change: Some(self.limit_change_to_wire(record)?),
            created: true,
        }))
    }

    pub(super) async fn submit_limit_change_request(
        &self,
        request: Request<SubmitLimitChangeRequest>,
    ) -> Result<Response<SubmitLimitChangeResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let id = parse_id(&body.limit_change_id, Refusal::InvalidLimitChangeId)?;
        let record = self
            .submit_vault_request(
                &scope,
                id,
                &body.signed_authorization_entry_xdr,
                false,
                &LIMIT_CHANGE,
            )
            .await?;
        Ok(Response::new(SubmitLimitChangeResponse {
            limit_change: Some(self.limit_change_to_wire(record)?),
        }))
    }

    pub(super) async fn get_limit_change_request(
        &self,
        request: Request<GetLimitChangeRequest>,
    ) -> Result<Response<GetLimitChangeResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let id = parse_id(&request.into_inner().limit_change_id, Refusal::InvalidLimitChangeId)?;
        let record = self
            .existing_request(&scope, Some(id), None, false)
            .await?
            .ok_or(Refusal::LimitChangeNotFound)?;
        Ok(Response::new(GetLimitChangeResponse {
            limit_change: Some(self.limit_change_to_wire(record)?),
        }))
    }

    pub(super) async fn prepare_exit_request(
        &self,
        request: Request<PrepareExitRequest>,
    ) -> Result<Response<PrepareExitResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let buyer_id = parse_id(&body.buyer_id, Refusal::InvalidBuyerId)?;
        let amount = parse_amount(body.amount)?;
        let key = parse_key(&body.idempotency_key)?;
        let replay = |existing: VaultRequestRecord| {
            let same = existing.buyer_id == buyer_id
                && matches!(&existing.kind, VaultRequestKind::RequestExit { amount: a, destination }
                    if *a == amount && (body.destination.is_empty()
                        || destination.to_string() == body.destination));
            if !same {
                return Err(Status::from(Refusal::IdempotencyConflict));
            }
            Ok(Response::new(PrepareExitResponse {
                exit: Some(self.exit_to_wire(existing)?),
                created: false,
            }))
        };
        if let Some(existing) =
            self.store.vault_request(&scope, None, Some(&key)).await.map_err(|e| internal(&e))?
        {
            return replay(existing);
        }
        let allow_others = self.policy.withdrawals_to_other_accounts;
        let (record, created) = self
            .prepare_vault_request(&scope, buyer_id, &key, |wallet| {
                let destination = parse_destination(&body.destination, wallet)?;
                if destination != *wallet && !allow_others {
                    return Err(Refusal::DestinationNotAllowed);
                }
                Ok(VaultRequestKind::RequestExit { amount, destination })
            })
            .await?;
        if !created {
            return replay(record);
        }
        Ok(Response::new(PrepareExitResponse { exit: Some(self.exit_to_wire(record)?), created }))
    }

    pub(super) async fn submit_exit_request(
        &self,
        request: Request<SubmitExitRequest>,
    ) -> Result<Response<SubmitExitResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let id = parse_id(&body.exit_id, Refusal::InvalidExitId)?;
        let record = self
            .submit_vault_request(&scope, id, &body.signed_authorization_entry_xdr, true, &EXIT)
            .await?;
        Ok(Response::new(SubmitExitResponse { exit: Some(self.exit_to_wire(record)?) }))
    }

    pub(super) async fn get_exit_request(
        &self,
        request: Request<GetExitRequest>,
    ) -> Result<Response<GetExitResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let id = parse_id(&request.into_inner().exit_id, Refusal::InvalidExitId)?;
        let record = self
            .existing_request(&scope, Some(id), None, true)
            .await?
            .ok_or(Refusal::ExitNotFound)?;
        Ok(Response::new(GetExitResponse { exit: Some(self.exit_to_wire(record)?) }))
    }
}
