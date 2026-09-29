//! Soroban transactions: building an invocation, applying simulated
//! resources, and wrapping the signed transaction in a fee bump.
//!
//! Three accounts take part in a sponsored call. Authorizers sign
//! authorization entries for the contract call; the inner source signs the
//! transaction and spends its sequence number; the fee source signs the outer
//! fee-bump envelope and pays. The fee-bump signature grants no contract
//! authority: an operator must still authorize its calls through its own
//! entry.

use fermah_pay_stellar_domain::{AccountAddress, Network};
use stellar_xdr::{
    ExtensionPoint, FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
    FeeBumpTransactionInnerTx, HostFunction, InvokeHostFunctionOp, Memo, Operation, OperationBody,
    Preconditions, RestoreFootprintOp, SequenceNumber, SorobanAuthorizationEntry,
    SorobanTransactionData, TimeBounds, TimePoint, Transaction, TransactionEnvelope,
    TransactionExt, TransactionSignaturePayload, TransactionSignaturePayloadTaggedTransaction,
    TransactionV1Envelope, VecM, WriteXdr,
};

use sha2::{Digest, Sha256};

use crate::keys::SecretKey;
use crate::network_id;
use crate::transaction::muxed_account;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AssemblyError {
    #[error("fee {needed} stroops exceeds the transaction fee field")]
    FeeOverflow { needed: i64 },
    #[error("an authorization list holds at most {max} entries", max = u32::MAX)]
    TooManyAuthEntries,
    #[error("the transaction already carries Soroban resources")]
    AlreadyAssembled,
    #[error("encoding transaction XDR: {0}")]
    Encode(String),
}

/// An unassembled transaction invoking `function` with `auth`, for
/// simulation. `valid_until_unix` must be finite: a bounded validity window is
/// what later lets a submitter prove the transaction can no longer land.
pub fn invocation_transaction(
    source: &AccountAddress,
    sequence: i64,
    function: HostFunction,
    auth: Vec<SorobanAuthorizationEntry>,
    inclusion_fee: u32,
    valid_until_unix: u64,
) -> Result<Transaction, AssemblyError> {
    let operation = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: function,
            auth: VecM::try_from(auth).map_err(|_| AssemblyError::TooManyAuthEntries)?,
        }),
    };
    Ok(Transaction {
        source_account: muxed_account(source),
        fee: inclusion_fee,
        seq_num: SequenceNumber(sequence),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(0),
            max_time: TimePoint(valid_until_unix),
        }),
        memo: Memo::None,
        operations: VecM::try_from(vec![operation])
            .expect("invariant: one operation fits the operation limit"),
        ext: TransactionExt::V0,
    })
}

/// An unassembled transaction restoring archived entries. Its footprint and
/// resources come from the simulation that asked for the restore, applied by
/// [`assemble`]; like every submission it carries a finite upper time bound.
#[must_use]
pub fn restore_transaction(
    source: &AccountAddress,
    sequence: i64,
    inclusion_fee: u32,
    valid_until_unix: u64,
) -> Transaction {
    let operation = Operation {
        source_account: None,
        body: OperationBody::RestoreFootprint(RestoreFootprintOp { ext: ExtensionPoint::V0 }),
    };
    Transaction {
        source_account: muxed_account(source),
        fee: inclusion_fee,
        seq_num: SequenceNumber(sequence),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(0),
            max_time: TimePoint(valid_until_unix),
        }),
        memo: Memo::None,
        operations: VecM::try_from(vec![operation])
            .expect("invariant: one operation fits the operation limit"),
        ext: TransactionExt::V0,
    }
}

/// Applies simulated resources. The resource fee is raised by
/// `margin_percent` so a ledger whose state changed slightly since
/// simulation still admits the transaction; the unused part is refunded.
pub fn assemble(
    mut tx: Transaction,
    mut data: SorobanTransactionData,
    min_resource_fee: i64,
    margin_percent: u8,
) -> Result<Transaction, AssemblyError> {
    if !matches!(tx.ext, TransactionExt::V0) {
        return Err(AssemblyError::AlreadyAssembled);
    }
    let resource_fee = min_resource_fee
        .checked_mul(100 + i64::from(margin_percent))
        .and_then(|scaled| scaled.checked_add(99))
        .map(|rounded_up| rounded_up / 100)
        .ok_or(AssemblyError::FeeOverflow { needed: i64::MAX })?;
    let total = resource_fee
        .checked_add(i64::from(tx.fee))
        .ok_or(AssemblyError::FeeOverflow { needed: i64::MAX })?;
    tx.fee = u32::try_from(total).map_err(|_| AssemblyError::FeeOverflow { needed: total })?;
    data.resource_fee = resource_fee;
    tx.ext = TransactionExt::V1(data);
    Ok(tx)
}

/// Wraps a signed transaction in a fee bump paid by `fee_source`. The outer
/// fee covers the inner transaction's resource fee plus an inclusion bid of
/// `inclusion_fee` per operation, counting the fee bump itself.
pub fn fee_bump(
    inner: TransactionV1Envelope,
    fee_source: &AccountAddress,
    inclusion_fee: u32,
) -> Result<FeeBumpTransaction, AssemblyError> {
    let resource_fee = match &inner.tx.ext {
        TransactionExt::V1(data) => data.resource_fee,
        TransactionExt::V0 => 0,
    };
    let operations = i64::try_from(inner.tx.operations.len()).unwrap_or(i64::MAX);
    let fee = operations
        .checked_add(1)
        .and_then(|n| n.checked_mul(i64::from(inclusion_fee)))
        .and_then(|inclusion| inclusion.checked_add(resource_fee))
        .ok_or(AssemblyError::FeeOverflow { needed: i64::MAX })?;
    Ok(FeeBumpTransaction {
        fee_source: muxed_account(fee_source),
        fee,
        inner_tx: FeeBumpTransactionInnerTx::Tx(inner),
        ext: FeeBumpTransactionExt::V0,
    })
}

/// The hash the network reports for a fee-bumped submission.
pub fn fee_bump_hash(tx: &FeeBumpTransaction, network: Network) -> Result<[u8; 32], AssemblyError> {
    let payload = TransactionSignaturePayload {
        network_id: stellar_xdr::Hash(network_id(network)),
        tagged_transaction: TransactionSignaturePayloadTaggedTransaction::TxFeeBump(tx.clone()),
    };
    let bytes = payload
        .to_xdr(stellar_xdr::Limits::none())
        .map_err(|e| AssemblyError::Encode(e.to_string()))?;
    Ok(Sha256::digest(bytes).into())
}

pub fn sign_fee_bump(
    tx: FeeBumpTransaction,
    network: Network,
    fee_source: &SecretKey,
) -> Result<TransactionEnvelope, AssemblyError> {
    let hash = fee_bump_hash(&tx, network)?;
    Ok(TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx,
        signatures: VecM::try_from(vec![fee_source.sign_payload(&hash)])
            .expect("invariant: one signature fits the signature limit"),
    }))
}

#[cfg(test)]
mod tests {
    use stellar_xdr::{
        ContractId, Hash, InvokeContractArgs, LedgerFootprint, ScAddress, ScSymbol,
        SorobanResources, SorobanTransactionDataExt, StringM,
    };

    use super::*;
    use crate::transaction::{self, transaction_hash};

    fn call() -> HostFunction {
        HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: ScAddress::Contract(ContractId(Hash([4; 32]))),
            function_name: ScSymbol(StringM::try_from("deposit").unwrap()),
            args: VecM::default(),
        })
    }

    fn resources(fee: i64) -> SorobanTransactionData {
        SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: VecM::default(),
                    read_write: VecM::default(),
                },
                instructions: 1_000,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: fee,
        }
    }

    fn unassembled(source: &SecretKey) -> Transaction {
        invocation_transaction(&source.address(), 7, call(), vec![], 100, 2_000_000_000).unwrap()
    }

    #[test]
    fn test_assemble_adds_margin_to_resource_fee_and_total() {
        let source = SecretKey::generate().unwrap();
        let tx = assemble(unassembled(&source), resources(0), 10_000, 15).unwrap();
        let TransactionExt::V1(data) = &tx.ext else { panic!("not assembled") };
        assert_eq!((data.resource_fee, tx.fee), (11_500, 11_600));
    }

    #[test]
    fn test_assemble_rounds_margin_up() {
        let source = SecretKey::generate().unwrap();
        let tx = assemble(unassembled(&source), resources(0), 101, 15).unwrap();
        let TransactionExt::V1(data) = &tx.ext else { panic!("not assembled") };
        assert_eq!(data.resource_fee, 117);
    }

    #[test]
    fn test_assemble_refuses_fee_beyond_u32() {
        let source = SecretKey::generate().unwrap();
        let result = assemble(unassembled(&source), resources(0), i64::from(u32::MAX), 0);
        assert!(matches!(result, Err(AssemblyError::FeeOverflow { .. })));
    }

    #[test]
    fn test_assemble_twice_is_refused() {
        let source = SecretKey::generate().unwrap();
        let once = assemble(unassembled(&source), resources(0), 100, 0).unwrap();
        assert_eq!(assemble(once, resources(0), 100, 0), Err(AssemblyError::AlreadyAssembled));
    }

    #[test]
    fn test_fee_bump_covers_resource_fee_and_both_inclusion_bids() {
        let (source, sponsor) = (SecretKey::generate().unwrap(), SecretKey::generate().unwrap());
        let tx = assemble(unassembled(&source), resources(0), 10_000, 0).unwrap();
        let TransactionEnvelope::Tx(inner) =
            transaction::sign(tx, Network::Testnet, &[&source]).unwrap()
        else {
            panic!("expected a v1 envelope")
        };
        let bump = fee_bump(inner, &sponsor.address(), 100).unwrap();
        assert_eq!((bump.fee, bump.fee_source), (10_200, muxed_account(&sponsor.address())));
    }

    #[test]
    fn test_outer_hash_differs_from_inner_hash() {
        let (source, sponsor) = (SecretKey::generate().unwrap(), SecretKey::generate().unwrap());
        let tx = assemble(unassembled(&source), resources(0), 10_000, 0).unwrap();
        let inner_hash = transaction_hash(&tx, Network::Testnet).unwrap();
        let TransactionEnvelope::Tx(inner) =
            transaction::sign(tx, Network::Testnet, &[&source]).unwrap()
        else {
            panic!("expected a v1 envelope")
        };
        let bump = fee_bump(inner, &sponsor.address(), 100).unwrap();
        assert_ne!(fee_bump_hash(&bump, Network::Testnet).unwrap(), inner_hash);
    }
}
