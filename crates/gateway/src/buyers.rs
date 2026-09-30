//! `BuyerService` gRPC handlers: parse at the edge, act within the caller's
//! scope, map every outcome to one stable refusal.

use fermah_pay_stellar_domain::{AccountAddress, AccountAddressError, ExternalRef};
use fermah_pay_stellar_proto::v1::buyer_service_server::BuyerService;
use fermah_pay_stellar_proto::v1::get_buyer_request::Lookup;
use fermah_pay_stellar_proto::v1::{
    Buyer, CreateBuyerRequest, CreateBuyerResponse, GetBuyerRequest, GetBuyerResponse,
};
use time::format_description::well_known::Rfc3339;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::auth::scope_of;
use crate::refusal::Refusal;
use crate::store::{BuyerLookup, BuyerRecord, CreateBuyerOutcome, Store, StoreError};

#[derive(Clone, Debug)]
pub struct BuyerApi {
    store: Store,
}

impl BuyerApi {
    #[must_use]
    pub const fn new(store: Store) -> Self {
        Self { store }
    }
}

fn to_wire(record: BuyerRecord) -> Result<Buyer, Status> {
    let created_at = record.created_at.format(&Rfc3339).map_err(|error| {
        tracing::error!(error = %error, "formatting buyer timestamp");
        Status::from(Refusal::Internal)
    })?;
    Ok(Buyer {
        buyer_id: record.id.to_string(),
        external_ref: record.external_ref,
        wallet_address: record.wallet_address,
        network: record.network.caip2().to_owned(),
        created_at,
    })
}

fn internal(error: &StoreError) -> Status {
    tracing::error!(error = %error, source = ?std::error::Error::source(error), "store failure");
    Refusal::Internal.into()
}

#[tonic::async_trait]
impl BuyerService for BuyerApi {
    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn create_buyer(
        &self,
        request: Request<CreateBuyerRequest>,
    ) -> Result<Response<CreateBuyerResponse>, Status> {
        let scope = scope_of(&request)?;
        tracing::Span::current()
            .record("seller_deployment_id", tracing::field::display(scope.seller_deployment_id()));
        let body = request.into_inner();
        let external_ref: ExternalRef =
            body.external_ref.parse().map_err(|_| Refusal::InvalidExternalRef)?;
        let wallet: AccountAddress = body.wallet_address.parse().map_err(|e| match e {
            AccountAddressError::Malformed => Refusal::InvalidWalletAddress,
            AccountAddressError::UnsupportedMuxed | AccountAddressError::UnsupportedContract => {
                Refusal::UnsupportedWalletAddress
            }
        })?;
        let (record, created) = match self
            .store
            .create_buyer(&scope, &external_ref, &wallet)
            .await
            .map_err(|e| internal(&e))?
        {
            CreateBuyerOutcome::Created(record) => (record, true),
            CreateBuyerOutcome::Existing(record) => (record, false),
            CreateBuyerOutcome::Conflict => return Err(Refusal::BuyerConflict.into()),
            CreateBuyerOutcome::QuotaExceeded => return Err(Refusal::BuyerQuotaExceeded.into()),
        };
        Ok(Response::new(CreateBuyerResponse { buyer: Some(to_wire(record)?), created }))
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id))]
    async fn get_buyer(
        &self,
        request: Request<GetBuyerRequest>,
    ) -> Result<Response<GetBuyerResponse>, Status> {
        let scope = scope_of(&request)?;
        tracing::Span::current()
            .record("seller_deployment_id", tracing::field::display(scope.seller_deployment_id()));
        let external_ref;
        let lookup = match request.into_inner().lookup {
            Some(Lookup::BuyerId(raw)) => {
                BuyerLookup::Id(Uuid::parse_str(&raw).map_err(|_| Refusal::InvalidBuyerId)?)
            }
            Some(Lookup::ExternalRef(raw)) => {
                external_ref =
                    raw.parse::<ExternalRef>().map_err(|_| Refusal::InvalidExternalRef)?;
                BuyerLookup::ExternalRef(&external_ref)
            }
            None => return Err(Refusal::MissingLookup.into()),
        };
        let record = self
            .store
            .buyer(&scope, lookup)
            .await
            .map_err(|e| internal(&e))?
            .ok_or(Refusal::BuyerNotFound)?;
        Ok(Response::new(GetBuyerResponse { buyer: Some(to_wire(record)?) }))
    }
}
