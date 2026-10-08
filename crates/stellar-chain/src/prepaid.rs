//! Calls and authorization trees for the ledger contracts: the prepaid
//! ledger, whose USDC a separate treasury account holds, and the prepaid
//! vault, which holds the USDC itself. Both keep the same accounts, charge
//! records, mandates and events; a deployment's [`Custody`] says which one
//! it runs.
//!
//! Everything a signer is asked to authorize is rebuilt here from the pinned
//! deployment (contract, USDC contract, custody) and the intent, never taken
//! from a caller: a signer then authorizes exactly this contract, function,
//! amount and counterparty, and a changed field invalidates the signature.

use fermah_pay_stellar_domain::AccountAddress;
pub use fermah_pay_stellar_domain::ChainAddress;

use crate::transaction::ScAddressOf;
use stellar_xdr::{
    BytesM, ContractDataDurability, ContractDataEntry, ContractEvent, ContractEventBody,
    ContractExecutable, ContractId, Hash, Int128Parts, InvokeContractArgs, LedgerEntryData,
    LedgerKey, LedgerKeyContractData, ScAddress, ScBytes, ScContractInstance, ScMap, ScMapEntry,
    ScSymbol, ScVal, ScVec, SorobanAuthorizedFunction, SorobanAuthorizedInvocation, StringM,
    TransactionMeta, VecM,
};

/// The contract's limits the gateway must respect, mirrored from the
/// contract and pinned against it by the contract's tests.
pub const MAX_BATCH: usize = 98;
/// Most recurring charges one `charge_recurring_batch` call accepts.
pub const MAX_RECURRING_BATCH: usize = 35;
/// Furthest ahead of the current ledger a charge's last ledger may be.
pub const MAX_CHARGE_WINDOW: u32 = 17_280;
/// Ledgers a charge record outlives the charge's last ledger.
pub const CHARGE_RECORD_GRACE: u32 = 720;
/// Vault: ledgers from a buyer's lower limit or exit request to its effect.
pub const VAULT_NOTICE_LEDGERS: u32 = 18_720;

/// Who holds a deployment's USDC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Custody {
    /// The prepaid ledger: a separate treasury account holds it.
    Treasury(AccountAddress),
    /// The prepaid vault: the contract holds it.
    Vault,
}

/// A deployed ledger contract and the counterparties it is pinned to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepaidDeployment {
    pub contract: [u8; 32],
    pub usdc: [u8; 32],
    pub custody: Custody,
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

/// The `Limits` struct, which encodes as a map keyed by field name.
fn limits_val(min_deposit: i128, max_charge: i128) -> ScVal {
    ScVal::Map(Some(ScMap(
        VecM::try_from(vec![
            ScMapEntry { key: symbol_val("max_charge"), val: i128_val(max_charge) },
            ScMapEntry { key: symbol_val("min_deposit"), val: i128_val(min_deposit) },
        ])
        .expect("invariant: two entries fit a map"),
    )))
}

/// A change only the admin may make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdminAction {
    Pause,
    Unpause,
    SetLimits {
        min_deposit: i128,
        max_charge: i128,
    },
    /// Replaces the contract's code with the uploaded Wasm of this hash.
    Upgrade {
        wasm_hash: [u8; 32],
    },
    /// Vault: proposes new code, installable after the contract's delay.
    ProposeUpgrade {
        wasm_hash: [u8; 32],
    },
    /// Vault: drops the proposed code.
    CancelUpgrade,
    /// Vault: installs the proposed code once its delay has passed.
    InstallUpgrade,
    /// Vault: bounds each buyer's balance and the total, or with `None`
    /// removes the bounds.
    SetLaunchLimits {
        limits: Option<(i128, i128)>,
    },
    /// Moves a role to `holder`, who must authorize it as well.
    SetRole {
        role: Role,
        holder: AccountAddress,
    },
    /// Limits what one account, and all accounts together, may be charged
    /// in a UTC day.
    SetDailyLimits {
        per_buyer: i128,
        per_seller: i128,
    },
}

impl AdminAction {
    #[must_use]
    pub fn call(&self, contract: [u8; 32]) -> InvokeContractArgs {
        match self {
            Self::Pause => call(contract, "pause", vec![]),
            Self::Unpause => call(contract, "unpause", vec![]),
            Self::SetLimits { min_deposit, max_charge } => {
                call(contract, "set_limits", vec![limits_val(*min_deposit, *max_charge)])
            }
            Self::Upgrade { wasm_hash } => call(contract, "upgrade", vec![bytes_val(wasm_hash)]),
            Self::ProposeUpgrade { wasm_hash } => {
                call(contract, "propose_upgrade", vec![bytes_val(wasm_hash)])
            }
            Self::CancelUpgrade => call(contract, "cancel_upgrade", vec![]),
            Self::InstallUpgrade => call(contract, "upgrade", vec![]),
            Self::SetLaunchLimits { limits } => call(
                contract,
                "set_launch_limits",
                vec![match limits {
                    None => ScVal::Void,
                    Some((max_balance, max_total)) => ScVal::Map(Some(ScMap(
                        VecM::try_from(vec![
                            ScMapEntry {
                                key: symbol_val("max_balance"),
                                val: i128_val(*max_balance),
                            },
                            ScMapEntry { key: symbol_val("max_total"), val: i128_val(*max_total) },
                        ])
                        .expect("invariant: two entries fit a map"),
                    ))),
                }],
            ),
            Self::SetRole { role, holder } => {
                call(contract, &format!("set_{}", role.token()), vec![account_val(holder)])
            }
            Self::SetDailyLimits { per_buyer, per_seller } => call(
                contract,
                "set_daily_limits",
                vec![ScVal::Map(Some(ScMap(
                    VecM::try_from(vec![
                        ScMapEntry { key: symbol_val("per_buyer"), val: i128_val(*per_buyer) },
                        ScMapEntry { key: symbol_val("per_seller"), val: i128_val(*per_seller) },
                    ])
                    .expect("invariant: two entries fit a map"),
                )))],
            ),
        }
    }

    /// The accounts that must authorize the call, and what each signs: the
    /// admin always; for a role change, the new holder too.
    #[must_use]
    pub fn authorizations(
        &self,
        contract: [u8; 32],
        admin: &AccountAddress,
    ) -> Vec<(AccountAddress, SorobanAuthorizedInvocation)> {
        let tree = invocation(self.call(contract), vec![]);
        let mut needed = vec![(admin.clone(), tree.clone())];
        if let Self::SetRole { holder, .. } = self {
            needed.push((holder.clone(), tree));
        }
        needed
    }
}

/// The vault's constructor arguments, in its order: admin, operator, seller,
/// USDC contract and limits. The vault has no treasury.
#[must_use]
pub fn vault_constructor_args(
    admin: &AccountAddress,
    operator: &AccountAddress,
    seller: &AccountAddress,
    usdc: [u8; 32],
    min_deposit: i128,
    max_charge: i128,
) -> Vec<ScVal> {
    vec![
        account_val(admin),
        account_val(operator),
        account_val(seller),
        ScVal::Address(ScAddress::Contract(ContractId(Hash(usdc)))),
        limits_val(min_deposit, max_charge),
    ]
}

/// Constructor arguments, in the contract's order: roles, USDC contract and
/// limits.
#[must_use]
pub fn constructor_args(roles: &Roles, min_deposit: i128, max_charge: i128) -> Vec<ScVal> {
    let limits = limits_val(min_deposit, max_charge);
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

/// One charge: the buyer (account owner), the seller's identifier for the
/// charge, the amount, the last ledger in which it may be settled, and the
/// UTC day of ledger time it was admitted in. The vault counts the amount
/// against that day's share of the buyer's limit; the prepaid ledger does
/// not take it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChargeRequest {
    pub owner: ChainAddress,
    pub charge_id: [u8; 32],
    pub amount: i128,
    pub last_ledger: u32,
    pub day: u64,
}

/// A buyer's mandate: up to `amount` per `period_secs` for `cycles` periods,
/// until ledger `live_until`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MandateIntent {
    pub owner: AccountAddress,
    pub mandate_id: [u8; 32],
    pub amount: i128,
    pub period_secs: u64,
    pub cycles: u32,
    pub live_until: u32,
}

impl MandateIntent {
    /// The allowance the mandate approves: the amount for every period.
    #[must_use]
    pub fn allowance(&self) -> i128 {
        self.amount.saturating_mul(i128::from(self.cycles))
    }
}

/// One attempt to charge period `cycle` of a mandate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecurringChargeRequest {
    pub owner: AccountAddress,
    pub charge_id: [u8; 32],
    pub mandate_id: [u8; 32],
    pub cycle: u32,
    pub amount: i128,
    pub last_ledger: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositIntent {
    pub owner: ChainAddress,
    pub amount: i128,
    pub deposit_id: [u8; 32],
    /// Vault only: the daily spending limit signed with the deposit.
    pub cap: Option<i128>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevenueWithdrawIntent {
    pub destination: AccountAddress,
    pub amount: i128,
    pub withdrawal_id: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WithdrawIntent {
    pub owner: ChainAddress,
    pub amount: i128,
    pub destination: ChainAddress,
    pub withdrawal_id: [u8; 32],
}

impl PrepaidDeployment {
    /// The treasury account, for a deployment whose USDC one holds.
    #[must_use]
    pub const fn treasury(&self) -> Option<&AccountAddress> {
        match &self.custody {
            Custody::Treasury(treasury) => Some(treasury),
            Custody::Vault => None,
        }
    }

    #[must_use]
    pub const fn is_vault(&self) -> bool {
        matches!(self.custody, Custody::Vault)
    }

    /// Where deposits go and payouts come from.
    fn custodian(&self) -> ScAddress {
        match &self.custody {
            Custody::Treasury(treasury) => treasury.sc_address(),
            Custody::Vault => ScAddress::Contract(ContractId(Hash(self.contract))),
        }
    }

    #[must_use]
    pub fn deposit_call(&self, intent: &DepositIntent) -> InvokeContractArgs {
        let mut args = vec![
            account_val(&intent.owner),
            i128_val(intent.amount),
            bytes_val(&intent.deposit_id),
        ];
        if self.is_vault() {
            args.push(intent.cap.map_or(ScVal::Void, i128_val));
        }
        call(self.contract, "deposit", args)
    }

    /// What the buyer signs for a deposit: the deposit itself and, beneath
    /// it, the USDC transfer from the buyer to the pinned treasury, or to
    /// the vault.
    #[must_use]
    pub fn deposit_authorization(&self, intent: &DepositIntent) -> SorobanAuthorizedInvocation {
        invocation(
            self.deposit_call(intent),
            vec![invocation(
                usdc_transfer_call(self.usdc, &intent.owner, &self.custodian(), intent.amount),
                vec![],
            )],
        )
    }

    /// Vault: sets the buyer's daily spending limit.
    #[must_use]
    pub fn set_cap_call(&self, owner: &impl ScAddressOf, cap: i128) -> InvokeContractArgs {
        call(self.contract, "set_cap", vec![account_val(owner), i128_val(cap)])
    }

    /// What the buyer signs to set its limit: the call alone.
    #[must_use]
    pub fn set_cap_authorization(
        &self,
        owner: &impl ScAddressOf,
        cap: i128,
    ) -> SorobanAuthorizedInvocation {
        invocation(self.set_cap_call(owner, cap), vec![])
    }

    /// Vault: the buyer's request to take `amount` to `destination` without
    /// the operator, after the contract's notice.
    #[must_use]
    pub fn request_exit_call(
        &self,
        owner: &impl ScAddressOf,
        amount: i128,
        destination: &impl ScAddressOf,
    ) -> InvokeContractArgs {
        call(
            self.contract,
            "request_exit",
            vec![account_val(owner), i128_val(amount), account_val(destination)],
        )
    }

    /// What the buyer signs to request an exit: the call alone.
    #[must_use]
    pub fn request_exit_authorization(
        &self,
        owner: &impl ScAddressOf,
        amount: i128,
        destination: &impl ScAddressOf,
    ) -> SorobanAuthorizedInvocation {
        invocation(self.request_exit_call(owner, amount, destination), vec![])
    }

    /// Vault: pays a buyer's unlocked exit request; anyone may send it, and
    /// nobody authorizes it.
    #[must_use]
    pub fn exit_call(&self, owner: &impl ScAddressOf) -> InvokeContractArgs {
        call(self.contract, "exit", vec![account_val(owner)])
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

    /// What the withdrawal's co-signer signs. With a treasury, the treasury
    /// signs the call and the USDC transfer out of it to the same
    /// destination and amount; with a vault, the operator signs the call
    /// alone, the vault's own transfer needing no signature.
    #[must_use]
    pub fn cosigner_withdraw_authorization(
        &self,
        intent: &WithdrawIntent,
    ) -> SorobanAuthorizedInvocation {
        match &self.custody {
            Custody::Treasury(treasury) => invocation(
                self.withdraw_call(intent),
                vec![invocation(
                    self.transfer(treasury, &intent.destination, intent.amount),
                    vec![],
                )],
            ),
            Custody::Vault => invocation(self.withdraw_call(intent), vec![]),
        }
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
    /// transfer to the same destination and amount. `None` for a vault,
    /// whose seller pays revenue out alone.
    #[must_use]
    pub fn treasury_revenue_authorization(
        &self,
        intent: &RevenueWithdrawIntent,
    ) -> Option<SorobanAuthorizedInvocation> {
        let treasury = self.treasury()?;
        Some(invocation(
            self.withdraw_revenue_call(intent),
            vec![invocation(self.transfer(treasury, &intent.destination, intent.amount), vec![])],
        ))
    }

    #[must_use]
    pub fn get_balance_call(&self, owner: &impl ScAddressOf) -> InvokeContractArgs {
        call(self.contract, "get_balance", vec![account_val(owner)])
    }

    #[must_use]
    pub fn get_totals_call(&self) -> InvokeContractArgs {
        call(self.contract, "get_totals", vec![])
    }

    /// Replaces the contract's code with the uploaded Wasm `wasm_hash`.
    #[must_use]
    pub fn upgrade_call(&self, wasm_hash: [u8; 32]) -> InvokeContractArgs {
        call(self.contract, "upgrade", vec![bytes_val(&wasm_hash)])
    }

    /// What the admin signs for an upgrade.
    #[must_use]
    pub fn upgrade_authorization(&self, wasm_hash: [u8; 32]) -> SorobanAuthorizedInvocation {
        invocation(self.upgrade_call(wasm_hash), vec![])
    }

    #[must_use]
    pub fn get_config_call(&self) -> InvokeContractArgs {
        call(self.contract, "get_config", vec![])
    }

    #[must_use]
    pub fn charge_call(&self, charge: &ChargeRequest) -> InvokeContractArgs {
        call(self.contract, "charge", vec![self.charge_val(charge)])
    }

    /// The contract's `Charge` tuple struct, which encodes as a vector of
    /// fields; the vault's carries the admission day.
    fn charge_val(&self, charge: &ChargeRequest) -> ScVal {
        let mut fields = vec![
            account_val(&charge.owner),
            bytes_val(&charge.charge_id),
            i128_val(charge.amount),
            ScVal::U32(charge.last_ledger),
        ];
        if self.is_vault() {
            fields.push(ScVal::U64(charge.day));
        }
        vec_val(fields)
    }

    /// What the operator signs for a single charge.
    #[must_use]
    pub fn charge_authorization(&self, charge: &ChargeRequest) -> SorobanAuthorizedInvocation {
        invocation(self.charge_call(charge), vec![])
    }

    #[must_use]
    pub fn charge_batch_call(&self, charges: &[ChargeRequest]) -> InvokeContractArgs {
        let entries: Vec<ScVal> = charges.iter().map(|charge| self.charge_val(charge)).collect();
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

    #[must_use]
    pub fn authorize_recurring_call(&self, intent: &MandateIntent) -> InvokeContractArgs {
        call(
            self.contract,
            "authorize_recurring",
            vec![
                account_val(&intent.owner),
                bytes_val(&intent.mandate_id),
                i128_val(intent.amount),
                ScVal::U64(intent.period_secs),
                ScVal::U32(intent.cycles),
                ScVal::U32(intent.live_until),
            ],
        )
    }

    /// What the buyer signs for a mandate: the mandate and, beneath it, the
    /// approval of the ledger contract to move up to the whole mandate's
    /// amount until its last ledger.
    #[must_use]
    pub fn authorize_recurring_authorization(
        &self,
        intent: &MandateIntent,
    ) -> SorobanAuthorizedInvocation {
        invocation(
            self.authorize_recurring_call(intent),
            vec![invocation(
                self.approve(&intent.owner, intent.allowance(), intent.live_until),
                vec![],
            )],
        )
    }

    #[must_use]
    pub fn revoke_recurring_call(&self, owner: &AccountAddress) -> InvokeContractArgs {
        call(self.contract, "revoke_recurring", vec![account_val(owner)])
    }

    /// What the buyer signs to revoke: the revocation and, beneath it, the
    /// approval that sets the ledger contract's allowance to zero.
    #[must_use]
    pub fn revoke_recurring_authorization(
        &self,
        owner: &AccountAddress,
    ) -> SorobanAuthorizedInvocation {
        invocation(
            self.revoke_recurring_call(owner),
            vec![invocation(self.approve(owner, 0, 0), vec![])],
        )
    }

    #[must_use]
    pub fn charge_recurring_batch_call(
        &self,
        charges: &[RecurringChargeRequest],
    ) -> InvokeContractArgs {
        let entries: Vec<ScVal> = charges.iter().map(recurring_charge_val).collect();
        call(self.contract, "charge_recurring_batch", vec![vec_val(entries)])
    }

    /// What the operator signs for a batch of recurring charges.
    #[must_use]
    pub fn charge_recurring_batch_authorization(
        &self,
        charges: &[RecurringChargeRequest],
    ) -> SorobanAuthorizedInvocation {
        invocation(self.charge_recurring_batch_call(charges), vec![])
    }

    /// Ledger key of the owner's mandate.
    #[must_use]
    pub fn mandate_key(&self, owner: &AccountAddress) -> LedgerKey {
        self.persistent_key(vec_val(vec![symbol_val("Mandate"), account_val(owner)]))
    }

    /// Ledger key of the USDC contract's allowance from `owner` to this
    /// ledger contract, the approval a mandate makes.
    #[must_use]
    pub fn allowance_key(&self, owner: &AccountAddress) -> LedgerKey {
        let field = |name: &str, val: ScVal| ScMapEntry { key: symbol_val(name), val };
        let pair = ScVal::Map(Some(ScMap(
            VecM::try_from(vec![
                field("from", account_val(owner)),
                field(
                    "spender",
                    ScVal::Address(ScAddress::Contract(ContractId(Hash(self.contract)))),
                ),
            ])
            .expect("invariant: two fields"),
        )));
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash(self.usdc))),
            key: vec_val(vec![symbol_val("Allowance"), pair]),
            durability: ContractDataDurability::Temporary,
        })
    }

    /// Ledger key of the record of one recurring charge attempt.
    #[must_use]
    pub fn recurring_record_key(&self, owner: &AccountAddress, charge_id: &[u8; 32]) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash(self.contract))),
            key: vec_val(vec![symbol_val("Recurring"), account_val(owner), bytes_val(charge_id)]),
            durability: ContractDataDurability::Temporary,
        })
    }

    /// Ledger key of the USDC contract's balance entry for this contract:
    /// what a vault holds. Like any persistent entry it must be kept alive.
    #[must_use]
    pub fn vault_balance_key(&self) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash(self.usdc))),
            key: vec_val(vec![
                symbol_val("Balance"),
                ScVal::Address(ScAddress::Contract(ContractId(Hash(self.contract)))),
            ]),
            durability: ContractDataDurability::Persistent,
        })
    }

    /// Ledger key of the contract's instance entry, which holds its
    /// configuration and totals.
    #[must_use]
    pub fn instance_key(&self) -> LedgerKey {
        self.persistent_key(ScVal::LedgerKeyContractInstance)
    }

    /// Ledger key of the owner's account entry, whose value carries the
    /// balance.
    #[must_use]
    pub fn account_key(&self, owner: &impl ScAddressOf) -> LedgerKey {
        self.persistent_key(vec_val(vec![symbol_val("Account"), account_val(owner)]))
    }

    /// Ledger key of the marker the contract writes when it processes
    /// `deposit_id` for `owner`: present exactly when that deposit was
    /// credited.
    #[must_use]
    pub fn deposit_key(&self, owner: &impl ScAddressOf, deposit_id: &[u8; 32]) -> LedgerKey {
        self.persistent_key(vec_val(vec![
            symbol_val("Deposit"),
            account_val(owner),
            bytes_val(deposit_id),
        ]))
    }

    /// Ledger key of the marker the contract writes when it processes
    /// `withdrawal_id` for `owner`: present exactly when that withdrawal
    /// moved USDC.
    #[must_use]
    pub fn withdrawal_key(&self, owner: &impl ScAddressOf, withdrawal_id: &[u8; 32]) -> LedgerKey {
        self.persistent_key(vec_val(vec![
            symbol_val("Withdrawal"),
            account_val(owner),
            bytes_val(withdrawal_id),
        ]))
    }

    /// Ledger key of the temporary record the contract writes when it
    /// settles `charge_id` for `owner`, holding the outcome. It lives until
    /// shortly after the charge's last ledger.
    #[must_use]
    pub fn charge_record_key(&self, owner: &impl ScAddressOf, charge_id: &[u8; 32]) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash(self.contract))),
            key: vec_val(vec![symbol_val("Charge"), account_val(owner), bytes_val(charge_id)]),
            durability: ContractDataDurability::Temporary,
        })
    }

    fn persistent_key(&self, key: ScVal) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash(self.contract))),
            key,
            durability: ContractDataDurability::Persistent,
        })
    }

    /// A buyer's own call to the USDC contract setting its approval of the
    /// ledger contract, outside any mandate, and what the buyer signs for
    /// it: that call alone.
    #[must_use]
    pub fn buyer_approval_authorization(
        &self,
        owner: &AccountAddress,
        amount: i128,
        live_until: u32,
    ) -> (InvokeContractArgs, SorobanAuthorizedInvocation) {
        let call = self.approve(owner, amount, live_until);
        (call.clone(), invocation(call, vec![]))
    }

    /// The USDC approval of the ledger contract as spender of `owner`'s USDC.
    fn approve(&self, owner: &AccountAddress, amount: i128, live_until: u32) -> InvokeContractArgs {
        call(
            self.usdc,
            "approve",
            vec![
                account_val(owner),
                ScVal::Address(ScAddress::Contract(ContractId(Hash(self.contract)))),
                i128_val(amount),
                ScVal::U32(live_until),
            ],
        )
    }

    fn transfer(
        &self,
        from: &impl ScAddressOf,
        to: &impl ScAddressOf,
        amount: i128,
    ) -> InvokeContractArgs {
        usdc_transfer_call(self.usdc, from, to, amount)
    }

    /// A USDC transfer of `amount` out of the treasury to `to`, outside the
    /// ledger contract: what moves a surplus to a reserve. `None` for a
    /// vault, whose USDC only its own code moves.
    #[must_use]
    pub fn treasury_transfer_call(
        &self,
        to: &AccountAddress,
        amount: i128,
    ) -> Option<InvokeContractArgs> {
        Some(self.transfer(self.treasury()?, to, amount))
    }

    /// What the treasury signs for [`Self::treasury_transfer_call`]: that
    /// transfer and nothing else.
    #[must_use]
    pub fn treasury_transfer_authorization(
        &self,
        to: &AccountAddress,
        amount: i128,
    ) -> Option<SorobanAuthorizedInvocation> {
        Some(invocation(self.treasury_transfer_call(to, amount)?, vec![]))
    }
}

/// A plain transfer of `amount` USDC, through the asset contract `usdc`,
/// from `from` to `to`; `from` authorizes exactly this call.
#[must_use]
pub fn usdc_transfer_call(
    usdc: [u8; 32],
    from: &impl ScAddressOf,
    to: &impl ScAddressOf,
    amount: i128,
) -> InvokeContractArgs {
    call(usdc, "transfer", vec![account_val(from), account_val(to), i128_val(amount)])
}

/// What `from` signs for [`usdc_transfer_call`]: that transfer alone.
#[must_use]
pub fn usdc_transfer_authorization(
    usdc: [u8; 32],
    from: &impl ScAddressOf,
    to: &impl ScAddressOf,
    amount: i128,
) -> SorobanAuthorizedInvocation {
    invocation(usdc_transfer_call(usdc, from, to, amount), vec![])
}

/// The USDC `owner` holds, as the asset contract `usdc` reports it; works
/// for contract accounts, which hold USDC without a trustline.
#[must_use]
pub fn usdc_balance_call(usdc: [u8; 32], owner: &impl ScAddressOf) -> InvokeContractArgs {
    call(usdc, "balance", vec![account_val(owner)])
}

/// A charge's result as `charge_batch` returns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Charged,
    InsufficientBalance,
    AboveLimit,
    Duplicate,
    Expired,
    UnknownAccount,
    AboveDailyLimit,
    /// Vault: above what the buyer's signed limit leaves for the charge's day.
    AboveCap,
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
            4 => Self::Expired,
            5 => Self::UnknownAccount,
            6 => Self::AboveDailyLimit,
            7 => Self::AboveCap,
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
            Self::Expired => "expired",
            Self::UnknownAccount => "unknown_account",
            Self::AboveDailyLimit => "above_daily_limit",
            Self::AboveCap => "above_cap",
        }
    }
}

/// A recurring charge's result as `charge_recurring_batch` returns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecurringOutcome {
    Charged,
    Duplicate,
    Expired,
    NoMandate,
    MandateExpired,
    AlreadyCharged,
    NotDue,
    PeriodOver,
    AboveMandate,
    AboveLimit,
    AboveDailyLimit,
    AllowanceShort,
    WalletShort,
    TransferRefused,
}

impl RecurringOutcome {
    /// The contract encodes the enum as its `u32` discriminant.
    #[must_use]
    pub const fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::Charged,
            1 => Self::Duplicate,
            2 => Self::Expired,
            3 => Self::NoMandate,
            4 => Self::MandateExpired,
            5 => Self::AlreadyCharged,
            6 => Self::NotDue,
            7 => Self::PeriodOver,
            8 => Self::AboveMandate,
            9 => Self::AboveLimit,
            10 => Self::AboveDailyLimit,
            11 => Self::AllowanceShort,
            12 => Self::WalletShort,
            13 => Self::TransferRefused,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Charged => "charged",
            Self::Duplicate => "duplicate",
            Self::Expired => "expired",
            Self::NoMandate => "no_mandate",
            Self::MandateExpired => "mandate_expired",
            Self::AlreadyCharged => "already_charged",
            Self::NotDue => "not_due",
            Self::PeriodOver => "period_over",
            Self::AboveMandate => "above_mandate",
            Self::AboveLimit => "above_limit",
            Self::AboveDailyLimit => "above_daily_limit",
            Self::AllowanceShort => "allowance_short",
            Self::WalletShort => "wallet_short",
            Self::TransferRefused => "transfer_refused",
        }
    }
}

/// The outcomes `charge_recurring_batch` returned, one per submitted charge
/// in order; `None` if the value has any other shape.
#[must_use]
pub fn recurring_outcomes(value: &ScVal) -> Option<Vec<RecurringOutcome>> {
    let ScVal::Vec(Some(ScVec(items))) = value else { return None };
    items
        .iter()
        .map(|item| match item {
            ScVal::U32(code) => RecurringOutcome::from_code(*code),
            _ => None,
        })
        .collect()
}

/// A mandate as the contract stores it and announces it: up to `amount` per
/// `period_secs` of ledger time from `start`, for `cycles` periods, until
/// ledger `live_until`; `next_cycle` is the first period not yet charged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MandateTerms {
    pub mandate_id: [u8; 32],
    pub amount: i128,
    pub period_secs: u64,
    pub start: u64,
    pub cycles: u32,
    pub live_until: u32,
    pub next_cycle: u32,
}

fn mandate_of(value: &ScVal) -> Option<MandateTerms> {
    let ScVal::Map(Some(fields)) = value else { return None };
    let field = |name: &[u8]| {
        fields
            .iter()
            .find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
            .map(|f| &f.val)
    };
    let (ScVal::U64(period_secs), ScVal::U64(start)) = (field(b"period_secs")?, field(b"start")?)
    else {
        return None;
    };
    let (ScVal::U32(cycles), ScVal::U32(live_until), ScVal::U32(next_cycle)) =
        (field(b"cycles")?, field(b"live_until")?, field(b"next_cycle")?)
    else {
        return None;
    };
    Some(MandateTerms {
        mandate_id: bytes32_of(field(b"mandate_id")?)?,
        amount: i128_of(field(b"amount")?)?,
        period_secs: *period_secs,
        start: *start,
        cycles: *cycles,
        live_until: *live_until,
        next_cycle: *next_cycle,
    })
}

/// The mandate in a mandate entry read from the ledger; `None` if the entry
/// is not one.
#[must_use]
pub fn stored_mandate(entry: &LedgerEntryData) -> Option<MandateTerms> {
    let LedgerEntryData::ContractData(ContractDataEntry { val, .. }) = entry else { return None };
    mandate_of(val)
}

/// The amount and last ledger of a USDC allowance entry read from the
/// ledger. The USDC contract honours none of it after that ledger.
#[must_use]
pub fn allowance_of(entry: &LedgerEntryData) -> Option<(i128, u32)> {
    let LedgerEntryData::ContractData(ContractDataEntry { val: ScVal::Map(Some(fields)), .. }) =
        entry
    else {
        return None;
    };
    let field = |name: &[u8]| {
        fields
            .iter()
            .find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
            .map(|f| &f.val)
    };
    let ScVal::U32(live_until) = field(b"live_until_ledger")? else { return None };
    Some((i128_of(field(b"amount")?)?, *live_until))
}

/// The outcome in a recurring charge attempt's record read from the ledger.
#[must_use]
pub fn recurring_record(entry: &LedgerEntryData) -> Option<RecurringOutcome> {
    match entry {
        LedgerEntryData::ContractData(ContractDataEntry { val: ScVal::U32(code), .. }) => {
            RecurringOutcome::from_code(*code)
        }
        _ => None,
    }
}

/// One entry of the contract's `recurring` event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecurringEntry {
    pub owner: ChainAddress,
    pub charge_id: [u8; 32],
    pub mandate_id: [u8; 32],
    pub cycle: u32,
    pub amount: i128,
    pub outcome: RecurringOutcome,
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
/// charge, which amount, and what the contract decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettledEntry {
    pub owner: ChainAddress,
    pub charge_id: [u8; 32],
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
    let Some(LedgerEvent::Charges(entries)) = ledger_event(&body.topics, &body.data) else {
        return None;
    };
    entries
        .into_iter()
        .map(|entry| {
            Some(SettledEntry {
                owner: entry.owner,
                charge_id: entry.charge_id,
                amount: entry.amount,
                outcome: entry.outcome,
            })
        })
        .collect()
}

/// An owner or destination as the contract's events carry it.
fn chain_address_val(value: &ScVal) -> Option<ChainAddress> {
    match value {
        ScVal::Address(address) => crate::transaction::chain_address(address),
        _ => None,
    }
}

/// A contract role, as the `role` event names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Admin,
    Operator,
    Seller,
    Treasury,
}

impl Role {
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Operator => "operator",
            Self::Seller => "seller",
            Self::Treasury => "treasury",
        }
    }
}

/// One entry of a `charges` event, whatever the owner's address kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChargeEntry {
    pub owner: ChainAddress,
    pub charge_id: [u8; 32],
    pub amount: i128,
    pub outcome: Outcome,
}

/// An event the prepaid ledger contract publishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerEvent {
    Deposited {
        owner: ChainAddress,
        amount: i128,
        deposit_id: [u8; 32],
    },
    /// Every entry one `charge` or `charge_batch` call settled or refused.
    Charges(Vec<ChargeEntry>),
    Withdrawn {
        owner: ChainAddress,
        destination: ChainAddress,
        amount: i128,
        withdrawal_id: [u8; 32],
    },
    RevenueWithdrawn {
        destination: ChainAddress,
        amount: i128,
        withdrawal_id: [u8; 32],
    },
    RoleChanged {
        role: Role,
        previous: ChainAddress,
        current: ChainAddress,
    },
    /// The admin paused or unpaused the contract; the state after the call.
    PauseChanged {
        paused: bool,
    },
    /// The admin replaced the deposit and charge limits.
    LimitsChanged {
        previous: ContractLimits,
        current: ContractLimits,
    },
    /// The admin set the daily charge limits; `previous` is `None` the first
    /// time.
    DailyLimitsChanged {
        previous: Option<DailyLimits>,
        current: DailyLimits,
    },
    /// A buyer authorized a mandate, replacing any previous one.
    MandateAuthorized {
        owner: ChainAddress,
        mandate: MandateTerms,
    },
    /// A buyer revoked their mandate; `None` if there was none.
    MandateRevoked {
        owner: ChainAddress,
        mandate_id: Option<[u8; 32]>,
    },
    /// Every entry one `charge_recurring_batch` call settled or refused.
    Recurring(Vec<RecurringEntry>),
    /// Vault: the buyer's limit rose, in force at once.
    CapRaised {
        owner: ChainAddress,
        cap: i128,
    },
    /// Vault: the buyer asked for a lower limit, in force from `effective_at`.
    CapLowered {
        owner: ChainAddress,
        cap: i128,
        effective_at: u32,
    },
    /// Vault: the buyer asked to take `amount` out without the operator,
    /// payable from `unlock_at`.
    ExitRequested {
        owner: ChainAddress,
        amount: i128,
        destination: ChainAddress,
        unlock_at: u32,
    },
    /// Vault: an exit was paid.
    Exited {
        owner: ChainAddress,
        destination: ChainAddress,
        amount: i128,
    },
    /// Vault: the admin set or removed the launch limits.
    LaunchLimitsChanged {
        previous: Option<(i128, i128)>,
        current: Option<(i128, i128)>,
    },
    /// Vault: new code proposed, installable from `effective_at`.
    UpgradeProposed {
        wasm_hash: [u8; 32],
        effective_at: u32,
    },
    /// Vault: the proposed code was dropped.
    UpgradeCancelled {
        wasm_hash: [u8; 32],
    },
}

/// What one account, and all the seller's accounts together, may be charged
/// in a UTC day, in USDC base units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DailyLimits {
    pub per_buyer: i128,
    pub per_seller: i128,
}

fn daily_limits_of(value: &ScVal) -> Option<DailyLimits> {
    let ScVal::Map(Some(fields)) = value else { return None };
    let field = |name: &[u8]| {
        fields
            .iter()
            .find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
            .and_then(|f| i128_of(&f.val))
    };
    Some(DailyLimits { per_buyer: field(b"per_buyer")?, per_seller: field(b"per_seller")? })
}

/// The contract's deposit and charge limits, in USDC base units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContractLimits {
    pub min_deposit: i128,
    pub max_charge: i128,
}

fn limits_of(value: &ScVal) -> Option<ContractLimits> {
    let ScVal::Map(Some(fields)) = value else { return None };
    let field = |name: &[u8]| {
        fields
            .iter()
            .find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
            .and_then(|f| i128_of(&f.val))
    };
    Some(ContractLimits { min_deposit: field(b"min_deposit")?, max_charge: field(b"max_charge")? })
}

/// Decodes a contract event from its topics and data; `None` for any shape
/// the contract does not publish. Only the layout is checked: which contract
/// emitted the event is the caller's to check.
#[must_use]
pub fn ledger_event(topics: &[ScVal], data: &ScVal) -> Option<LedgerEvent> {
    let (ScVal::Symbol(name), rest) = topics.split_first()? else { return None };
    // Published as a single value rather than a vector.
    if let (b"pause", [], ScVal::Bool(paused)) = (name.0.as_slice(), rest, data) {
        return Some(LedgerEvent::PauseChanged { paused: *paused });
    }
    match (name.0.as_slice(), rest) {
        (b"mandate", [owner]) => {
            return Some(LedgerEvent::MandateAuthorized {
                owner: chain_address_val(owner)?,
                mandate: mandate_of(data)?,
            });
        }
        (b"revoke", [owner]) => {
            return Some(LedgerEvent::MandateRevoked {
                owner: chain_address_val(owner)?,
                mandate_id: match data {
                    ScVal::Void => None,
                    other => Some(bytes32_of(other)?),
                },
            });
        }
        (b"cap_raised", [owner]) => {
            return Some(LedgerEvent::CapRaised {
                owner: chain_address_val(owner)?,
                cap: i128_of(data)?,
            });
        }
        (b"upgrade_cancelled", []) => {
            return Some(LedgerEvent::UpgradeCancelled { wasm_hash: bytes32_of(data)? });
        }
        _ => {}
    }
    let fields = match data {
        ScVal::Vec(Some(ScVec(fields))) => fields.as_slice(),
        _ => return None,
    };
    Some(match (name.0.as_slice(), rest) {
        (b"deposit", [owner]) => {
            let [amount, deposit_id] = fields else { return None };
            LedgerEvent::Deposited {
                owner: chain_address_val(owner)?,
                amount: i128_of(amount)?,
                deposit_id: bytes32_of(deposit_id)?,
            }
        }
        (b"charges", []) => LedgerEvent::Charges(
            fields
                .iter()
                .map(|entry| {
                    let ScVal::Vec(Some(ScVec(entry))) = entry else { return None };
                    let [owner, charge_id, amount, ScVal::U32(code)] = entry.as_slice() else {
                        return None;
                    };
                    Some(ChargeEntry {
                        owner: chain_address_val(owner)?,
                        charge_id: bytes32_of(charge_id)?,
                        amount: i128_of(amount)?,
                        outcome: Outcome::from_code(*code)?,
                    })
                })
                .collect::<Option<_>>()?,
        ),
        (b"recurring", []) => {
            LedgerEvent::Recurring(
                fields
                    .iter()
                    .map(|entry| {
                        let ScVal::Vec(Some(ScVec(entry))) = entry else { return None };
                        let [
                            owner,
                            charge_id,
                            mandate_id,
                            ScVal::U32(cycle),
                            amount,
                            ScVal::U32(code),
                        ] = entry.as_slice()
                        else {
                            return None;
                        };
                        Some(RecurringEntry {
                            owner: chain_address_val(owner)?,
                            charge_id: bytes32_of(charge_id)?,
                            mandate_id: bytes32_of(mandate_id)?,
                            cycle: *cycle,
                            amount: i128_of(amount)?,
                            outcome: RecurringOutcome::from_code(*code)?,
                        })
                    })
                    .collect::<Option<_>>()?,
            )
        }
        (b"withdraw", [owner]) => {
            let [destination, amount, withdrawal_id] = fields else { return None };
            LedgerEvent::Withdrawn {
                owner: chain_address_val(owner)?,
                destination: chain_address_val(destination)?,
                amount: i128_of(amount)?,
                withdrawal_id: bytes32_of(withdrawal_id)?,
            }
        }
        (b"revenue", []) => {
            let [destination, amount, withdrawal_id] = fields else { return None };
            LedgerEvent::RevenueWithdrawn {
                destination: chain_address_val(destination)?,
                amount: i128_of(amount)?,
                withdrawal_id: bytes32_of(withdrawal_id)?,
            }
        }
        (b"role", [ScVal::Symbol(role)]) => {
            let [previous, current] = fields else { return None };
            let role = match role.0.as_slice() {
                b"admin" => Role::Admin,
                b"operator" => Role::Operator,
                b"seller" => Role::Seller,
                b"treasury" => Role::Treasury,
                _ => return None,
            };
            LedgerEvent::RoleChanged {
                role,
                previous: chain_address_val(previous)?,
                current: chain_address_val(current)?,
            }
        }
        (b"limits", []) => {
            let [previous, current] = fields else { return None };
            LedgerEvent::LimitsChanged {
                previous: limits_of(previous)?,
                current: limits_of(current)?,
            }
        }
        (b"cap_lowered", [owner]) => {
            let [cap, ScVal::U32(effective_at)] = fields else { return None };
            LedgerEvent::CapLowered {
                owner: chain_address_val(owner)?,
                cap: i128_of(cap)?,
                effective_at: *effective_at,
            }
        }
        (b"exit_requested", [owner]) => {
            let [amount, destination, ScVal::U32(unlock_at)] = fields else { return None };
            LedgerEvent::ExitRequested {
                owner: chain_address_val(owner)?,
                amount: i128_of(amount)?,
                destination: chain_address_val(destination)?,
                unlock_at: *unlock_at,
            }
        }
        (b"exit", [owner]) => {
            let [destination, amount] = fields else { return None };
            LedgerEvent::Exited {
                owner: chain_address_val(owner)?,
                destination: chain_address_val(destination)?,
                amount: i128_of(amount)?,
            }
        }
        (b"launch", []) => {
            let [previous, current] = fields else { return None };
            let limits = |value: &ScVal| -> Option<Option<(i128, i128)>> {
                match value {
                    ScVal::Void => Some(None),
                    ScVal::Map(Some(map)) => {
                        let field = |name: &[u8]| {
                            map.iter()
                                .find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
                                .and_then(|f| i128_of(&f.val))
                        };
                        Some(Some((field(b"max_balance")?, field(b"max_total")?)))
                    }
                    _ => None,
                }
            };
            LedgerEvent::LaunchLimitsChanged {
                previous: limits(previous)?,
                current: limits(current)?,
            }
        }
        (b"upgrade_proposed", []) => {
            let [wasm_hash, ScVal::U32(effective_at)] = fields else { return None };
            LedgerEvent::UpgradeProposed {
                wasm_hash: bytes32_of(wasm_hash)?,
                effective_at: *effective_at,
            }
        }
        (b"daily", []) => {
            let [previous, current] = fields else { return None };
            LedgerEvent::DailyLimitsChanged {
                previous: match previous {
                    ScVal::Void => None,
                    other => Some(daily_limits_of(other)?),
                },
                current: daily_limits_of(current)?,
            }
        }
        _ => return None,
    })
}

fn i128_of(value: &ScVal) -> Option<i128> {
    match value {
        ScVal::I128(Int128Parts { hi, lo }) => Some((i128::from(*hi) << 64) | i128::from(*lo)),
        _ => None,
    }
}

fn bytes32_of(value: &ScVal) -> Option<[u8; 32]> {
    match value {
        ScVal::Bytes(bytes) => bytes.as_slice().try_into().ok(),
        _ => None,
    }
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
/// `treasury` is `None` for a vault, which has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractConfig {
    pub admin: AccountAddress,
    pub operator: AccountAddress,
    pub seller: AccountAddress,
    pub treasury: Option<AccountAddress>,
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
        treasury: match field(b"treasury") {
            None => None,
            Some(_) => Some(account(b"treasury")?),
        },
        usdc,
        paused: *paused,
    })
}

/// The contract's running totals, as `get_totals` returns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Totals {
    /// Sum of every buyer's balance.
    pub liabilities: i128,
    /// Charged amounts the seller has not withdrawn.
    pub revenue: i128,
}

/// Decodes `get_totals`'s return value; `None` for any other shape.
#[must_use]
pub fn contract_totals(value: &ScVal) -> Option<Totals> {
    let ScVal::Map(Some(fields)) = value else { return None };
    let field = |name: &[u8]| {
        fields
            .iter()
            .find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
            .and_then(|f| i128_of(&f.val))
    };
    Some(Totals { liabilities: field(b"liabilities")?, revenue: field(b"revenue")? })
}

/// The roles and totals held in the contract's instance entry, read in one
/// ledger entry so both describe the same ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceState {
    pub config: ContractConfig,
    pub totals: Totals,
}

/// Decodes the contract instance entry read at [`PrepaidDeployment::instance_key`];
/// `None` if it is not this contract's layout. The layout is internal to the
/// contract and pinned by the contract's tests.
/// The hash of the Wasm the contract instance runs, from its instance entry.
#[must_use]
pub fn instance_wasm(entry: &LedgerEntryData) -> Option<[u8; 32]> {
    match entry {
        LedgerEntryData::ContractData(ContractDataEntry {
            val:
                ScVal::ContractInstance(ScContractInstance {
                    executable: ContractExecutable::Wasm(Hash(hash)),
                    ..
                }),
            ..
        }) => Some(*hash),
        _ => None,
    }
}

#[must_use]
pub fn instance_state(entry: &LedgerEntryData) -> Option<InstanceState> {
    let LedgerEntryData::ContractData(ContractDataEntry {
        val: ScVal::ContractInstance(instance),
        ..
    }) = entry
    else {
        return None;
    };
    let storage = instance.storage.as_ref()?;
    let slot = |name: &[u8]| {
        storage.iter().find_map(|entry| match &entry.key {
            ScVal::Vec(Some(ScVec(key)))
                if matches!(key.as_slice(), [ScVal::Symbol(s)] if s.0.as_slice() == name) =>
            {
                Some(&entry.val)
            }
            _ => None,
        })
    };
    Some(InstanceState {
        config: contract_config(slot(b"Config")?)?,
        totals: contract_totals(slot(b"Totals")?)?,
    })
}

/// The outcome held by a charge record read from the ledger; `None` if the
/// entry is not a charge record.
#[must_use]
pub fn charge_record(entry: &LedgerEntryData) -> Option<Outcome> {
    match entry {
        LedgerEntryData::ContractData(ContractDataEntry { val: ScVal::U32(code), .. }) => {
            Outcome::from_code(*code)
        }
        _ => None,
    }
}

/// The balance in an account entry read from the ledger; `None` if the entry
/// is not an account entry of this contract.
#[must_use]
pub fn account_balance(entry: &LedgerEntryData) -> Option<i128> {
    let LedgerEntryData::ContractData(ContractDataEntry { val: ScVal::Map(Some(fields)), .. }) =
        entry
    else {
        return None;
    };
    let field = |name: &[u8]| {
        fields.iter().find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
    };
    match &field(b"balance")?.val {
        ScVal::I128(Int128Parts { hi, lo }) => Some((i128::from(*hi) << 64) | i128::from(*lo)),
        _ => None,
    }
}

/// A balance entry of the USDC asset contract, as at
/// [`PrepaidDeployment::vault_balance_key`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SacBalance {
    pub amount: i128,
    /// The issuer has not frozen it; a frozen balance can neither send nor
    /// receive.
    pub authorized: bool,
    /// The issuer may claw it back.
    pub clawback: bool,
}

/// Decodes a USDC balance entry held by a contract; `None` for any other
/// entry.
#[must_use]
pub fn sac_balance(entry: &LedgerEntryData) -> Option<SacBalance> {
    let LedgerEntryData::ContractData(ContractDataEntry { val: ScVal::Map(Some(fields)), .. }) =
        entry
    else {
        return None;
    };
    let field = |name: &[u8]| {
        fields
            .iter()
            .find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
            .map(|f| &f.val)
    };
    let (ScVal::Bool(authorized), ScVal::Bool(clawback)) =
        (field(b"authorized")?, field(b"clawback")?)
    else {
        return None;
    };
    Some(SacBalance {
        amount: i128_of(field(b"amount")?)?,
        authorized: *authorized,
        clawback: *clawback,
    })
}

/// A vault account as the contract stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultAccount {
    pub balance: i128,
    /// The limit last stored; a pending lower limit may already be in force.
    pub cap: i128,
    /// A lower limit and the ledger it applies from.
    pub pending_cap: Option<(i128, u32)>,
    /// A requested exit: amount, destination and the ledger it unlocks at.
    pub exit: Option<(i128, ChainAddress, u32)>,
}

impl VaultAccount {
    /// The limit in force at ledger `now`.
    #[must_use]
    pub fn cap_at(&self, now: u32) -> i128 {
        match self.pending_cap {
            Some((cap, effective_at)) if now >= effective_at => cap,
            _ => self.cap,
        }
    }
}

/// Decodes a vault account entry; `None` if the entry is not one. The layout
/// is internal to the contract and pinned by the vault's tests.
#[must_use]
pub fn vault_account(entry: &LedgerEntryData) -> Option<VaultAccount> {
    let LedgerEntryData::ContractData(ContractDataEntry { val: ScVal::Map(Some(fields)), .. }) =
        entry
    else {
        return None;
    };
    let field = |name: &[u8]| {
        fields
            .iter()
            .find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
            .map(|f| &f.val)
    };
    // A `#[contracttype]` enum: a vector holding the variant's name and, for
    // a tuple variant, its value.
    let variant = |value: &ScVal| -> Option<Option<ScVal>> {
        let ScVal::Vec(Some(ScVec(items))) = value else { return None };
        match items.as_slice() {
            [ScVal::Symbol(name)] if name.0.as_slice() == b"None" => Some(None),
            [ScVal::Symbol(_), inner] => Some(Some(inner.clone())),
            _ => None,
        }
    };
    let map_field = |value: &ScVal, name: &[u8]| -> Option<ScVal> {
        let ScVal::Map(Some(map)) = value else { return None };
        map.iter()
            .find(|f| matches!(&f.key, ScVal::Symbol(s) if s.0.as_slice() == name))
            .map(|f| f.val.clone())
    };
    let pending_cap = match variant(field(b"pending_cap")?)? {
        None => None,
        Some(inner) => {
            let ScVal::U32(effective_at) = map_field(&inner, b"effective_at")? else { return None };
            Some((i128_of(&map_field(&inner, b"cap")?)?, effective_at))
        }
    };
    let exit = match variant(field(b"exit")?)? {
        None => None,
        Some(inner) => {
            let ScVal::U32(unlock_at) = map_field(&inner, b"unlock_at")? else { return None };
            Some((
                i128_of(&map_field(&inner, b"amount")?)?,
                chain_address_val(&map_field(&inner, b"destination")?)?,
                unlock_at,
            ))
        }
    };
    Some(VaultAccount {
        balance: i128_of(field(b"balance")?)?,
        cap: i128_of(field(b"cap")?)?,
        pending_cap,
        exit,
    })
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

fn account_val(address: &impl ScAddressOf) -> ScVal {
    ScVal::Address(address.sc_address())
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

/// The contract's `RecurringCharge`, a struct with named fields, which
/// encodes as a map keyed by field name in sorted order.
fn recurring_charge_val(charge: &RecurringChargeRequest) -> ScVal {
    let field = |name: &str, val: ScVal| ScMapEntry { key: symbol_val(name), val };
    ScVal::Map(Some(ScMap(
        VecM::try_from(vec![
            field("amount", i128_val(charge.amount)),
            field("charge_id", bytes_val(&charge.charge_id)),
            field("cycle", ScVal::U32(charge.cycle)),
            field("last_ledger", ScVal::U32(charge.last_ledger)),
            field("mandate_id", bytes_val(&charge.mandate_id)),
            field("owner", account_val(&charge.owner)),
        ])
        .expect("invariant: six fields"),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(topics: &[&str], value: &str) -> Option<LedgerEvent> {
        use stellar_xdr::{Limits, ReadXdr};
        let topics: Vec<ScVal> =
            topics.iter().map(|t| ScVal::from_xdr_base64(t, Limits::none()).unwrap()).collect();
        ledger_event(&topics, &ScVal::from_xdr_base64(value, Limits::none()).unwrap())
    }

    fn hex32(text: &str) -> [u8; 32] {
        let bytes: Vec<u8> =
            (0..64).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect();
        bytes.try_into().unwrap()
    }

    // Events returned by testnet `getEvents` for prepaid ledger deployments
    // (the withdrawal and the earlier-layout charges from an earlier contract
    // version); the expected values are the node's own `xdrFormat: "json"`
    // rendering of the same events.
    #[test]
    fn test_live_withdraw_event_decodes() {
        let account: AccountAddress =
            "GCBD53TBX6VDTD7M34MYDLFBFX4G7WZRSO6T24CMGVVXV3X52DQOZ4MV".parse().unwrap();
        assert_eq!(
            live(
                &[
                    "AAAADwAAAAh3aXRoZHJhdw==",
                    "AAAAEgAAAAAAAAAAgj7uYb+qOY/s3xmBrKEt+G/bMZO9PXBMNWt67v3Q4Ow="
                ],
                "AAAAEAAAAAEAAAADAAAAEgAAAAAAAAAAgj7uYb+qOY/s3xmBrKEt+G/bMZO9PXBMNWt67v3Q4OwAAAAKAAAAAAAAAAAAAAAAAAJJ8AAAAA0AAAAgRYcq/aQ7rFEucYIUND1XdTpiPMoQoFPIyHqS+FfpWCo="
            ),
            Some(LedgerEvent::Withdrawn {
                owner: ChainAddress::Account(account.clone()),
                destination: ChainAddress::Account(account),
                amount: 150_000,
                withdrawal_id: hex32(
                    "45872afda43bac512e718214343d57753a623cca10a053c8c87a92f857e9582a"
                ),
            })
        );
    }

    #[test]
    fn test_live_deposit_event_decodes() {
        assert_eq!(
            live(
                &[
                    "AAAADwAAAAdkZXBvc2l0AA==",
                    "AAAAEgAAAAAAAAAApDDeli/J1RNw3at1WfsXX/9WgFg2KxTvYMoyskV5ZJc="
                ],
                "AAAAEAAAAAEAAAACAAAACgAAAAAAAAAAAAAAAAAEk+AAAAANAAAAILtpCxgjkLJuwgyIqP1zjkR1j5RpJIuZXkRPVPAfcmTf"
            ),
            Some(LedgerEvent::Deposited {
                owner: ChainAddress::Account(
                    "GCSDBXUWF7E5KE3Q3WVXKWP3C5P76VUALA3CWFHPMDFDFMSFPFSJPW4H".parse().unwrap()
                ),
                amount: 300_000,
                deposit_id: hex32(
                    "bb690b182390b26ec20c88a8fd738e44758f9469248b995e444f54f01f7264df"
                ),
            })
        );
    }

    /// A `charges` event of an earlier contract version, which named each
    /// charge by a `u64` sequence number, is not read as the current layout.
    #[test]
    fn test_live_charges_event_of_an_earlier_layout_is_not_decoded() {
        assert_eq!(
            live(
                &["AAAADwAAAAdjaGFyZ2VzAA=="],
                "AAAAEAAAAAEAAAABAAAAEAAAAAEAAAAEAAAAEgAAAAAAAAAApDDeli/J1RNw3at1WfsXX/9WgFg2KxTvYMoyskV5ZJcAAAAFAAAAAAAAAAIAAAAKAAAAAAAAAAAAAAAAAAGGoAAAAAMAAAAA"
            ),
            None
        );
    }

    /// The vault's co-signer authorizes the withdrawal alone, and a vault
    /// deposit's transfer goes to the vault itself: the trees are exactly
    /// these, as the gateway compares what it prepared byte for byte.
    #[test]
    fn test_vault_trees_are_exactly_the_calls_needed() {
        let buyer: AccountAddress =
            "GCSDBXUWF7E5KE3Q3WVXKWP3C5P76VUALA3CWFHPMDFDFMSFPFSJPW4H".parse().unwrap();
        let vault = PrepaidDeployment { contract: [1; 32], usdc: [2; 32], custody: Custody::Vault };
        let withdraw = WithdrawIntent {
            owner: ChainAddress::Account(buyer.clone()),
            amount: 5,
            destination: ChainAddress::Account(buyer.clone()),
            withdrawal_id: [3; 32],
        };
        assert_eq!(
            vault.cosigner_withdraw_authorization(&withdraw),
            invocation(vault.withdraw_call(&withdraw), vec![])
        );
        let deposit = DepositIntent {
            owner: ChainAddress::Account(buyer.clone()),
            amount: 5,
            deposit_id: [4; 32],
            cap: Some(7),
        };
        let vault_address = ScAddress::Contract(ContractId(Hash([1; 32])));
        assert_eq!(
            vault.deposit_authorization(&deposit),
            invocation(
                vault.deposit_call(&deposit),
                vec![invocation(usdc_transfer_call([2; 32], &buyer, &vault_address, 5), vec![])],
            )
        );
        assert_eq!(vault.deposit_call(&deposit).args.len(), 4);
        // A treasury deployment takes no limit and transfers to the treasury.
        let treasury = PrepaidDeployment { custody: Custody::Treasury(buyer.clone()), ..vault };
        assert_eq!(treasury.deposit_call(&deposit).args.len(), 3);
    }

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
