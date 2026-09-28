//! Buyer account onboarding with sponsored reserves.
//!
//! A new `G...` account needs XLM for its base reserve and one more reserve
//! for its USDC trustline. Paying a transaction fee on the buyer's behalf is
//! a different thing from paying those reserves. This module builds the one
//! transaction that creates the buyer with a zero XLM balance while a sponsor
//! pays the fee and both reserves, and reads back from the ledger who
//! actually pays each reserve, so evidence never rests on the builder's
//! intent.

use std::time::Duration;

use fermah_pay_stellar_domain::{AccountAddress, Network};
use stellar_xdr::{
    Asset, BeginSponsoringFutureReservesOp, ChangeTrustAsset, ChangeTrustOp, CreateAccountOp,
    LedgerEntryData, LedgerEntryExt, LedgerKey, LedgerKeyAccount, LedgerKeyTrustLine, Memo,
    Operation, OperationBody, Preconditions, SequenceNumber, TimeBounds, TimePoint, Transaction,
    TransactionExt, TrustLineAsset, VecM,
};

use crate::keys::SecretKey;
use crate::rpc::{RpcClient, RpcError};
use crate::submission::{SubmissionError, submit_and_wait};
use crate::transaction::{self, SigningError, account_id, address_of, muxed_account};

/// Who funds one reserve of the buyer's ledger entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReservePayer {
    Buyer,
    Sponsor(AccountAddress),
}

/// What the ledger says about the buyer's XLM and reserves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReserveProvenance {
    pub buyer: AccountAddress,
    pub native_balance_stroops: i64,
    pub account_reserve: ReservePayer,
    /// `None` when the buyer has no trustline for the asset.
    pub trustline_reserve: Option<ReservePayer>,
    pub trustline_balance: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SponsorshipViolation {
    #[error("buyer holds {stroops} stroops of XLM")]
    BuyerHoldsXlm { stroops: i64 },
    #[error("buyer account base reserve is paid by {payer:?}")]
    AccountReserve { payer: ReservePayer },
    #[error("buyer has no trustline for the asset")]
    TrustlineMissing,
    #[error("buyer trustline reserve is paid by {payer:?}")]
    TrustlineReserve { payer: ReservePayer },
}

impl ReserveProvenance {
    /// Every way the observed state differs from "the buyer holds no XLM and
    /// `sponsor` pays both reserves". Empty means the property holds.
    #[must_use]
    pub fn violations_for_sponsor(&self, sponsor: &AccountAddress) -> Vec<SponsorshipViolation> {
        let expected = ReservePayer::Sponsor(sponsor.clone());
        let mut violations = Vec::new();
        if self.native_balance_stroops != 0 {
            violations
                .push(SponsorshipViolation::BuyerHoldsXlm { stroops: self.native_balance_stroops });
        }
        if self.account_reserve != expected {
            violations
                .push(SponsorshipViolation::AccountReserve { payer: self.account_reserve.clone() });
        }
        match &self.trustline_reserve {
            None => violations.push(SponsorshipViolation::TrustlineMissing),
            Some(payer) if *payer != expected => {
                violations.push(SponsorshipViolation::TrustlineReserve { payer: payer.clone() });
            }
            Some(_) => {}
        }
        violations
    }
}

/// Ledger keys whose entries establish [`ReserveProvenance`].
#[must_use]
pub fn provenance_keys(buyer: &AccountAddress, asset: &Asset) -> [LedgerKey; 2] {
    [
        LedgerKey::Account(LedgerKeyAccount { account_id: account_id(buyer) }),
        LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: account_id(buyer),
            asset: trustline_asset(asset),
        }),
    ]
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProvenanceError {
    #[error("buyer account does not exist on the ledger")]
    AccountMissing,
}

/// Classifies reserve payers from ledger entries. `entries` may contain
/// unrelated records; only the buyer's account and `asset` trustline count.
pub fn reserve_provenance<'a>(
    buyer: &AccountAddress,
    asset: &Asset,
    entries: impl IntoIterator<Item = (&'a LedgerEntryData, &'a LedgerEntryExt)>,
) -> Result<ReserveProvenance, ProvenanceError> {
    let wanted_trustline = trustline_asset(asset);
    let mut account = None;
    let mut trustline = None;
    for (data, ext) in entries {
        match data {
            LedgerEntryData::Account(entry) if address_of(&entry.account_id) == *buyer => {
                account = Some((entry.balance, payer_of(ext)));
            }
            LedgerEntryData::Trustline(entry)
                if address_of(&entry.account_id) == *buyer && entry.asset == wanted_trustline =>
            {
                trustline = Some((entry.balance, payer_of(ext)));
            }
            _ => {}
        }
    }
    let (native_balance_stroops, account_reserve) =
        account.ok_or(ProvenanceError::AccountMissing)?;
    Ok(ReserveProvenance {
        buyer: buyer.clone(),
        native_balance_stroops,
        account_reserve,
        trustline_balance: trustline.as_ref().map(|(balance, _)| *balance),
        trustline_reserve: trustline.map(|(_, payer)| payer),
    })
}

fn payer_of(ext: &LedgerEntryExt) -> ReservePayer {
    match ext {
        LedgerEntryExt::V1(v1) => match &v1.sponsoring_id.0 {
            Some(sponsor) => ReservePayer::Sponsor(address_of(sponsor)),
            None => ReservePayer::Buyer,
        },
        LedgerEntryExt::V0 => ReservePayer::Buyer,
    }
}

fn trustline_asset(asset: &Asset) -> TrustLineAsset {
    match asset {
        Asset::Native => TrustLineAsset::Native,
        Asset::CreditAlphanum4(a) => TrustLineAsset::CreditAlphanum4(a.clone()),
        Asset::CreditAlphanum12(a) => TrustLineAsset::CreditAlphanum12(a.clone()),
    }
}

fn change_trust_asset(asset: &Asset) -> ChangeTrustAsset {
    match asset {
        Asset::Native => ChangeTrustAsset::Native,
        Asset::CreditAlphanum4(a) => ChangeTrustAsset::CreditAlphanum4(a.clone()),
        Asset::CreditAlphanum12(a) => ChangeTrustAsset::CreditAlphanum12(a.clone()),
    }
}

/// The sponsored onboarding transaction. The sponsor is the transaction
/// source, so it pays the fee and consumes the sequence number; the
/// `BeginSponsoring`/`EndSponsoring` sandwich makes it pay the reserves of the
/// account and trustline created in between. Both sponsor and buyer must
/// sign: the buyer's signature is its consent to being sponsored and to
/// holding the trustline.
#[must_use]
pub fn sponsored_onboarding_transaction(
    sponsor: &AccountAddress,
    sponsor_next_sequence: i64,
    buyer: &AccountAddress,
    asset: &Asset,
    fee_stroops: u32,
    valid_until_unix: u64,
) -> Transaction {
    let mut tx = sponsored_onboarding_batch_transaction(
        sponsor,
        sponsor_next_sequence,
        std::slice::from_ref(buyer),
        asset,
        0,
        valid_until_unix,
    )
    .expect("invariant: one buyer fits a batch");
    tx.fee = fee_stroops;
    tx
}

/// A transaction signs with at most 20 keys: the sponsor and 19 buyers.
pub const MAX_BUYERS_PER_TRANSACTION: usize = 19;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "between 1 and {MAX_BUYERS_PER_TRANSACTION} buyers fit one onboarding transaction, got {0}"
)]
pub struct BatchSizeError(pub usize);

/// Onboards several buyers in one transaction: one sponsorship sandwich per
/// buyer, all paid by `sponsor`. The fee is `fee_per_operation` for each of
/// the four operations per buyer.
pub fn sponsored_onboarding_batch_transaction(
    sponsor: &AccountAddress,
    sponsor_next_sequence: i64,
    buyers: &[AccountAddress],
    asset: &Asset,
    fee_per_operation: u32,
    valid_until_unix: u64,
) -> Result<Transaction, BatchSizeError> {
    if buyers.is_empty() || buyers.len() > MAX_BUYERS_PER_TRANSACTION {
        return Err(BatchSizeError(buyers.len()));
    }
    let operations: Vec<Operation> =
        buyers.iter().flat_map(|buyer| sponsorship_operations(buyer, asset)).collect();
    let count = u32::try_from(operations.len()).expect("invariant: at most 76 operations");
    Ok(Transaction {
        source_account: muxed_account(sponsor),
        fee: fee_per_operation.saturating_mul(count),
        seq_num: SequenceNumber(sponsor_next_sequence),
        // A finite upper bound gives a submitter a point after which a
        // not-yet-included envelope can never land.
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(0),
            max_time: TimePoint(valid_until_unix),
        }),
        memo: Memo::None,
        operations: VecM::try_from(operations)
            .expect("invariant: 19 buyers need 76 operations, within the 100-operation limit"),
        ext: TransactionExt::V0,
    })
}

fn sponsorship_operations(buyer: &AccountAddress, asset: &Asset) -> [Operation; 4] {
    let as_buyer = Some(muxed_account(buyer));
    [
        Operation {
            source_account: None,
            body: OperationBody::BeginSponsoringFutureReserves(BeginSponsoringFutureReservesOp {
                sponsored_id: account_id(buyer),
            }),
        },
        Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: account_id(buyer),
                starting_balance: 0,
            }),
        },
        Operation {
            source_account: as_buyer.clone(),
            body: OperationBody::ChangeTrust(ChangeTrustOp {
                line: change_trust_asset(asset),
                limit: i64::MAX,
            }),
        },
        Operation { source_account: as_buyer, body: OperationBody::EndSponsoringFutureReserves },
    ]
}

#[derive(Debug)]
pub struct OnboardingReceipt {
    pub transaction_hash: [u8; 32],
    pub ledger: u32,
    pub fee_charged_stroops: i64,
    /// Account whose XLM paid the fee: the transaction source.
    pub fee_source: AccountAddress,
    pub provenance: ReserveProvenance,
}

#[derive(Debug, thiserror::Error)]
pub enum OnboardingError {
    #[error("reading the sponsor account")]
    SponsorLookup(#[source] RpcError),
    #[error("sponsor account {0} does not exist")]
    SponsorMissing(AccountAddress),
    #[error("signing the onboarding transaction")]
    Signing(#[source] SigningError),
    #[error(transparent)]
    BatchSize(BatchSizeError),
    #[error("submitting the onboarding transaction")]
    Submission(#[source] SubmissionError),
    #[error("reading buyer entries after inclusion")]
    ProvenanceLookup(#[source] RpcError),
    #[error("classifying buyer entries after inclusion")]
    Provenance(#[source] ProvenanceError),
}

/// Parameters the caller controls; defaults suit a lightly loaded network.
#[derive(Clone, Copy, Debug)]
pub struct SubmissionPolicy {
    pub fee_stroops: u32,
    pub validity: Duration,
    pub poll_interval: Duration,
}

/// Creates `buyer` with sponsored reserves and a trustline to `asset`, then
/// reads the resulting ledger state.
pub async fn onboard_buyer(
    rpc: &RpcClient,
    network: Network,
    sponsor: &SecretKey,
    buyer: &SecretKey,
    asset: &Asset,
    policy: SubmissionPolicy,
) -> Result<OnboardingReceipt, OnboardingError> {
    let sponsor_address = sponsor.address();
    let buyer_address = buyer.address();
    let sequence = current_sequence(rpc, &sponsor_address).await?;
    let valid_until = crate::submission::unix_now().saturating_add(policy.validity.as_secs());
    let tx = sponsored_onboarding_transaction(
        &sponsor_address,
        sequence + 1,
        &buyer_address,
        asset,
        policy.fee_stroops,
        valid_until,
    );
    let hash = transaction::transaction_hash(&tx, network).map_err(OnboardingError::Signing)?;
    let envelope =
        transaction::sign(tx, network, &[sponsor, buyer]).map_err(OnboardingError::Signing)?;
    let included = submit_and_wait(rpc, &envelope, hash, valid_until, policy.poll_interval)
        .await
        .map_err(OnboardingError::Submission)?;

    let keys = provenance_keys(&buyer_address, asset);
    let records = rpc.get_ledger_entries(&keys).await.map_err(OnboardingError::ProvenanceLookup)?;
    let provenance = reserve_provenance(
        &buyer_address,
        asset,
        records.iter().map(|record| (&record.data, &record.ext)),
    )
    .map_err(OnboardingError::Provenance)?;

    Ok(OnboardingReceipt {
        transaction_hash: included.envelope_hash,
        ledger: included.transaction.ledger,
        fee_charged_stroops: included.transaction.result.fee_charged,
        fee_source: sponsor_address,
        provenance,
    })
}

/// Result of onboarding several buyers in one transaction.
#[derive(Debug)]
pub struct BatchOnboardingReceipt {
    pub transaction_hash: [u8; 32],
    pub ledger: u32,
    pub fee_charged_stroops: i64,
    pub fee_source: AccountAddress,
    pub provenance: Vec<ReserveProvenance>,
}

/// Creates up to [`MAX_BUYERS_PER_TRANSACTION`] buyers in one transaction and
/// reads each one's reserve provenance back from the ledger.
pub async fn onboard_buyers(
    rpc: &RpcClient,
    network: Network,
    sponsor: &SecretKey,
    buyers: &[&SecretKey],
    asset: &Asset,
    policy: SubmissionPolicy,
) -> Result<BatchOnboardingReceipt, OnboardingError> {
    let sponsor_address = sponsor.address();
    let addresses: Vec<AccountAddress> = buyers.iter().map(|b| b.address()).collect();
    let sequence = current_sequence(rpc, &sponsor_address).await?;
    let valid_until = crate::submission::unix_now().saturating_add(policy.validity.as_secs());
    let tx = sponsored_onboarding_batch_transaction(
        &sponsor_address,
        sequence + 1,
        &addresses,
        asset,
        policy.fee_stroops,
        valid_until,
    )
    .map_err(OnboardingError::BatchSize)?;
    let hash = transaction::transaction_hash(&tx, network).map_err(OnboardingError::Signing)?;
    let mut signers: Vec<&SecretKey> = vec![sponsor];
    signers.extend_from_slice(buyers);
    let envelope = transaction::sign(tx, network, &signers).map_err(OnboardingError::Signing)?;
    let included = submit_and_wait(rpc, &envelope, hash, valid_until, policy.poll_interval)
        .await
        .map_err(OnboardingError::Submission)?;

    let mut provenance = Vec::with_capacity(addresses.len());
    for buyer in &addresses {
        let records = rpc
            .get_ledger_entries(&provenance_keys(buyer, asset))
            .await
            .map_err(OnboardingError::ProvenanceLookup)?;
        provenance.push(
            reserve_provenance(buyer, asset, records.iter().map(|r| (&r.data, &r.ext)))
                .map_err(OnboardingError::Provenance)?,
        );
    }
    Ok(BatchOnboardingReceipt {
        transaction_hash: included.envelope_hash,
        ledger: included.transaction.ledger,
        fee_charged_stroops: included.transaction.result.fee_charged,
        fee_source: sponsor_address,
        provenance,
    })
}

async fn current_sequence(
    rpc: &RpcClient,
    account: &AccountAddress,
) -> Result<i64, OnboardingError> {
    let key = LedgerKey::Account(LedgerKeyAccount { account_id: account_id(account) });
    let records = rpc.get_ledger_entries(&[key]).await.map_err(OnboardingError::SponsorLookup)?;
    records
        .iter()
        .find_map(|record| match &record.data {
            LedgerEntryData::Account(entry) => Some(entry.seq_num.0),
            _ => None,
        })
        .ok_or_else(|| OnboardingError::SponsorMissing(account.clone()))
}

#[cfg(test)]
mod tests {
    use stellar_xdr::{
        AccountEntry, AccountEntryExt, AccountId, LedgerEntryExtensionV1,
        LedgerEntryExtensionV1Ext, SponsorshipDescriptor, String32, Thresholds, TrustLineEntry,
        TrustLineEntryExt,
    };

    use super::*;
    use crate::usdc::circle_usdc;

    // Synthetic ledger entries: unit-test fixtures, not network evidence.

    fn address() -> AccountAddress {
        SecretKey::generate().unwrap().address()
    }

    fn sponsored_by(sponsor: &AccountAddress) -> LedgerEntryExt {
        LedgerEntryExt::V1(LedgerEntryExtensionV1 {
            sponsoring_id: SponsorshipDescriptor(Some(account_id(sponsor))),
            ext: LedgerEntryExtensionV1Ext::V0,
        })
    }

    fn account_entry(owner: &AccountAddress, balance: i64) -> LedgerEntryData {
        LedgerEntryData::Account(AccountEntry {
            account_id: account_id(owner),
            balance,
            seq_num: SequenceNumber(0),
            num_sub_entries: 1,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        })
    }

    fn trustline_entry(owner: &AccountAddress, asset: &Asset) -> LedgerEntryData {
        LedgerEntryData::Trustline(TrustLineEntry {
            account_id: account_id(owner),
            asset: trustline_asset(asset),
            balance: 0,
            limit: i64::MAX,
            flags: 1,
            ext: TrustLineEntryExt::V0,
        })
    }

    fn classify(
        buyer: &AccountAddress,
        entries: &[(LedgerEntryData, LedgerEntryExt)],
    ) -> Result<ReserveProvenance, ProvenanceError> {
        reserve_provenance(
            buyer,
            &circle_usdc(Network::Testnet),
            entries.iter().map(|(data, ext)| (data, ext)),
        )
    }

    fn fully_sponsored(
        buyer: &AccountAddress,
        sponsor: &AccountAddress,
    ) -> Vec<(LedgerEntryData, LedgerEntryExt)> {
        let usdc = circle_usdc(Network::Testnet);
        vec![
            (account_entry(buyer, 0), sponsored_by(sponsor)),
            (trustline_entry(buyer, &usdc), sponsored_by(sponsor)),
        ]
    }

    #[test]
    fn test_fully_sponsored_zero_balance_buyer_has_no_violations() {
        let (buyer, sponsor) = (address(), address());
        let provenance = classify(&buyer, &fully_sponsored(&buyer, &sponsor)).unwrap();
        assert_eq!(provenance.violations_for_sponsor(&sponsor), vec![]);
    }

    #[test]
    fn test_buyer_holding_xlm_is_reported() {
        let (buyer, sponsor) = (address(), address());
        let mut entries = fully_sponsored(&buyer, &sponsor);
        entries[0].0 = account_entry(&buyer, 10_000_000_000);
        let provenance = classify(&buyer, &entries).unwrap();
        assert_eq!(
            provenance.violations_for_sponsor(&sponsor),
            vec![SponsorshipViolation::BuyerHoldsXlm { stroops: 10_000_000_000 }]
        );
    }

    #[test]
    fn test_unsponsored_account_reserve_is_attributed_to_buyer() {
        let (buyer, sponsor) = (address(), address());
        let mut entries = fully_sponsored(&buyer, &sponsor);
        entries[0].1 = LedgerEntryExt::V0;
        let provenance = classify(&buyer, &entries).unwrap();
        assert_eq!(
            provenance.violations_for_sponsor(&sponsor),
            vec![SponsorshipViolation::AccountReserve { payer: ReservePayer::Buyer }]
        );
    }

    #[test]
    fn test_empty_sponsorship_descriptor_is_attributed_to_buyer() {
        let (buyer, sponsor) = (address(), address());
        let mut entries = fully_sponsored(&buyer, &sponsor);
        entries[1].1 = LedgerEntryExt::V1(LedgerEntryExtensionV1 {
            sponsoring_id: SponsorshipDescriptor(None),
            ext: LedgerEntryExtensionV1Ext::V0,
        });
        let provenance = classify(&buyer, &entries).unwrap();
        assert_eq!(
            provenance.violations_for_sponsor(&sponsor),
            vec![SponsorshipViolation::TrustlineReserve { payer: ReservePayer::Buyer }]
        );
    }

    #[test]
    fn test_reserve_paid_by_a_different_sponsor_is_reported() {
        let (buyer, sponsor, other) = (address(), address(), address());
        let mut entries = fully_sponsored(&buyer, &sponsor);
        entries[0].1 = sponsored_by(&other);
        let provenance = classify(&buyer, &entries).unwrap();
        assert_eq!(
            provenance.violations_for_sponsor(&sponsor),
            vec![SponsorshipViolation::AccountReserve { payer: ReservePayer::Sponsor(other) }]
        );
    }

    #[test]
    fn test_trustline_for_another_asset_does_not_count() {
        let (buyer, sponsor) = (address(), address());
        let mut entries = fully_sponsored(&buyer, &sponsor);
        // Same code, wrong issuer: a lookalike USDC.
        entries[1].0 = trustline_entry(&buyer, &circle_usdc(Network::Pubnet));
        let provenance = classify(&buyer, &entries).unwrap();
        assert_eq!(
            provenance.violations_for_sponsor(&sponsor),
            vec![SponsorshipViolation::TrustlineMissing]
        );
    }

    #[test]
    fn test_entries_of_another_account_are_ignored() {
        let (buyer, sponsor, stranger) = (address(), address(), address());
        let entries = fully_sponsored(&stranger, &sponsor);
        assert_eq!(classify(&buyer, &entries), Err(ProvenanceError::AccountMissing));
    }

    fn onboarding_fixture() -> (AccountAddress, AccountAddress, Transaction) {
        let (sponsor, buyer) = (address(), address());
        let tx = sponsored_onboarding_transaction(
            &sponsor,
            42,
            &buyer,
            &circle_usdc(Network::Testnet),
            400,
            1_900_000_000,
        );
        (sponsor, buyer, tx)
    }

    #[test]
    fn test_onboarding_transaction_is_sourced_by_sponsor() {
        let (sponsor, _, tx) = onboarding_fixture();
        assert_eq!(tx.source_account, muxed_account(&sponsor));
    }

    #[test]
    fn test_onboarding_creates_buyer_with_zero_xlm() {
        let (_, buyer, tx) = onboarding_fixture();
        let created: Vec<_> = tx
            .operations
            .iter()
            .filter_map(|op| match &op.body {
                OperationBody::CreateAccount(create) => Some(create.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            created,
            vec![CreateAccountOp { destination: account_id(&buyer), starting_balance: 0 }]
        );
    }

    #[test]
    fn test_onboarding_wraps_buyer_entries_in_sponsorship_sandwich() {
        let (_, buyer, tx) = onboarding_fixture();
        let shape: Vec<(&'static str, Option<AccountAddress>)> = tx
            .operations
            .iter()
            .map(|op| {
                let source = op.source_account.as_ref().map(|m| match m {
                    stellar_xdr::MuxedAccount::Ed25519(key) => {
                        AccountAddress::from_public_key(key.0)
                    }
                    stellar_xdr::MuxedAccount::MuxedEd25519(_) => panic!("unexpected muxed source"),
                });
                (op.body.name(), source)
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("BeginSponsoringFutureReserves", None),
                ("CreateAccount", None),
                ("ChangeTrust", Some(buyer.clone())),
                ("EndSponsoringFutureReserves", Some(buyer)),
            ]
        );
    }

    #[test]
    fn test_onboarding_trusts_exactly_the_pinned_usdc() {
        let (_, _, tx) = onboarding_fixture();
        let OperationBody::ChangeTrust(change) = &tx.operations[2].body else {
            panic!("third operation must be ChangeTrust");
        };
        assert_eq!(change.line, change_trust_asset(&circle_usdc(Network::Testnet)));
    }

    #[test]
    fn test_onboarding_transaction_has_finite_upper_time_bound() {
        let (_, _, tx) = onboarding_fixture();
        assert!(matches!(
            tx.cond,
            Preconditions::Time(TimeBounds { max_time: TimePoint(1_900_000_000), .. })
        ));
    }

    fn buyers(n: usize) -> Vec<AccountAddress> {
        (0..n).map(|_| address()).collect()
    }

    fn batch(n: usize) -> Result<Transaction, BatchSizeError> {
        sponsored_onboarding_batch_transaction(
            &address(),
            1,
            &buyers(n),
            &circle_usdc(Network::Testnet),
            100,
            1_900_000_000,
        )
    }

    #[test]
    fn test_batch_at_signature_limit_is_built() {
        let tx = batch(MAX_BUYERS_PER_TRANSACTION).unwrap();
        assert_eq!((tx.operations.len(), tx.fee), (76, 7_600));
    }

    #[test]
    fn test_batch_above_signature_limit_is_refused() {
        assert_eq!(batch(MAX_BUYERS_PER_TRANSACTION + 1), Err(BatchSizeError(20)));
    }

    #[test]
    fn test_empty_batch_is_refused() {
        assert_eq!(batch(0), Err(BatchSizeError(0)));
    }

    #[test]
    fn test_batch_gives_each_buyer_its_own_sandwich() {
        let sponsor = address();
        let group = buyers(2);
        let tx = sponsored_onboarding_batch_transaction(
            &sponsor,
            1,
            &group,
            &circle_usdc(Network::Testnet),
            100,
            1_900_000_000,
        )
        .unwrap();
        let sponsored: Vec<AccountId> = tx
            .operations
            .iter()
            .filter_map(|op| match &op.body {
                OperationBody::BeginSponsoringFutureReserves(b) => Some(b.sponsored_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(sponsored, group.iter().map(account_id).collect::<Vec<_>>());
    }
}
