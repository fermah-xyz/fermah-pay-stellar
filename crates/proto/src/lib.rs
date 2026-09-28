//! Wire types and gRPC stubs generated from `proto/`.

pub mod v1 {
    #![allow(clippy::all, clippy::pedantic, missing_docs)]
    tonic::include_proto!("fermah.pay.stellar.v1");
}
