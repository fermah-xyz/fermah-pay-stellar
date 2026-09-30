//! How much of an account's XLM it can spend. An account must keep a
//! reserve of two base reserves plus one per sub-entry (trustlines, signers,
//! offers, data), counting the entries it sponsors for others and not those
//! others sponsor for it; XLM promised to open sell offers is not spendable
//! either.

use stellar_xdr::{AccountEntry, AccountEntryExt, AccountEntryExtensionV1Ext};

/// XLM, in stroops, `account` can spend above its reserve and liabilities,
/// given the network's base reserve. Negative when the account is short.
#[must_use]
pub fn spendable_stroops(account: &AccountEntry, base_reserve: u32) -> i64 {
    let (sponsoring, sponsored, selling) = match &account.ext {
        AccountEntryExt::V0 => (0, 0, 0),
        AccountEntryExt::V1(v1) => {
            let (sponsoring, sponsored) = match &v1.ext {
                AccountEntryExtensionV1Ext::V0 => (0, 0),
                AccountEntryExtensionV1Ext::V2(v2) => (v2.num_sponsoring, v2.num_sponsored),
            };
            (sponsoring, sponsored, v1.liabilities.selling)
        }
    };
    let entries =
        2 + i64::from(account.num_sub_entries) + i64::from(sponsoring) - i64::from(sponsored);
    account.balance - entries * i64::from(base_reserve) - selling
}

#[cfg(test)]
mod tests {
    use stellar_xdr::{
        AccountEntryExtensionV1, AccountEntryExtensionV2, AccountEntryExtensionV2Ext, AccountId,
        Liabilities, PublicKey, SequenceNumber, String32, Thresholds, Uint256, VecM,
    };

    use super::*;

    const RESERVE: u32 = 5_000_000;

    fn account(balance: i64, sub_entries: u32, ext: AccountEntryExt) -> AccountEntry {
        AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1; 32]))),
            balance,
            seq_num: SequenceNumber(1),
            num_sub_entries: sub_entries,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext,
        }
    }

    fn with(sponsoring: u32, sponsored: u32, selling: i64) -> AccountEntryExt {
        AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities { buying: 0, selling },
            ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsored: sponsored,
                num_sponsoring: sponsoring,
                signer_sponsoring_i_ds: VecM::default(),
                ext: AccountEntryExtensionV2Ext::V0,
            }),
        })
    }

    #[test]
    fn test_the_reserve_counts_sub_entries_and_sponsorships_and_liabilities() {
        // 100 XLM, one trustline: 3 base reserves kept.
        assert_eq!(
            spendable_stroops(&account(1_000_000_000, 1, AccountEntryExt::V0), RESERVE),
            985_000_000
        );
        // Sponsoring 40 entries for others: 40 more kept.
        assert_eq!(
            spendable_stroops(&account(1_000_000_000, 1, with(40, 0, 0)), RESERVE),
            785_000_000
        );
        // Its own trustline sponsored by someone else: that one is not kept.
        assert_eq!(
            spendable_stroops(&account(1_000_000_000, 1, with(0, 1, 0)), RESERVE),
            990_000_000
        );
        // XLM promised to a sell offer is not spendable.
        assert_eq!(
            spendable_stroops(&account(1_000_000_000, 1, with(0, 0, 5)), RESERVE),
            984_999_995
        );
        // Short of its reserve: negative.
        assert!(spendable_stroops(&account(1_000, 1, AccountEntryExt::V0), RESERVE) < 0);
    }
}
