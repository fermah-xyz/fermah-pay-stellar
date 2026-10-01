//! Withdrawals of unused credit: prepared for the buyer to sign, then held
//! from the available balance when the signed entry is stored. The worker
//! adds the treasury's authorization and submits them.

use fermah_pay_stellar_chain::authorization::{signature_payload, verify_signed_entry};
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::WithdrawIntent;
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::stellar_xdr::{
    Limits, ScAddress, ScVal, SorobanAddressCredentials, SorobanAuthorizationEntry,
    SorobanCredentials, WriteXdr,
};
use fermah_pay_stellar_chain::transaction::account_id;
use fermah_pay_stellar_domain::{AccountAddress, IdempotencyKey};
use fermah_pay_stellar_proto::v1::{
    GetWithdrawalRequest, GetWithdrawalResponse, PrepareWithdrawalRequest,
    PrepareWithdrawalResponse, SubmitWithdrawalRequest, SubmitWithdrawalResponse, Withdrawal,
    WithdrawalState as WireWithdrawalState,
};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::store::{
    Insertion, NewWithdrawal, WithdrawalRecord, WithdrawalSigning, WithdrawalState,
};
use super::{
    LatestLedger, LedgerApi, corrupt, decode_entry, internal, network_unavailable, parse_amount,
    parse_id, parse_key, random, record_scope, signature_refusal, timestamp, unsigned, wire_ledger,
};
use crate::auth::scope_of;
use crate::refusal::Refusal;
use crate::scope::Scope;

const fn withdrawal_state(state: WithdrawalState) -> WireWithdrawalState {
    match state {
        WithdrawalState::AwaitingSignature => WireWithdrawalState::AwaitingSignature,
        WithdrawalState::Signed => WireWithdrawalState::Signed,
        WithdrawalState::Submitted => WireWithdrawalState::Submitted,
        WithdrawalState::Confirmed => WireWithdrawalState::Confirmed,
        WithdrawalState::Failed => WireWithdrawalState::Failed,
        WithdrawalState::Expired => WireWithdrawalState::Expired,
    }
}

enum Signing {
    Open,
    /// The withdrawal already holds exactly this signed entry: a retry of a
    /// submission that succeeded, which changes nothing.
    Replay,
    Closed(Refusal),
}

fn signing(record: &WithdrawalRecord, signed: &SorobanAuthorizationEntry) -> Signing {
    let stored = record.signed_authorization_xdr.as_deref().and_then(decode_entry);
    match record.state {
        WithdrawalState::AwaitingSignature => Signing::Open,
        _ if stored.as_ref() == Some(signed) => Signing::Replay,
        WithdrawalState::Expired => Signing::Closed(Refusal::WithdrawalExpired),
        _ => Signing::Closed(Refusal::WithdrawalAlreadySigned),
    }
}

fn parse_destination(raw: &str, wallet: &AccountAddress) -> Result<AccountAddress, Refusal> {
    if raw.is_empty() {
        return Ok(wallet.clone());
    }
    raw.parse().map_err(|_| Refusal::InvalidDestination)
}

impl<L> LedgerApi<L> {
    fn withdrawal_to_wire(&self, record: WithdrawalRecord) -> Result<Withdrawal, Status> {
        let entry = decode_entry(&record.authorization_xdr)
            .ok_or_else(|| corrupt("authorization entry"))?;
        let payload =
            signature_payload(network_id(self.network), &entry.credentials, &entry.root_invocation)
                .map_err(|_| corrupt("authorization credentials"))?;
        Ok(Withdrawal {
            withdrawal_id: record.id.to_string(),
            buyer_id: record.buyer_id.to_string(),
            amount: record.amount,
            destination: record.destination.to_string(),
            state: withdrawal_state(record.state).into(),
            authorization_entry_xdr: record.authorization_xdr,
            signature_payload: hex_lower(&payload),
            expiration_ledger: u32::try_from(record.expiration_ledger)
                .map_err(|_| corrupt("expiration ledger"))?,
            transaction_hash: record.transaction_hash.as_deref().map(hex_lower).unwrap_or_default(),
            ledger: wire_ledger(record.ledger),
            created_at: timestamp(record.created_at)?,
        })
    }

    async fn existing_withdrawal(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<WithdrawalRecord>, Status> {
        self.store.withdrawal(scope, id, key).await.map_err(|e| internal(&e))
    }

    /// The existing withdrawal under a key, if the request repeats it
    /// exactly. An empty destination repeats one to the buyer's wallet.
    fn replay_withdrawal(
        &self,
        existing: WithdrawalRecord,
        buyer_id: Uuid,
        amount: i64,
        destination: &str,
    ) -> Result<Response<PrepareWithdrawalResponse>, Status> {
        let same_destination = if destination.is_empty() {
            existing.destination == existing.wallet
        } else {
            existing.destination.as_str() == destination
        };
        if existing.buyer_id != buyer_id || existing.amount != amount || !same_destination {
            return Err(Refusal::IdempotencyConflict.into());
        }
        Ok(Response::new(PrepareWithdrawalResponse {
            withdrawal: Some(self.withdrawal_to_wire(existing)?),
            created: false,
        }))
    }
}

impl<L: LatestLedger> LedgerApi<L> {
    pub(super) async fn prepare_withdrawal_request(
        &self,
        request: Request<PrepareWithdrawalRequest>,
    ) -> Result<Response<PrepareWithdrawalResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let buyer_id = parse_id(&body.buyer_id, Refusal::InvalidBuyerId)?;
        let amount = parse_amount(body.amount)?;
        let key = parse_key(&body.idempotency_key)?;
        if let Some(existing) = self.existing_withdrawal(&scope, None, Some(&key)).await? {
            return self.replay_withdrawal(existing, buyer_id, amount, &body.destination);
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
        let destination = parse_destination(&body.destination, &wallet)?;
        if destination != wallet && !self.policy.withdrawals_to_other_accounts {
            return Err(Refusal::DestinationNotAllowed.into());
        }
        if amount < self.store.quotas.min_withdrawal {
            return Err(Refusal::WithdrawalBelowMinimum.into());
        }
        // Refused early when it cannot be covered now; the hold when the
        // signed entry is stored is what decides.
        let balance = self
            .store
            .balance(&scope, buyer_id)
            .await
            .map_err(|e| internal(&e))?
            .ok_or(Refusal::BuyerNotFound)?;
        if balance.available < amount {
            return Err(Refusal::InsufficientBalance.into());
        }
        let latest = self.ledger.latest_ledger().await.map_err(|e| network_unavailable(&e))?;
        let expiration_ledger = latest
            .checked_add(self.policy.authorization_validity_ledgers)
            .ok_or(Refusal::Internal)?;
        let intent = WithdrawIntent {
            owner: wallet.clone(),
            amount: i128::from(amount),
            destination: destination.clone(),
            withdrawal_id: random()?,
        };
        let entry = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
                address: ScAddress::Account(account_id(&wallet)),
                nonce: i64::from_le_bytes(random()?),
                signature_expiration_ledger: expiration_ledger,
                signature: ScVal::Void,
            }),
            root_invocation: deployment.owner_withdraw_authorization(&intent),
        };
        let authorization_xdr =
            entry.to_xdr_base64(Limits::none()).map_err(|_| corrupt("new authorization entry"))?;

        let inserted = self
            .store
            .insert_withdrawal(
                &scope,
                &NewWithdrawal {
                    buyer_id,
                    key: &key,
                    amount,
                    destination: &destination,
                    withdrawal_id: intent.withdrawal_id,
                    authorization_xdr: &authorization_xdr,
                    expiration_ledger,
                },
            )
            .await
            .map_err(|e| internal(&e))?;
        let id = match inserted {
            Insertion::Created(id) => {
                let to = if destination == wallet { "own" } else { "other" };
                metrics::counter!("pay_stellar_withdrawals_prepared_total", "destination" => to)
                    .increment(1);
                id
            }
            Insertion::QuotaExceeded | Insertion::DeploymentQuotaExceeded => {
                return Err(Refusal::WithdrawalQuotaExceeded.into());
            }
            // A concurrent request with the same key committed first.
            Insertion::KeyTaken => {
                let existing = self
                    .existing_withdrawal(&scope, None, Some(&key))
                    .await?
                    .ok_or(Refusal::Internal)?;
                return self.replay_withdrawal(existing, buyer_id, amount, &body.destination);
            }
        };
        let created =
            self.existing_withdrawal(&scope, Some(id), None).await?.ok_or(Refusal::Internal)?;
        Ok(Response::new(PrepareWithdrawalResponse {
            withdrawal: Some(self.withdrawal_to_wire(created)?),
            created: true,
        }))
    }

    pub(super) async fn submit_withdrawal_request(
        &self,
        request: Request<SubmitWithdrawalRequest>,
    ) -> Result<Response<SubmitWithdrawalResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let body = request.into_inner();
        let id = parse_id(&body.withdrawal_id, Refusal::InvalidWithdrawalId)?;
        let signed = decode_entry(&body.signed_authorization_entry_xdr)
            .ok_or(Refusal::InvalidAuthorizationEntry)?;
        let respond =
            |record: WithdrawalRecord| -> Result<Response<SubmitWithdrawalResponse>, Status> {
                Ok(Response::new(SubmitWithdrawalResponse {
                    withdrawal: Some(self.withdrawal_to_wire(record)?),
                }))
            };

        let record = self
            .existing_withdrawal(&scope, Some(id), None)
            .await?
            .ok_or(Refusal::WithdrawalNotFound)?;
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
        let latest = self.ledger.latest_ledger().await.map_err(|e| network_unavailable(&e))?;
        verify_signed_entry(
            &signed,
            &record.wallet,
            &prepared.root_invocation,
            network_id(self.network),
            latest,
            self.policy.authorization_validity_ledgers,
        )
        .map_err(|refusal| signature_refusal(&refusal, Refusal::WithdrawalExpired))?;

        let signed_xdr = signed
            .to_xdr_base64(Limits::none())
            .map_err(|_| corrupt("signed authorization entry"))?;
        match self.store.sign_withdrawal(&scope, id, &signed_xdr).await.map_err(|e| internal(&e))? {
            WithdrawalSigning::InsufficientBalance => {
                return Err(Refusal::InsufficientBalance.into());
            }
            WithdrawalSigning::Held | WithdrawalSigning::NotOpen => {}
        }
        // Whether this request or a concurrent one stored the signature, the
        // stored row now decides the answer.
        let record =
            self.existing_withdrawal(&scope, Some(id), None).await?.ok_or(Refusal::Internal)?;
        match signing(&record, &signed) {
            Signing::Replay => respond(record),
            Signing::Closed(refusal) => Err(refusal.into()),
            Signing::Open => Err(corrupt("withdrawal still unsigned after signing")),
        }
    }

    pub(super) async fn get_withdrawal_request(
        &self,
        request: Request<GetWithdrawalRequest>,
    ) -> Result<Response<GetWithdrawalResponse>, Status> {
        let scope = scope_of(&request)?;
        record_scope(&scope);
        let id = parse_id(&request.into_inner().withdrawal_id, Refusal::InvalidWithdrawalId)?;
        let record = self
            .existing_withdrawal(&scope, Some(id), None)
            .await?
            .ok_or(Refusal::WithdrawalNotFound)?;
        Ok(Response::new(GetWithdrawalResponse {
            withdrawal: Some(self.withdrawal_to_wire(record)?),
        }))
    }
}
