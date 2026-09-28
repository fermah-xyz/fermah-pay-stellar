//! Contract invocations submitted by an operator on behalf of signers who
//! hold no XLM: the signers authorize the call, an operator source account
//! signs the transaction, and a separate fee account pays through a fee bump.

use std::time::Duration;

use fermah_pay_stellar_domain::{AccountAddress, Network};
use stellar_xdr::{
    HostFunction, LedgerEntryData, LedgerKey, LedgerKeyAccount, ScAddress, ScVal,
    SorobanAddressCredentials, SorobanAuthorizationEntry, SorobanAuthorizedInvocation,
    SorobanCredentials, SorobanResources, TransactionEnvelope, TransactionExt,
};

use crate::keys::SecretKey;
use crate::rpc::{AuthMode, RpcClient, RpcError, SimulationOutcome, hex_lower};
use crate::soroban::{self, AssemblyError};
use crate::submission::{Included, SubmissionError, submit_and_wait, unix_now};
use crate::transaction::{self, SigningError, account_id};

/// Knobs a deployment tunes per network conditions.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// Inclusion bid per operation, in stroops.
    pub inclusion_fee: u32,
    /// Headroom added to the simulated resource fee; unused fee is refunded.
    pub resource_fee_margin_percent: u8,
    /// Upper time bound of each transaction, from its construction.
    pub validity: Duration,
    pub poll_interval: Duration,
}

/// Which credential encoding to request signatures in. `AddressV2` binds
/// the signer's address into the signed payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Credentials {
    Address,
    AddressV2,
}

#[derive(Debug, thiserror::Error)]
pub enum SponsoredError {
    #[error("RPC failure before submission")]
    Rpc(#[source] RpcError),
    #[error("source account {0} does not exist")]
    SourceMissing(AccountAddress),
    #[error("simulation failed: {0}")]
    SimulationFailed(String),
    #[error("archived state must be restored before this call")]
    RestoreRequired,
    #[error("the call needs authorization from {needed:?}, not the prepared {prepared:?}")]
    AuthorizationMismatch { needed: Vec<String>, prepared: Vec<String> },
    #[error(
        "the call needs an address signature, but only source-account authorization was expected"
    )]
    UnexpectedAddressAuthorization,
    #[error("operating system randomness unavailable")]
    Randomness(#[source] getrandom::Error),
    #[error("assembling the transaction")]
    Assembly(#[source] AssemblyError),
    #[error("signing the transaction")]
    Signing(#[source] SigningError),
    #[error("submitting the transaction")]
    Submission(#[source] SubmissionError),
}

/// What happened to a sponsored submission, with the identities needed to
/// correlate it later.
#[derive(Debug, Clone)]
pub struct Receipt {
    /// Hash of the signed inner transaction.
    pub inner_hash: [u8; 32],
    /// Hash of the fee-bump envelope, which the network reports.
    pub outer_hash: [u8; 32],
    pub inner_source: AccountAddress,
    pub fee_source: AccountAddress,
    pub ledger: u32,
    pub fee_charged_stroops: i64,
    pub return_value: Option<ScVal>,
    pub resources: SorobanResources,
    pub envelope: TransactionEnvelope,
}

/// Unsigned authorization entries for the expected signers, with fresh
/// nonces and an expiration ledger, ready to be signed.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub entries: Vec<SorobanAuthorizationEntry>,
    pub latest_ledger: u32,
}

pub struct Submitter<'a> {
    pub rpc: &'a RpcClient,
    pub network: Network,
    /// Signs the inner transaction and spends its sequence number.
    pub source: &'a SecretKey,
    /// Signs the fee bump and pays.
    pub fee_source: &'a SecretKey,
    pub policy: Policy,
}

impl Submitter<'_> {
    /// Discovers, by recording simulation, which authorizations `function`
    /// needs and refuses unless they are exactly `expected`: each signer
    /// authorizing exactly the tree built for it. Returns unsigned entries for
    /// those trees, valid for `validity_ledgers` from the latest ledger.
    pub async fn prepare_authorizations(
        &self,
        function: &HostFunction,
        expected: &[(AccountAddress, SorobanAuthorizedInvocation)],
        validity_ledgers: u32,
        credentials: Credentials,
    ) -> Result<Prepared, SponsoredError> {
        let simulation = self.simulate(function.clone(), vec![], AuthMode::Record).await?;
        let mut needed: Vec<(AccountAddress, SorobanAuthorizedInvocation)> = Vec::new();
        for entry in &simulation.auth {
            match &entry.credentials {
                SorobanCredentials::Address(c) | SorobanCredentials::AddressV2(c) => match &c
                    .address
                {
                    ScAddress::Account(account) => needed
                        .push((transaction::address_of(account), entry.root_invocation.clone())),
                    other => {
                        return Err(SponsoredError::AuthorizationMismatch {
                            needed: vec![format!("{other:?}")],
                            prepared: describe(expected),
                        });
                    }
                },
                SorobanCredentials::SourceAccount => {}
                SorobanCredentials::AddressWithDelegates(_) => {
                    return Err(SponsoredError::UnexpectedAddressAuthorization);
                }
            }
        }
        if !same_multiset(&needed, expected) {
            return Err(SponsoredError::AuthorizationMismatch {
                needed: describe(&needed),
                prepared: describe(expected),
            });
        }
        let expiration = simulation.latest_ledger.saturating_add(validity_ledgers);
        let entries = expected
            .iter()
            .map(|(signer, invocation)| {
                let creds = SorobanAddressCredentials {
                    address: ScAddress::Account(account_id(signer)),
                    nonce: random_nonce()?,
                    signature_expiration_ledger: expiration,
                    signature: ScVal::Void,
                };
                Ok(SorobanAuthorizationEntry {
                    credentials: match credentials {
                        Credentials::Address => SorobanCredentials::Address(creds),
                        Credentials::AddressV2 => SorobanCredentials::AddressV2(creds),
                    },
                    root_invocation: invocation.clone(),
                })
            })
            .collect::<Result<Vec<_>, SponsoredError>>()?;
        Ok(Prepared { entries, latest_ledger: simulation.latest_ledger })
    }

    /// Source-account authorization entries recorded for `function`, for
    /// calls whose only authorizer is the inner source (e.g. deployment).
    /// Refuses if any other address must sign.
    pub async fn record_source_authorization(
        &self,
        function: &HostFunction,
    ) -> Result<Vec<SorobanAuthorizationEntry>, SponsoredError> {
        let simulation = self.simulate(function.clone(), vec![], AuthMode::Record).await?;
        if simulation
            .auth
            .iter()
            .any(|entry| !matches!(entry.credentials, SorobanCredentials::SourceAccount))
        {
            return Err(SponsoredError::UnexpectedAddressAuthorization);
        }
        Ok(simulation.auth)
    }

    /// The value a read-only call returns, by simulation; nothing is
    /// submitted.
    pub async fn read(&self, function: HostFunction) -> Result<Option<ScVal>, SponsoredError> {
        Ok(self.simulate(function, vec![], AuthMode::Record).await?.result)
    }

    /// Simulates with the signed `auth` entries in enforcing mode, assembles,
    /// signs, wraps in a fee bump and submits; waits for the outcome.
    pub async fn submit(
        &self,
        function: HostFunction,
        auth: Vec<SorobanAuthorizationEntry>,
    ) -> Result<Receipt, SponsoredError> {
        let sequence = self.next_sequence().await?;
        let valid_until = unix_now().saturating_add(self.policy.validity.as_secs());
        let unassembled = soroban::invocation_transaction(
            &self.source.address(),
            sequence,
            function,
            auth,
            self.policy.inclusion_fee,
            valid_until,
        )
        .map_err(SponsoredError::Assembly)?;
        let simulation = self.simulate_transaction(&unassembled, AuthMode::Enforce).await?;
        let tx = soroban::assemble(
            unassembled,
            simulation.transaction_data,
            simulation.min_resource_fee,
            self.policy.resource_fee_margin_percent,
        )
        .map_err(SponsoredError::Assembly)?;
        let resources = match &tx.ext {
            TransactionExt::V1(data) => data.resources.clone(),
            TransactionExt::V0 => unreachable!("assemble always sets Soroban resources"),
        };
        let inner_hash =
            transaction::transaction_hash(&tx, self.network).map_err(SponsoredError::Signing)?;
        let TransactionEnvelope::Tx(inner) =
            transaction::sign(tx, self.network, &[self.source]).map_err(SponsoredError::Signing)?
        else {
            unreachable!("transaction::sign produces a v1 envelope")
        };
        let bump = soroban::fee_bump(inner, &self.fee_source.address(), self.policy.inclusion_fee)
            .map_err(SponsoredError::Assembly)?;
        let outer_hash =
            soroban::fee_bump_hash(&bump, self.network).map_err(SponsoredError::Assembly)?;
        let envelope = soroban::sign_fee_bump(bump, self.network, self.fee_source)
            .map_err(SponsoredError::Assembly)?;
        let Included { transaction: included, .. } = submit_and_wait(
            self.rpc,
            &envelope,
            outer_hash,
            valid_until,
            self.policy.poll_interval,
        )
        .await
        .map_err(SponsoredError::Submission)?;
        Ok(Receipt {
            inner_hash,
            outer_hash,
            inner_source: self.source.address(),
            fee_source: self.fee_source.address(),
            ledger: included.ledger,
            fee_charged_stroops: included.result.fee_charged,
            return_value: included.return_value().cloned(),
            resources,
            envelope,
        })
    }

    async fn simulate(
        &self,
        function: HostFunction,
        auth: Vec<SorobanAuthorizationEntry>,
        mode: AuthMode,
    ) -> Result<crate::rpc::Simulation, SponsoredError> {
        let sequence = self.next_sequence().await?;
        let tx = soroban::invocation_transaction(
            &self.source.address(),
            sequence,
            function,
            auth,
            self.policy.inclusion_fee,
            unix_now().saturating_add(self.policy.validity.as_secs()),
        )
        .map_err(SponsoredError::Assembly)?;
        self.simulate_transaction(&tx, mode).await
    }

    async fn simulate_transaction(
        &self,
        tx: &stellar_xdr::Transaction,
        mode: AuthMode,
    ) -> Result<crate::rpc::Simulation, SponsoredError> {
        let envelope = TransactionEnvelope::Tx(stellar_xdr::TransactionV1Envelope {
            tx: tx.clone(),
            signatures: stellar_xdr::VecM::default(),
        });
        match self.rpc.simulate_transaction(&envelope, mode).await.map_err(SponsoredError::Rpc)? {
            SimulationOutcome::Succeeded(simulation) => Ok(*simulation),
            SimulationOutcome::Failed { error, .. } => Err(SponsoredError::SimulationFailed(error)),
            SimulationOutcome::RestoreRequired { .. } => Err(SponsoredError::RestoreRequired),
        }
    }

    async fn next_sequence(&self) -> Result<i64, SponsoredError> {
        let source = self.source.address();
        let key = LedgerKey::Account(LedgerKeyAccount { account_id: account_id(&source) });
        let records = self.rpc.get_ledger_entries(&[key]).await.map_err(SponsoredError::Rpc)?;
        records
            .iter()
            .find_map(|record| match &record.data {
                LedgerEntryData::Account(entry) => Some(entry.seq_num.0 + 1),
                _ => None,
            })
            .ok_or(SponsoredError::SourceMissing(source))
    }
}

fn random_nonce() -> Result<i64, SponsoredError> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(SponsoredError::Randomness)?;
    Ok(i64::from_le_bytes(bytes))
}

fn same_multiset(
    a: &[(AccountAddress, SorobanAuthorizedInvocation)],
    b: &[(AccountAddress, SorobanAuthorizedInvocation)],
) -> bool {
    let mut remaining: Vec<&(AccountAddress, SorobanAuthorizedInvocation)> = b.iter().collect();
    a.len() == b.len()
        && a.iter().all(|item| match remaining.iter().position(|candidate| *candidate == item) {
            Some(index) => {
                remaining.swap_remove(index);
                true
            }
            None => false,
        })
}

fn describe(items: &[(AccountAddress, SorobanAuthorizedInvocation)]) -> Vec<String> {
    items
        .iter()
        .map(|(signer, invocation)| {
            let function = match &invocation.function {
                stellar_xdr::SorobanAuthorizedFunction::ContractFn(call) => {
                    String::from_utf8_lossy(call.function_name.0.as_slice()).into_owned()
                }
                other => format!("{other:?}"),
            };
            format!("{signer}:{function}")
        })
        .collect()
}

/// Lowercase hex of a hash, for evidence records and logs.
#[must_use]
pub fn hash_hex(hash: &[u8; 32]) -> String {
    hex_lower(hash)
}

#[cfg(test)]
mod tests {
    use stellar_xdr::{
        ContractId, Hash, InvokeContractArgs, ScSymbol, SorobanAuthorizedFunction, StringM, VecM,
    };

    use super::*;

    fn tree(function: &str) -> SorobanAuthorizedInvocation {
        SorobanAuthorizedInvocation {
            function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                contract_address: ScAddress::Contract(ContractId(Hash([1; 32]))),
                function_name: ScSymbol(StringM::try_from(function).unwrap()),
                args: VecM::default(),
            }),
            sub_invocations: VecM::default(),
        }
    }

    #[test]
    fn test_same_multiset_ignores_order() {
        let (a, b) =
            (SecretKey::generate().unwrap().address(), SecretKey::generate().unwrap().address());
        let left = [(a.clone(), tree("withdraw")), (b.clone(), tree("withdraw"))];
        let right = [(b, tree("withdraw")), (a, tree("withdraw"))];
        assert!(same_multiset(&left, &right));
    }

    #[test]
    fn test_same_multiset_counts_duplicates() {
        let a = SecretKey::generate().unwrap().address();
        let twice = [(a.clone(), tree("deposit")), (a.clone(), tree("deposit"))];
        let once_and_other = [(a.clone(), tree("deposit")), (a, tree("withdraw"))];
        assert!(!same_multiset(&twice, &once_and_other));
    }

    #[test]
    fn test_same_multiset_refuses_missing_signer() {
        let a = SecretKey::generate().unwrap().address();
        assert!(!same_multiset(&[(a.clone(), tree("deposit"))], &[]));
    }
}
