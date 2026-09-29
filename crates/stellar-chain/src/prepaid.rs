//! Calls and authorization trees for the prepaid ledger contract.
//!
//! Everything a signer is asked to authorize is rebuilt here from the pinned
//! deployment (contract, USDC contract, treasury) and the intent, never taken
//! from a caller: a signer then authorizes exactly this contract, function,
//! amount and counterparty, and a changed field invalidates the signature.

use fermah_pay_stellar_domain::AccountAddress;
use stellar_xdr::{
    BytesM, ContractDataDurability, ContractDataEntry, ContractEvent, ContractEventBody,
    ContractId, Hash, Int128Parts, InvokeContractArgs, LedgerEntryData, LedgerKey,
    LedgerKeyContractData, ScAddress, ScBytes, ScMap, ScMapEntry, ScSymbol, ScVal, ScVec,
    SorobanAuthorizedFunction, SorobanAuthorizedInvocation, StringM, TransactionMeta, VecM,
};

use crate::transaction::account_id;

/// A deployed prepaid ledger and the counterparties it is pinned to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepaidDeployment {
    pub contract: [u8; 32],
    pub usdc: [u8; 32],
    pub treasury: AccountAddress,
}

/// The accounts a ledger instance is pinned to at construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roles {
    pub admin: AccountAddress,
    pub operator: AccountAddress,
    pub seller: AccountAddress,
    pub treasury: AccountAddress,
    pub usdc: [u8; 32],
}

/// Constructor arguments, in the contract's order: roles, USDC contract and
/// the `Limits` struct, which encodes as a map keyed by field name.
#[must_use]
pub fn constructor_args(roles: &Roles, min_deposit: i128, max_charge: i128) -> Vec<ScVal> {
    let limits = ScVal::Map(Some(ScMap(
        VecM::try_from(vec![
            ScMapEntry { key: symbol_val("max_charge"), val: i128_val(max_charge) },
            ScMapEntry { key: symbol_val("min_deposit"), val: i128_val(min_deposit) },
        ])
        .expect("invariant: two entries fit a map"),
    )));
    vec![
        account_val(&roles.admin),
        account_val(&roles.operator),
        account_val(&roles.seller),
        account_val(&roles.treasury),
        ScVal::Address(ScAddress::Contract(ContractId(Hash(roles.usdc)))),
        limits,
    ]
}

fn symbol_val(name: &str) -> ScVal {
    ScVal::Symbol(ScSymbol(
        StringM::try_from(name).expect("invariant: field names are valid symbols"),
    ))
}

/// One charge: the buyer (account owner), the account's next sequence
/// number, the amount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChargeRequest {
    pub owner: AccountAddress,
    pub seq: u64,
    pub amount: i128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositIntent {
    pub owner: AccountAddress,
    pub amount: i128,
    pub deposit_id: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevenueWithdrawIntent {
    pub destination: AccountAddress,
    pub amount: i128,
    pub withdrawal_id: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WithdrawIntent {
    pub owner: AccountAddress,
    pub amount: i128,
    pub destination: AccountAddress,
    pub withdrawal_id: [u8; 32],
}

impl PrepaidDeployment {
    #[must_use]
    pub fn deposit_call(&self, intent: &DepositIntent) -> InvokeContractArgs {
        call(
            self.contract,
            "deposit",
            vec![
                account_val(&intent.owner),
                i128_val(intent.amount),
                bytes_val(&intent.deposit_id),
            ],
        )
    }

    /// What the buyer signs for a deposit: the deposit itself and, beneath
    /// it, the USDC transfer from the buyer to the pinned treasury.
    #[must_use]
    pub fn deposit_authorization(&self, intent: &DepositIntent) -> SorobanAuthorizedInvocation {
        invocation(
            self.deposit_call(intent),
            vec![invocation(self.transfer(&intent.owner, &self.treasury, intent.amount), vec![])],
        )
    }

    #[must_use]
    pub fn withdraw_call(&self, intent: &WithdrawIntent) -> InvokeContractArgs {
        call(
            self.contract,
            "withdraw",
            vec![
                account_val(&intent.owner),
                i128_val(intent.amount),
                account_val(&intent.destination),
                bytes_val(&intent.withdrawal_id),
            ],
        )
    }

    /// What the buyer signs for a withdrawal: only the withdrawal call, which
    /// fixes amount and destination.
    #[must_use]
    pub fn owner_withdraw_authorization(
        &self,
        intent: &WithdrawIntent,
    ) -> SorobanAuthorizedInvocation {
        invocation(self.withdraw_call(intent), vec![])
    }

    /// What the treasury signs for a withdrawal: the call and the USDC
    /// transfer out of the treasury to the same destination and amount.
    #[must_use]
    pub fn treasury_withdraw_authorization(
        &self,
        intent: &WithdrawIntent,
    ) -> SorobanAuthorizedInvocation {
        invocation(
            self.withdraw_call(intent),
            vec![invocation(
                self.transfer(&self.treasury, &intent.destination, intent.amount),
                vec![],
            )],
        )
    }

    #[must_use]
    pub fn withdraw_revenue_call(&self, intent: &RevenueWithdrawIntent) -> InvokeContractArgs {
        call(
            self.contract,
            "withdraw_revenue",
            vec![
                account_val(&intent.destination),
                i128_val(intent.amount),
                bytes_val(&intent.withdrawal_id),
            ],
        )
    }

    /// What the seller signs to take revenue out: only the call.
    #[must_use]
    pub fn seller_revenue_authorization(
        &self,
        intent: &RevenueWithdrawIntent,
    ) -> SorobanAuthorizedInvocation {
        invocation(self.withdraw_revenue_call(intent), vec![])
    }

    /// What the treasury signs to pay revenue out: the call and the USDC
    /// transfer to the same destination and amount.
    #[must_use]
    pub fn treasury_revenue_authorization(
        &self,
        intent: &RevenueWithdrawIntent,
    ) -> SorobanAuthorizedInvocation {
        invocation(
            self.withdraw_revenue_call(intent),
            vec![invocation(
                self.transfer(&self.treasury, &intent.destination, intent.amount),
                vec![],
            )],
        )
    }

    #[must_use]
    pub fn get_balance_call(&self, owner: &AccountAddress) -> InvokeContractArgs {
        call(self.contract, "get_balance", vec![account_val(owner)])
    }

    #[must_use]
    pub fn get_totals_call(&self) -> InvokeContractArgs {
        call(self.contract, "get_totals", vec![])
    }

    #[must_use]
    pub fn get_config_call(&self) -> InvokeContractArgs {
        call(self.contract, "get_config", vec![])
    }

    #[must_use]
    pub fn charge_call(&self, charge: &ChargeRequest) -> InvokeContractArgs {
        call(self.contract, "charge", vec![charge_val(charge)])
    }

    /// What the operator signs for a single charge.
    #[must_use]
    pub fn charge_authorization(&self, charge: &ChargeRequest) -> SorobanAuthorizedInvocation {
        invocation(self.charge_call(charge), vec![])
    }

    #[must_use]
    pub fn charge_batch_call(&self, charges: &[ChargeRequest]) -> InvokeContractArgs {
        let entries: Vec<ScVal> = charges.iter().map(charge_val).collect();
        call(self.contract, "charge_batch", vec![vec_val(entries)])
    }

    /// What the operator signs for a batch.
    #[must_use]
    pub fn charge_batch_authorization(
        &self,
        charges: &[ChargeRequest],
    ) -> SorobanAuthorizedInvocation {
        invocation(self.charge_batch_call(charges), vec![])
    }

    /// Ledger key of the owner's account entry, whose value carries the
    /// balance and the last consumed charge sequence.
    #[must_use]
    pub fn account_key(&self, owner: &AccountAddress) -> LedgerKey {
        self.persistent_key(vec_val(vec![symbol_val("Account"), account_val(owner)]))
    }

    /// Ledger key of the marker the contract writes when it processes
    /// `deposit_id` for `owner`: present exactly when that deposit was
    /// credited.
    #[must_use]
    pub fn deposit_key(&self, owner: &AccountAddress, deposit_id: &[u8; 32]) -> LedgerKey {
        self.persistent_key(vec_val(vec![
            symbol_val("Deposit"),
            account_val(owner),
            bytes_val(deposit_id),
        ]))
    }

    fn persistent_key(&self, key: ScVal) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash(self.contract))),
            key,
            durability: ContractDataDurability::Persistent,
        })
    }

    fn transfer(
        &self,
        from: &AccountAddress,
        to: &AccountAddress,
        amount: i128,
    ) -> InvokeContractArgs {
        call(self.usdc, "transfer", vec![account_val(from), account_val(to), i128_val(amount)])
    }
}

/// A charge's result as `charge_batch` returns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Charged,
    InsufficientBalance,
    AboveLimit,
    Duplicate,
    OutOfOrder,
    UnknownAccount,
}

impl Outcome {
    /// The contract encodes the enum as its `u32` discriminant.
    #[must_use]
    pub const fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::Charged,
            1 => Self::InsufficientBalance,
            2 => Self::AboveLimit,
            3 => Self::Duplicate,
            4 => Self::OutOfOrder,
            5 => Self::UnknownAccount,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Charged => "charged",
            Self::InsufficientBalance => "insufficient_balance",
            Self::AboveLimit => "above_limit",
            Self::Duplicate => "duplicate",
            Self::OutOfOrder => "out_of_order",
            Self::UnknownAccount => "unknown_account",
        }
    }
}

/// The outcomes `charge_batch` returned, one per submitted charge in order;
/// `None` if the value has any other shape.
#[must_use]
pub fn batch_outcomes(value: &ScVal) -> Option<Vec<Outcome>> {
    let ScVal::Vec(Some(ScVec(items))) = value else { return None };
    items
        .iter()
        .map(|item| match item {
            ScVal::U32(code) => Outcome::from_code(*code),
            _ => None,
        })
        .collect()
}

/// One entry of the contract's `charges` event: which account, which
/// sequence, which amount, and what the contract decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettledEntry {
    pub owner: AccountAddress,
    pub seq: u64,
    pub amount: i128,
    pub outcome: Outcome,
}

/// The entries of a `charges` event emitted by `contract`; `None` if the
/// event is another contract's, another kind, or malformed.
#[must_use]
pub fn settled_entries(event: &ContractEvent, contract: &[u8; 32]) -> Option<Vec<SettledEntry>> {
    if event.contract_id != Some(ContractId(Hash(*contract))) {
        return None;
    }
    let ContractEventBody::V0(body) = &event.body;
    let [ScVal::Symbol(topic)] = body.topics.as_slice() else { return None };
    if topic.0.as_slice() != b"charges" {
        return None;
    }
    let ScVal::Vec(Some(ScVec(entries))) = &body.data else { return None };
    entries
        .iter()
        .map(|entry| {
            let ScVal::Vec(Some(ScVec(fields))) = entry else { return None };
            let [
                ScVal::Address(ScAddress::Account(owner)),
                ScVal::U64(seq),
                amount,
                ScVal::U32(code),
            ] = fields.as_slice()
            else {
                return None;
            };
            let ScVal::I128(Int128Parts { hi, lo }) = amount else { return None };
            Some(SettledEntry {
                owner: crate::transaction::address_of(owner),
                seq: *seq,
                amount: (i128::from(*hi) << 64) | i128::from(*lo),
                outcome: Outcome::from_code(*code)?,
            })
        })
        .collect()
}

/// Every `charges` entry `contract` emitted in an included transaction.
#[must_use]
pub fn settled_in(meta: &TransactionMeta, contract: &[u8; 32]) -> Vec<SettledEntry> {
    let events: Vec<&ContractEvent> = match meta {
        TransactionMeta::V4(meta) => {
            meta.operations.iter().flat_map(|op| op.events.iter()).collect()
        }
        TransactionMeta::V3(meta) => {
            meta.soroban_meta.iter().flat_map(|soroban| soroban.events.iter()).collect()
        }
        _ => Vec::new(),
    };
    events.into_iter().filter_map(|event| settled_entries(event, contract)).flatten().collect()
}

/// The contract's current roles and asset, as `get_config` returns them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractConfig {
    pub admin: AccountAddress,
    pub operator: AccountAddress,
    pub seller: AccountAddress,
    pub treasury: AccountAddress,
    pub usdc: [u8; 32],
    pub paused: bool,
}

/// Decodes `get_config`'s return value; `None` for any other shape.
#[must_use]
pub fn contract_config(value: &ScVal) -> Option<ContractConfig> {
    let ScVal::Map(Some(fields)) = value else { return None };
    let field = |name: &[u8]| {
        fields
            .iter()
            .find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
            .map(|f| &f.val)
    };
    let account = |name: &[u8]| match field(name)? {
        ScVal::Address(ScAddress::Account(account)) => {
            Some(crate::transaction::address_of(account))
        }
        _ => None,
    };
    let usdc = match field(b"usdc")? {
        ScVal::Address(ScAddress::Contract(ContractId(Hash(id)))) => *id,
        _ => return None,
    };
    let ScVal::Bool(paused) = field(b"paused")? else { return None };
    Some(ContractConfig {
        admin: account(b"admin")?,
        operator: account(b"operator")?,
        seller: account(b"seller")?,
        treasury: account(b"treasury")?,
        usdc,
        paused: *paused,
    })
}

/// The balance and last consumed charge sequence in an account entry read
/// from the ledger; `None` if the entry is not an account entry of this
/// contract.
#[must_use]
pub fn account_state(entry: &LedgerEntryData) -> Option<(i128, u64)> {
    let LedgerEntryData::ContractData(ContractDataEntry { val: ScVal::Map(Some(fields)), .. }) =
        entry
    else {
        return None;
    };
    let field = |name: &[u8]| {
        fields.iter().find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
    };
    let balance = match &field(b"balance")?.val {
        ScVal::I128(Int128Parts { hi, lo }) => (i128::from(*hi) << 64) | i128::from(*lo),
        _ => return None,
    };
    let ScVal::U64(seq) = field(b"charge_seq")?.val else { return None };
    Some((balance, seq))
}

fn call(contract: [u8; 32], function: &str, args: Vec<ScVal>) -> InvokeContractArgs {
    InvokeContractArgs {
        contract_address: ScAddress::Contract(ContractId(Hash(contract))),
        function_name: ScSymbol(
            StringM::try_from(function)
                .expect("invariant: contract function names are valid symbols"),
        ),
        args: VecM::try_from(args).expect("invariant: contract calls have few arguments"),
    }
}

fn invocation(
    call: InvokeContractArgs,
    sub_invocations: Vec<SorobanAuthorizedInvocation>,
) -> SorobanAuthorizedInvocation {
    SorobanAuthorizedInvocation {
        function: SorobanAuthorizedFunction::ContractFn(call),
        sub_invocations: VecM::try_from(sub_invocations)
            .expect("invariant: authorization trees here have at most one sub-invocation"),
    }
}

fn account_val(address: &AccountAddress) -> ScVal {
    ScVal::Address(ScAddress::Account(account_id(address)))
}

fn bytes_val(bytes: &[u8]) -> ScVal {
    ScVal::Bytes(ScBytes(
        BytesM::try_from(bytes.to_vec()).expect("invariant: identifiers are at most 32 bytes"),
    ))
}

/// `i128` as the contract ABI encodes it: high signed and low unsigned halves.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn i128_val(value: i128) -> ScVal {
    ScVal::I128(Int128Parts { hi: (value >> 64) as i64, lo: value as u64 })
}

fn vec_val(items: Vec<ScVal>) -> ScVal {
    ScVal::Vec(Some(ScVec(
        VecM::try_from(items).expect("invariant: batches are bounded far below the vector limit"),
    )))
}

/// The contract's `Charge` tuple struct, which encodes as a vector of fields.
fn charge_val(charge: &ChargeRequest) -> ScVal {
    vec_val(vec![account_val(&charge.owner), ScVal::U64(charge.seq), i128_val(charge.amount)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_i128_encoding_splits_sign_into_high_half() {
        assert_eq!(i128_val(-1), ScVal::I128(Int128Parts { hi: -1, lo: u64::MAX }));
    }

    #[test]
    fn test_i128_encoding_of_value_above_u64() {
        let value = i128::from(u64::MAX) + 5;
        assert_eq!(i128_val(value), ScVal::I128(Int128Parts { hi: 1, lo: 4 }));
    }
}
