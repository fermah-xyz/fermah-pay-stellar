//! Recurring charges: mandates the buyer signs once, their revocation, and
//! the seller's charge of each period. The API prepares what the buyer
//! signs and verifies what comes back; the worker submits it and settles
//! the charges.

use fermah_pay_stellar_chain::authorization::{signature_payload, verify_signed_entry};
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::MandateIntent;
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::stellar_xdr::{
    Limits, ScAddress, ScVal, SorobanAddressCredentials, SorobanAuthorizationEntry,
    SorobanAuthorizedInvocation, SorobanCredentials, WriteXdr,
};
use fermah_pay_stellar_chain::transaction::account_id;
use fermah_pay_stellar_domain::{AccountAddress, IdempotencyKey};
use fermah_pay_stellar_proto::v1::{
    CreateRecurringChargeRequest, CreateRecurringChargeResponse, GetMandateRequest,
    GetMandateResponse, GetRecurringChargeRequest, GetRecurringChargeResponse,
    GetRevocationRequest, GetRevocationResponse, Mandate, MandateState as WireMandateState,
    PrepareMandateRequest, PrepareMandateResponse, PrepareRevocationRequest,
    PrepareRevocationResponse, RecurringCharge, RecurringChargeState as WireRecurringChargeState,
    Revocation, RevocationState as WireRevocationState, SubmitMandateRequest,
    SubmitMandateResponse, SubmitRevocationRequest, SubmitRevocationResponse,
};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::store::{
    Insertion, MandateRecord, MandateState, NewMandate, NewRecurringCharge, NewRevocation,
    RecurringAdmission, RecurringChargeRecord, RecurringChargeState, RevocationRecord,
    RevocationState,
};
use super::{
    LatestLedger, LedgerApi, corrupt, decode_entry, internal, network_unavailable, parse_amount,
    parse_id, parse_key, random, record_scope, signature_refusal, timestamp, unsigned, wire_ledger,
};
use crate::auth::scope_of;
use crate::refusal::Refusal;
use crate::scope::Scope;

/// Seconds per ledger assumed when sizing a mandate's last ledger. Ledgers
/// close about every five seconds; assuming four leaves the last period
/// room if they close faster for a while. Ledgers closing slower only leave
/// the allowance live a little longer than the periods need.
const SECONDS_PER_LEDGER: u64 = 4;

const fn mandate_state(state: MandateState) -> WireMandateState {
    match state {
        MandateState::AwaitingSignature => WireMandateState::AwaitingSignature,
        MandateState::Signed => WireMandateState::Signed,
        MandateState::Submitted => WireMandateState::Submitted,
        MandateState::Active => WireMandateState::Active,
        MandateState::Failed => WireMandateState::Failed,
        MandateState::Expired => WireMandateState::Expired,
        MandateState::Replaced => WireMandateState::Replaced,
        MandateState::Revoked => WireMandateState::Revoked,
        MandateState::Ended => WireMandateState::Ended,
    }
}

const fn revocation_state(state: RevocationState) -> WireRevocationState {
    match state {
        RevocationState::AwaitingSignature => WireRevocationState::AwaitingSignature,
        RevocationState::Signed => WireRevocationState::Signed,
        RevocationState::Submitted => WireRevocationState::Submitted,
        RevocationState::Confirmed => WireRevocationState::Confirmed,
        RevocationState::Failed => WireRevocationState::Failed,
        RevocationState::Expired => WireRevocationState::Expired,
    }
}

const fn recurring_state(state: RecurringChargeState) -> WireRecurringChargeState {
    match state {
        RecurringChargeState::Admitted => WireRecurringChargeState::Admitted,
        RecurringChargeState::Submitted => WireRecurringChargeState::Submitted,
        RecurringChargeState::Charged => WireRecurringChargeState::Charged,
        RecurringChargeState::Refused => WireRecurringChargeState::Refused,
        RecurringChargeState::Quarantined => WireRecurringChargeState::Quarantined,
    }
}

/// Whether a signed entry can still be stored, given the row's state and
/// what it already holds.
enum Signing {
    Open,
    /// The row already holds exactly this signed entry: a retry of a
    /// submission that succeeded, which changes nothing.
    Replay,
    Closed(Refusal),
}

fn signing(
    awaiting: bool,
    expired: bool,
    stored: Option<&str>,
    signed: &SorobanAuthorizationEntry,
    (lapsed, taken): (Refusal, Refusal),
) -> Signing {
    let stored = stored.and_then(decode_entry);
    if awaiting {
        Signing::Open
    } else if stored.as_ref() == Some(signed) {
        Signing::Replay
    } else if expired {
        Signing::Closed(lapsed)
    } else {
        Signing::Closed(taken)
    }
}

fn mandate_signing(record: &MandateRecord, signed: &SorobanAuthorizationEntry) -> Signing {
    signing(
        record.state == MandateState::AwaitingSignature,
        record.state == MandateState::Expired,
        record.signed_authorization_xdr.as_deref(),
        signed,
        (Refusal::MandateAuthorizationExpired, Refusal::MandateAlreadySigned),
    )
}

fn revocation_signing(record: &RevocationRecord, signed: &SorobanAuthorizationEntry) -> Signing {
    signing(
        record.state == RevocationState::AwaitingSignature,
        record.state == RevocationState::Expired,
        record.signed_authorization_xdr.as_deref(),
        signed,
        (Refusal::RevocationExpired, Refusal::RevocationAlreadySigned),
    )
}

/// The ledger by which every period of a mandate prepared at `latest` has
/// ended, with room for the buyer to sign and the worker to send it, or
/// `None` if it is beyond `max_ledgers` from `latest`.
fn mandate_live_until(
    latest: u32,
    signing_room: u32,
    period_secs: u64,
    cycles: u32,
    max_ledgers: u32,
) -> Option<u32> {
    let seconds = period_secs.checked_mul(u64::from(cycles))?;
    let ledgers = u32::try_from(seconds.div_ceil(SECONDS_PER_LEDGER)).ok()?;
    let ahead = ledgers.checked_add(signing_room)?;
    (ahead <= max_ledgers).then_some(())?;
    latest.checked_add(ahead)
}

/// The period of a mandate that started at `starts_at`, at ledger time
/// `now`, both in Unix seconds. The charge lands in a later ledger; at a
/// period's edge the contract's answer (`not_due` or `period_over`) is what
/// counts.
fn current_cycle(starts_at: i64, period_secs: i64, now: i64) -> Option<u32> {
    let elapsed = now.saturating_sub(starts_at).max(0);
    u32::try_from(elapsed.checked_div(period_secs)?).ok()
}

fn buyer_entry(
    wallet: &AccountAddress,
    expiration_ledger: u32,
    invocation: SorobanAuthorizedInvocation,
) -> Result<(SorobanAuthorizationEntry, String), Status> {
    let entry = SorobanAuthorizationEntry {
        credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
            address: ScAddress::Account(account_id(wallet)),
            nonce: i64::from_le_bytes(random()?),
            signature_expiration_ledger: expiration_ledger,
            signature: ScVal::Void,
        }),
        root_invocation: invocation,
    };
    let xdr =
        entry.to_xdr_base64(Limits::none()).map_err(|_| corrupt("new authorization entry"))?;
    Ok((entry, xdr))
}

impl<L> LedgerApi<L> {
    fn payload_of(&self, authorization_xdr: &str) -> Result<String, Status> {
        let entry =
            decode_entry(authorization_xdr).ok_or_else(|| corrupt("authorization entry"))?;
        let payload =
            signature_payload(network_id(self.network), &entry.credentials, &entry.root_invocation)
                .map_err(|_| corrupt("authorization credentials"))?;
        Ok(hex_lower(&payload))
    }

    fn mandate_to_wire(&self, record: MandateRecord) -> Result<Mandate, Status> {
        let signature_payload = self.payload_of(&record.authorization_xdr)?;
        let narrow = |value: i64, what| u32::try_from(value).map_err(|_| corrupt(what));
        Ok(Mandate {
            mandate_id: record.id.to_string(),
            buyer_id: record.buyer_id.to_string(),
            amount: record.amount,
            period_secs: u64::try_from(record.period_secs).map_err(|_| corrupt("period"))?,
            cycles: u32::try_from(record.cycles).map_err(|_| corrupt("cycles"))?,
            live_until_ledger: narrow(record.live_until, "mandate last ledger")?,
            state: mandate_state(record.state).into(),
            authorization_entry_xdr: record.authorization_xdr,
            signature_payload,
            expiration_ledger: narrow(record.expiration_ledger, "expiration ledger")?,
            starts_at: record.starts_at.and_then(|s| u64::try_from(s).ok()).unwrap_or(0),
            transaction_hash: record.transaction_hash.as_deref().map(hex_lower).unwrap_or_default(),
            ledger: wire_ledger(record.ledger),
            created_at: timestamp(record.created_at)?,
        })
    }

    fn revocation_to_wire(&self, record: RevocationRecord) -> Result<Revocation, Status> {
        let signature_payload = self.payload_of(&record.authorization_xdr)?;
        Ok(Revocation {
            revocation_id: record.id.to_string(),
            buyer_id: record.buyer_id.to_string(),
            state: revocation_state(record.state).into(),
            authorization_entry_xdr: record.authorization_xdr,
            signature_payload,
            expiration_ledger: u32::try_from(record.expiration_ledger)
                .map_err(|_| corrupt("expiration ledger"))?,
            transaction_hash: record.transaction_hash.as_deref().map(hex_lower).unwrap_or_default(),
            ledger: wire_ledger(record.ledger),
            created_at: timestamp(record.created_at)?,
        })
    }

    async fn existing_mandate(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<MandateRecord>, Status> {
        self.store.mandate(scope, id, key).await.map_err(|e| internal(&e))
    }

    async fn existing_revocation(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<RevocationRecord>, Status> {
        self.store.revocation(scope, id, key).await.map_err(|e| internal(&e))
    }

    async fn existing_recurring(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<RecurringChargeRecord>, Status> {
        self.store.recurring_charge(scope, id, key).await.map_err(|e| internal(&e))
    }

    fn replay_mandate(
        &self,
        existing: MandateRecord,
        body: &PrepareMandateRequest,
        buyer_id: Uuid,
    ) -> Result<Response<PrepareMandateResponse>, Status> {
        let same = existing.buyer_id == buyer_id
            && existing.amount == body.amount
            && u64::try_from(existing.period_secs).ok() == Some(body.period_secs)
            && u32::try_from(existing.cycles).ok() == Some(body.cycles);
        if !same {
            return Err(Refusal::IdempotencyConflict.into());
        }
        Ok(Response::new(PrepareMandateResponse {
            mandate: Some(self.mandate_to_wire(existing)?),
            created: false,
        }))
    }

    fn replay_revocation(
        &self,
        existing: RevocationRecord,
        buyer_id: Uuid,
    ) -> Result<Response<PrepareRevocationResponse>, Status> {
        if existing.buyer_id != buyer_id {
            return Err(Refusal::IdempotencyConflict.into());
        }
        Ok(Response::new(PrepareRevocationResponse {
            revocation: Some(self.revocation_to_wire(existing)?),
            created: false,
        }))
    }
}

fn recurring_to_wire(record: RecurringChargeRecord) -> Result<RecurringCharge, Status> {
    Ok(RecurringCharge {
        recurring_charge_id: record.id.to_string(),
        mandate_id: record.mandate.to_string(),
        buyer_id: record.buyer_id.to_string(),
        cycle: u32::try_from(record.cycle).map_err(|_| corrupt("period"))?,
        amount: record.amount,
        state: recurring_state(record.state).into(),
        outcome: record.outcome.unwrap_or_default(),
        transaction_hash: record.transaction_hash.as_deref().map(hex_lower).unwrap_or_default(),
        ledger: wire_ledger(record.ledger),
        created_at: timestamp(record.created_at)?,
    })
}

impl<L: LatestLedger> LedgerApi<L> {
    pub(super) async fn prepare_mandate_request(
        &self,
        request: Request<PrepareMandateRequest>,
    ) -> Result<Response<PrepareMandateResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let buyer_id = parse_id(&body.buyer_id, Refusal::InvalidBuyerId)?;
        let amount = parse_amount(body.amount)?;
        let key = parse_key(&body.idempotency_key)?;
        if body.period_secs < self.policy.min_mandate_period_secs {
            return Err(Refusal::InvalidPeriod.into());
        }
        if body.cycles == 0 {
            return Err(Refusal::InvalidCycles.into());
        }
        if let Some(existing) = self.existing_mandate(&scope, None, Some(&key)).await? {
            return self.replay_mandate(existing, &body, buyer_id);
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
        let validity = self.policy.authorization_validity_ledgers;
        let expiration_ledger = latest.checked_add(validity).ok_or(Refusal::Internal)?;
        let live_until = mandate_live_until(
            latest,
            validity,
            body.period_secs,
            body.cycles,
            self.policy.max_mandate_ledgers,
        )
        .ok_or(Refusal::MandateTooLong)?;
        let intent = MandateIntent {
            owner: wallet.clone(),
            mandate_id: random()?,
            amount: i128::from(amount),
            period_secs: body.period_secs,
            cycles: body.cycles,
            live_until,
        };
        let (_, authorization_xdr) = buyer_entry(
            &wallet,
            expiration_ledger,
            deployment.authorize_recurring_authorization(&intent),
        )?;
        let inserted = self
            .store
            .insert_mandate(
                &scope,
                &NewMandate {
                    buyer_id,
                    key: &key,
                    mandate_id: intent.mandate_id,
                    amount,
                    period_secs: body.period_secs,
                    cycles: body.cycles,
                    live_until,
                    authorization_xdr: &authorization_xdr,
                    expiration_ledger,
                },
            )
            .await
            .map_err(|e| internal(&e))?;
        let id = match inserted {
            Insertion::Created(id) => id,
            Insertion::QuotaExceeded => return Err(Refusal::MandateQuotaExceeded.into()),
            Insertion::KeyTaken => {
                let existing = self
                    .existing_mandate(&scope, None, Some(&key))
                    .await?
                    .ok_or(Refusal::Internal)?;
                return self.replay_mandate(existing, &body, buyer_id);
            }
        };
        let created =
            self.existing_mandate(&scope, Some(id), None).await?.ok_or(Refusal::Internal)?;
        Ok(Response::new(PrepareMandateResponse {
            mandate: Some(self.mandate_to_wire(created)?),
            created: true,
        }))
    }

    pub(super) async fn submit_mandate_request(
        &self,
        request: Request<SubmitMandateRequest>,
    ) -> Result<Response<SubmitMandateResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let id = parse_id(&body.mandate_id, Refusal::InvalidMandateId)?;
        let signed = decode_entry(&body.signed_authorization_entry_xdr)
            .ok_or(Refusal::InvalidAuthorizationEntry)?;
        let respond = |record: MandateRecord| -> Result<Response<SubmitMandateResponse>, Status> {
            Ok(Response::new(SubmitMandateResponse {
                mandate: Some(self.mandate_to_wire(record)?),
            }))
        };
        let record =
            self.existing_mandate(&scope, Some(id), None).await?.ok_or(Refusal::MandateNotFound)?;
        match mandate_signing(&record, &signed) {
            Signing::Open => {}
            Signing::Replay => return respond(record),
            Signing::Closed(refusal) => return Err(refusal.into()),
        }
        let prepared = decode_entry(&record.authorization_xdr)
            .ok_or_else(|| corrupt("authorization entry"))?;
        if unsigned(&signed) != prepared {
            return Err(Refusal::AuthorizationMismatch.into());
        }
        let latest = self.ledger.latest_ledger().await.map_err(|e| network_unavailable(&e))?;
        verify_signed_entry(
            &signed,
            &record.wallet,
            &prepared.root_invocation,
            network_id(self.network),
            latest,
            self.policy.authorization_validity_ledgers,
        )
        .map_err(|refusal| signature_refusal(&refusal, Refusal::MandateAuthorizationExpired))?;
        let signed_xdr = signed
            .to_xdr_base64(Limits::none())
            .map_err(|_| corrupt("signed authorization entry"))?;
        self.store.sign_mandate(&scope, id, &signed_xdr).await.map_err(|e| internal(&e))?;
        // Whether this request or a concurrent one stored the signature, the
        // stored row now decides the answer.
        let record =
            self.existing_mandate(&scope, Some(id), None).await?.ok_or(Refusal::Internal)?;
        match mandate_signing(&record, &signed) {
            Signing::Replay => respond(record),
            Signing::Closed(refusal) => Err(refusal.into()),
            Signing::Open => Err(corrupt("mandate still unsigned after signing")),
        }
    }

    pub(super) async fn get_mandate_request(
        &self,
        request: Request<GetMandateRequest>,
    ) -> Result<Response<GetMandateResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let id = parse_id(&request.into_inner().mandate_id, Refusal::InvalidMandateId)?;
        let record =
            self.existing_mandate(&scope, Some(id), None).await?.ok_or(Refusal::MandateNotFound)?;
        Ok(Response::new(GetMandateResponse { mandate: Some(self.mandate_to_wire(record)?) }))
    }

    pub(super) async fn prepare_revocation_request(
        &self,
        request: Request<PrepareRevocationRequest>,
    ) -> Result<Response<PrepareRevocationResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let buyer_id = parse_id(&body.buyer_id, Refusal::InvalidBuyerId)?;
        let key = parse_key(&body.idempotency_key)?;
        if let Some(existing) = self.existing_revocation(&scope, None, Some(&key)).await? {
            return self.replay_revocation(existing, buyer_id);
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
        let (_, authorization_xdr) = buyer_entry(
            &wallet,
            expiration_ledger,
            deployment.revoke_recurring_authorization(&wallet),
        )?;
        let inserted = self
            .store
            .insert_revocation(
                &scope,
                &NewRevocation {
                    buyer_id,
                    key: &key,
                    authorization_xdr: &authorization_xdr,
                    expiration_ledger,
                },
            )
            .await
            .map_err(|e| internal(&e))?;
        let id = match inserted {
            Insertion::Created(id) => id,
            Insertion::QuotaExceeded => return Err(Refusal::MandateQuotaExceeded.into()),
            Insertion::KeyTaken => {
                let existing = self
                    .existing_revocation(&scope, None, Some(&key))
                    .await?
                    .ok_or(Refusal::Internal)?;
                return self.replay_revocation(existing, buyer_id);
            }
        };
        let created =
            self.existing_revocation(&scope, Some(id), None).await?.ok_or(Refusal::Internal)?;
        Ok(Response::new(PrepareRevocationResponse {
            revocation: Some(self.revocation_to_wire(created)?),
            created: true,
        }))
    }

    pub(super) async fn submit_revocation_request(
        &self,
        request: Request<SubmitRevocationRequest>,
    ) -> Result<Response<SubmitRevocationResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let id = parse_id(&body.revocation_id, Refusal::InvalidRevocationId)?;
        let signed = decode_entry(&body.signed_authorization_entry_xdr)
            .ok_or(Refusal::InvalidAuthorizationEntry)?;
        let respond =
            |record: RevocationRecord| -> Result<Response<SubmitRevocationResponse>, Status> {
                Ok(Response::new(SubmitRevocationResponse {
                    revocation: Some(self.revocation_to_wire(record)?),
                }))
            };
        let record = self
            .existing_revocation(&scope, Some(id), None)
            .await?
            .ok_or(Refusal::RevocationNotFound)?;
        match revocation_signing(&record, &signed) {
            Signing::Open => {}
            Signing::Replay => return respond(record),
            Signing::Closed(refusal) => return Err(refusal.into()),
        }
        let prepared = decode_entry(&record.authorization_xdr)
            .ok_or_else(|| corrupt("authorization entry"))?;
        if unsigned(&signed) != prepared {
            return Err(Refusal::AuthorizationMismatch.into());
        }
        let latest = self.ledger.latest_ledger().await.map_err(|e| network_unavailable(&e))?;
        verify_signed_entry(
            &signed,
            &record.wallet,
            &prepared.root_invocation,
            network_id(self.network),
            latest,
            self.policy.authorization_validity_ledgers,
        )
        .map_err(|refusal| signature_refusal(&refusal, Refusal::RevocationExpired))?;
        let signed_xdr = signed
            .to_xdr_base64(Limits::none())
            .map_err(|_| corrupt("signed authorization entry"))?;
        self.store.sign_revocation(&scope, id, &signed_xdr).await.map_err(|e| internal(&e))?;
        let record =
            self.existing_revocation(&scope, Some(id), None).await?.ok_or(Refusal::Internal)?;
        match revocation_signing(&record, &signed) {
            Signing::Replay => respond(record),
            Signing::Closed(refusal) => Err(refusal.into()),
            Signing::Open => Err(corrupt("revocation still unsigned after signing")),
        }
    }

    pub(super) async fn get_revocation_request(
        &self,
        request: Request<GetRevocationRequest>,
    ) -> Result<Response<GetRevocationResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let id = parse_id(&request.into_inner().revocation_id, Refusal::InvalidRevocationId)?;
        let record = self
            .existing_revocation(&scope, Some(id), None)
            .await?
            .ok_or(Refusal::RevocationNotFound)?;
        Ok(Response::new(GetRevocationResponse {
            revocation: Some(self.revocation_to_wire(record)?),
        }))
    }

    pub(super) async fn create_recurring_charge_request(
        &self,
        request: Request<CreateRecurringChargeRequest>,
    ) -> Result<Response<CreateRecurringChargeResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let mandate_row = parse_id(&body.mandate_id, Refusal::InvalidMandateId)?;
        let amount = parse_amount(body.amount)?;
        let key = parse_key(&body.idempotency_key)?;
        let replay = |existing: RecurringChargeRecord| {
            if existing.mandate != mandate_row || existing.amount != amount {
                return Err(Status::from(Refusal::IdempotencyConflict));
            }
            Ok(Response::new(CreateRecurringChargeResponse {
                recurring_charge: Some(recurring_to_wire(existing)?),
                created: false,
            }))
        };
        if let Some(existing) = self.existing_recurring(&scope, None, Some(&key)).await? {
            return replay(existing);
        }
        let mandate = self
            .existing_mandate(&scope, Some(mandate_row), None)
            .await?
            .ok_or(Refusal::MandateNotFound)?;
        match mandate.state {
            MandateState::Active => {}
            MandateState::Ended => return Err(Refusal::MandateEnded.into()),
            _ => return Err(Refusal::MandateNotActive.into()),
        }
        if amount > mandate.amount {
            return Err(Refusal::AboveMandate.into());
        }
        let starts_at = mandate.starts_at.ok_or_else(|| corrupt("active mandate without start"))?;
        let now = self.ledger.latest_close_time().await.map_err(|e| network_unavailable(&e))?;
        let cycle = current_cycle(starts_at, mandate.period_secs, now)
            .ok_or_else(|| corrupt("mandate period"))?;
        let latest = self.ledger.latest_ledger().await.map_err(|e| network_unavailable(&e))?;
        if i64::from(cycle) >= i64::from(mandate.cycles) || i64::from(latest) > mandate.live_until {
            return Err(Refusal::MandateEnded.into());
        }
        let window = latest.saturating_add(self.policy.charge_validity_ledgers);
        let last_ledger = u32::try_from(mandate.live_until.min(i64::from(window)))
            .map_err(|_| corrupt("mandate last ledger"))?;
        let admitted = self
            .store
            .admit_recurring_charge(
                &scope,
                &NewRecurringCharge {
                    mandate: mandate.id,
                    buyer_id: mandate.buyer_id,
                    key: &key,
                    cycle,
                    charge_id: random()?,
                    amount,
                    last_ledger,
                },
            )
            .await
            .map_err(|e| internal(&e))?;
        let id = match admitted {
            RecurringAdmission::Created(id) => id,
            RecurringAdmission::PeriodTaken => return Err(Refusal::PeriodAlreadyCharged.into()),
            RecurringAdmission::KeyTaken => {
                let existing = self
                    .existing_recurring(&scope, None, Some(&key))
                    .await?
                    .ok_or(Refusal::Internal)?;
                return replay(existing);
            }
        };
        let created =
            self.existing_recurring(&scope, Some(id), None).await?.ok_or(Refusal::Internal)?;
        Ok(Response::new(CreateRecurringChargeResponse {
            recurring_charge: Some(recurring_to_wire(created)?),
            created: true,
        }))
    }

    pub(super) async fn get_recurring_charge_request(
        &self,
        request: Request<GetRecurringChargeRequest>,
    ) -> Result<Response<GetRecurringChargeResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let id =
            parse_id(&request.into_inner().recurring_charge_id, Refusal::InvalidRecurringChargeId)?;
        let record = self
            .existing_recurring(&scope, Some(id), None)
            .await?
            .ok_or(Refusal::RecurringChargeNotFound)?;
        Ok(Response::new(GetRecurringChargeResponse {
            recurring_charge: Some(recurring_to_wire(record)?),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_mandate_lives_until_its_last_period_ends_at_four_seconds_a_ledger() {
        // Two 30-day periods: 5,184,000 s, 1,296,000 ledgers at 4 s, plus
        // the 720 ledgers the buyer has to sign.
        assert_eq!(mandate_live_until(1_000, 720, 2_592_000, 2, 3_000_000), Some(1_297_720));
        assert_eq!(mandate_live_until(1_000, 720, 2_592_000, 3, 1_000_000), None);
        assert_eq!(mandate_live_until(1_000, 720, u64::MAX, 2, u32::MAX), None);
    }

    #[test]
    fn test_the_current_period_counts_from_the_start() {
        assert_eq!(current_cycle(1_000, 100, 1_000), Some(0));
        assert_eq!(current_cycle(1_000, 100, 1_099), Some(0));
        assert_eq!(current_cycle(1_000, 100, 1_100), Some(1));
        // A ledger time a little behind the start still names the first
        // period.
        assert_eq!(current_cycle(1_000, 100, 990), Some(0));
    }
}
