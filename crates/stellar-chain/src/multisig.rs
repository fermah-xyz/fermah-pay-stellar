//! Accounts that need several signatures. A Stellar account lists extra
//! Ed25519 signers with weights next to its master key, and each operation
//! class requires its total weight to reach a threshold. A contract's
//! `require_auth` on an account needs the medium threshold (the same as a
//! payment), so an account whose medium threshold is above any single key's
//! weight can only authorize with several signatures.

use fermah_pay_stellar_domain::AccountAddress;
use stellar_xdr::{
    BeginSponsoringFutureReservesOp, LedgerEntryData, LedgerKey, LedgerKeyAccount, Memo, Operation,
    OperationBody, Preconditions, SequenceNumber, SetOptionsOp, Signer, SignerKey, TimeBounds,
    TimePoint, Transaction, TransactionExt, Uint256, VecM,
};

use crate::rpc::{RpcClient, RpcError};
use crate::transaction::{account_id, muxed_account};

/// An account's signers and thresholds, as the ledger holds them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountSigners {
    pub account: AccountAddress,
    pub master_weight: u8,
    /// Ed25519 signers other than the master key, with their weights.
    pub signers: Vec<(AccountAddress, u32)>,
    pub low: u8,
    pub medium: u8,
    pub high: u8,
}

impl AccountSigners {
    /// Decodes an account entry; `None` for any other entry.
    #[must_use]
    pub fn from_entry(entry: &LedgerEntryData) -> Option<Self> {
        let LedgerEntryData::Account(account) = entry else { return None };
        let [master_weight, low, medium, high] = account.thresholds.0;
        let signers = account
            .signers
            .iter()
            .filter_map(|signer| match &signer.key {
                SignerKey::Ed25519(key) => {
                    Some((AccountAddress::from_public_key(key.0), signer.weight))
                }
                // Pre-authorized transactions, hash preimages and signed
                // payloads never sign a contract authorization.
                _ => None,
            })
            .collect();
        Some(Self {
            account: crate::transaction::address_of(&account.account_id),
            master_weight,
            signers,
            low,
            medium,
            high,
        })
    }

    /// Total weight of `keys` on this account; unknown keys weigh nothing
    /// and each key counts once.
    #[must_use]
    pub fn weight_of(&self, keys: &[AccountAddress]) -> u32 {
        let mut counted: Vec<&AccountAddress> = Vec::new();
        let mut total = 0_u32;
        for key in keys {
            if counted.contains(&key) {
                continue;
            }
            counted.push(key);
            total = total.saturating_add(if *key == self.account {
                u32::from(self.master_weight)
            } else {
                self.signers.iter().find(|(signer, _)| signer == key).map_or(0, |(_, w)| *w)
            });
        }
        total
    }

    /// Whether `keys` together may authorize a contract call or a payment.
    #[must_use]
    pub fn meets_medium(&self, keys: &[AccountAddress]) -> bool {
        // A zero threshold still needs a signer with some weight.
        self.weight_of(keys) >= u32::from(self.medium).max(1)
    }
}

/// A signing policy to give an account: extra signers and their weights,
/// the master key's weight, and the thresholds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub signers: Vec<(AccountAddress, u32)>,
    pub master_weight: u8,
    pub low: u8,
    pub medium: u8,
    pub high: u8,
}

/// A transaction giving `account` the signers and thresholds of `policy`.
/// `sponsor` is the transaction source: it pays the fee and, through the
/// sponsorship sandwich, the reserve each signer adds, so the account can
/// hold no XLM. The account's current signers must sign it (the high
/// threshold), and so must the sponsor.
#[must_use]
pub fn set_signers_transaction(
    sponsor: &AccountAddress,
    sponsor_next_sequence: i64,
    account: &AccountAddress,
    policy: &Policy,
    fee_per_operation: u32,
    valid_until_unix: u64,
) -> Transaction {
    let as_account = Some(muxed_account(account));
    let mut operations = vec![Operation {
        source_account: None,
        body: OperationBody::BeginSponsoringFutureReserves(BeginSponsoringFutureReservesOp {
            sponsored_id: account_id(account),
        }),
    }];
    for (signer, weight) in &policy.signers {
        operations.push(Operation {
            source_account: as_account.clone(),
            body: OperationBody::SetOptions(SetOptionsOp {
                signer: Some(Signer {
                    key: SignerKey::Ed25519(Uint256(*signer.public_key())),
                    weight: *weight,
                }),
                ..empty_set_options()
            }),
        });
    }
    // Thresholds last: raising them first could leave the remaining
    // operations short of the weight they need.
    operations.push(Operation {
        source_account: as_account.clone(),
        body: OperationBody::SetOptions(SetOptionsOp {
            master_weight: Some(u32::from(policy.master_weight)),
            low_threshold: Some(u32::from(policy.low)),
            med_threshold: Some(u32::from(policy.medium)),
            high_threshold: Some(u32::from(policy.high)),
            ..empty_set_options()
        }),
    });
    operations.push(Operation {
        source_account: as_account,
        body: OperationBody::EndSponsoringFutureReserves,
    });
    let count = u32::try_from(operations.len()).unwrap_or(u32::MAX);
    Transaction {
        source_account: muxed_account(sponsor),
        fee: fee_per_operation.saturating_mul(count),
        seq_num: SequenceNumber(sponsor_next_sequence),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(0),
            max_time: TimePoint(valid_until_unix),
        }),
        memo: Memo::None,
        operations: VecM::try_from(operations)
            .expect("invariant: a handful of signers fits the operation limit"),
        ext: TransactionExt::V0,
    }
}

const fn empty_set_options() -> SetOptionsOp {
    SetOptionsOp {
        inflation_dest: None,
        clear_flags: None,
        set_flags: None,
        master_weight: None,
        low_threshold: None,
        med_threshold: None,
        high_threshold: None,
        home_domain: None,
        signer: None,
    }
}

/// The signers and thresholds of `account`, or `None` if it does not exist.
pub async fn account_signers(
    rpc: &RpcClient,
    account: &AccountAddress,
) -> Result<Option<AccountSigners>, RpcError> {
    let key = LedgerKey::Account(LedgerKeyAccount { account_id: account_id(account) });
    let entries = rpc.get_ledger_entries(&[key]).await?;
    Ok(entries.first().and_then(|entry| AccountSigners::from_entry(&entry.data)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> AccountAddress {
        AccountAddress::from_public_key([seed; 32])
    }

    /// Two of three: the master key and two signers weigh one each, and a
    /// contract call needs two.
    fn two_of_three() -> AccountSigners {
        AccountSigners {
            account: key(1),
            master_weight: 1,
            signers: vec![(key(2), 1), (key(3), 1)],
            low: 1,
            medium: 2,
            high: 2,
        }
    }

    #[test]
    fn test_two_distinct_signers_meet_the_medium_threshold_and_one_does_not() {
        let policy = two_of_three();
        assert!(policy.meets_medium(&[key(1), key(3)]));
        assert!(policy.meets_medium(&[key(2), key(3)]));
        assert!(!policy.meets_medium(&[key(2)]));
        // The same key twice counts once; a stranger counts nothing.
        assert!(!policy.meets_medium(&[key(2), key(2)]));
        assert!(!policy.meets_medium(&[key(2), key(9)]));
    }

    #[test]
    fn test_signers_are_added_before_the_thresholds_rise_inside_a_sponsorship() {
        let policy = Policy {
            signers: vec![(key(2), 1), (key(3), 1)],
            master_weight: 1,
            low: 1,
            medium: 2,
            high: 2,
        };
        let tx = set_signers_transaction(&key(9), 5, &key(1), &policy, 100, 1_000);
        let kinds: Vec<&str> = tx.operations.iter().map(|op| op.body.name()).collect();
        assert_eq!(
            kinds,
            [
                "BeginSponsoringFutureReserves",
                "SetOptions",
                "SetOptions",
                "SetOptions",
                "EndSponsoringFutureReserves"
            ]
        );
        assert_eq!(tx.fee, 500);
        let OperationBody::SetOptions(last) = &tx.operations[3].body else { panic!() };
        assert_eq!((last.med_threshold, last.high_threshold), (Some(2), Some(2)));
        assert!(
            tx.operations[1..].iter().all(|op| op.source_account == Some(muxed_account(&key(1))))
        );
    }

    #[test]
    fn test_a_disabled_master_key_weighs_nothing() {
        let policy = AccountSigners { master_weight: 0, ..two_of_three() };
        assert!(!policy.meets_medium(&[key(1), key(2)]));
        assert!(policy.meets_medium(&[key(2), key(3)]));
    }
}
