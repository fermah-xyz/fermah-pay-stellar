//! Classic asset payments from one account to many, paid by another.

use fermah_pay_stellar_domain::AccountAddress;
use stellar_xdr::{
    Asset, Memo, Operation, OperationBody, PaymentOp, Preconditions, SequenceNumber, TimeBounds,
    TimePoint, Transaction, TransactionExt, VecM,
};

use crate::transaction::muxed_account;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("between 1 and 100 payments fit one transaction, got {0}")]
pub struct PaymentCountError(pub usize);

/// Payments of `asset` from `from` to each recipient, in a transaction whose
/// source (and fee payer) is `fee_payer`. Both must sign; `from` needs no XLM.
pub fn payments_transaction(
    fee_payer: &AccountAddress,
    fee_payer_next_sequence: i64,
    from: &AccountAddress,
    recipients: &[(AccountAddress, i64)],
    asset: &Asset,
    fee_per_operation: u32,
    valid_until_unix: u64,
) -> Result<Transaction, PaymentCountError> {
    if recipients.is_empty() || recipients.len() > 100 {
        return Err(PaymentCountError(recipients.len()));
    }
    let operations: Vec<Operation> = recipients
        .iter()
        .map(|(to, amount)| Operation {
            source_account: Some(muxed_account(from)),
            body: OperationBody::Payment(PaymentOp {
                destination: muxed_account(to),
                asset: asset.clone(),
                amount: *amount,
            }),
        })
        .collect();
    let count = u32::try_from(operations.len()).expect("invariant: at most 100 operations");
    Ok(Transaction {
        source_account: muxed_account(fee_payer),
        fee: fee_per_operation.saturating_mul(count),
        seq_num: SequenceNumber(fee_payer_next_sequence),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(0),
            max_time: TimePoint(valid_until_unix),
        }),
        memo: Memo::None,
        operations: VecM::try_from(operations).expect("invariant: at most 100 operations"),
        ext: TransactionExt::V0,
    })
}

#[cfg(test)]
mod tests {
    use fermah_pay_stellar_domain::Network;

    use super::*;
    use crate::keys::SecretKey;
    use crate::usdc::circle_usdc;

    fn address() -> AccountAddress {
        SecretKey::generate().unwrap().address()
    }

    #[test]
    fn test_payments_come_from_sender_while_fee_payer_is_source() {
        let (payer, from, to) = (address(), address(), address());
        let tx = payments_transaction(
            &payer,
            1,
            &from,
            &[(to, 5)],
            &circle_usdc(Network::Testnet),
            100,
            1,
        )
        .unwrap();
        assert_eq!(
            (tx.source_account.clone(), tx.operations[0].source_account.clone()),
            (muxed_account(&payer), Some(muxed_account(&from)))
        );
    }

    #[test]
    fn test_more_than_one_hundred_payments_are_refused() {
        let recipients: Vec<_> = (0..101).map(|_| (address(), 1)).collect();
        let result = payments_transaction(
            &address(),
            1,
            &address(),
            &recipients,
            &circle_usdc(Network::Testnet),
            100,
            1,
        );
        assert_eq!(result, Err(PaymentCountError(101)));
    }
}
