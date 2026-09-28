use fermah_pay_stellar_domain::Network;
use uuid::Uuid;

/// The tenancy scope an authenticated caller acts within.
///
/// Only [`crate::auth`] constructs a `Scope`, and only from a live API key,
/// so a request handler cannot obtain one without authentication succeeding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    product_id: Uuid,
    seller_deployment_id: Uuid,
    network: Network,
}

impl Scope {
    pub(crate) const fn new(
        product_id: Uuid,
        seller_deployment_id: Uuid,
        network: Network,
    ) -> Self {
        Self { product_id, seller_deployment_id, network }
    }

    #[must_use]
    pub const fn product_id(&self) -> Uuid {
        self.product_id
    }

    #[must_use]
    pub const fn seller_deployment_id(&self) -> Uuid {
        self.seller_deployment_id
    }

    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }
}
