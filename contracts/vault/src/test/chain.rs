//! The services build every call and authorization tree for the vault with
//! `fermah_pay_stellar_chain::prepaid`, and decode its events and entries
//! there. These tests pin that module against the contract: classic accounts
//! sign the trees it builds with real Ed25519 keys, the host verifies them,
//! and what the contract emits and stores is decoded back.

extern crate std;

use std::cell::Cell;
use std::rc::Rc;

use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::prepaid::{
    AdminAction, ChargeRequest, Custody, DepositIntent, LedgerEvent, Outcome as Decoded,
    PrepaidDeployment, RevenueWithdrawIntent, WithdrawIntent, batch_outcomes, instance_state,
    ledger_event, vault_account, vault_constructor_args,
};
use fermah_pay_stellar_domain::{AccountAddress, ChainAddress};
use soroban_sdk::testutils::{Events as _, Ledger as _};
use soroban_sdk::xdr;
use soroban_sdk::{Address, Env, Symbol, TryFromVal, Val};

use super::USDC;
use crate::*;

struct Party {
    key: SecretKey,
    address: Address,
}

impl Party {
    fn account(&self) -> AccountAddress {
        self.key.address()
    }

    fn chain(&self) -> ChainAddress {
        ChainAddress::Account(self.key.address())
    }
}

struct World {
    env: Env,
    vault: Address,
    usdc: Address,
    asset: xdr::Asset,
    admin: Party,
    operator: Party,
    seller: Party,
    nonce: Cell<i64>,
}

fn account_id(key: &SecretKey) -> xdr::AccountId {
    xdr::AccountId(xdr::PublicKey::PublicKeyTypeEd25519(xdr::Uint256(*key.address().public_key())))
}

fn contract_bytes(address: &Address) -> [u8; 32] {
    match xdr::ScAddress::from(address) {
        xdr::ScAddress::Contract(xdr::ContractId(xdr::Hash(bytes))) => bytes,
        other => panic!("not a contract address: {other:?}"),
    }
}

/// A classic account with master weight 1 and a USDC trustline.
fn add_party(env: &Env, asset: &xdr::Asset, usdc: i64) -> Party {
    let key = SecretKey::generate().unwrap();
    let account_key =
        Rc::new(xdr::LedgerKey::Account(xdr::LedgerKeyAccount { account_id: account_id(&key) }));
    let account = Rc::new(xdr::LedgerEntry {
        last_modified_ledger_seq: 0,
        data: xdr::LedgerEntryData::Account(xdr::AccountEntry {
            account_id: account_id(&key),
            balance: 100 * 10_000_000,
            seq_num: xdr::SequenceNumber(0),
            num_sub_entries: 1,
            inflation_dest: None,
            flags: 0,
            home_domain: xdr::String32::default(),
            thresholds: xdr::Thresholds([1, 0, 0, 0]),
            signers: xdr::VecM::default(),
            ext: xdr::AccountEntryExt::V0,
        }),
        ext: xdr::LedgerEntryExt::V0,
    });
    env.host().add_ledger_entry(&account_key, &account, None).unwrap();
    let xdr::Asset::CreditAlphanum4(alpha) = asset else { panic!("test asset is alphanum4") };
    let line = xdr::TrustLineAsset::CreditAlphanum4(alpha.clone());
    let trustline_key = Rc::new(xdr::LedgerKey::Trustline(xdr::LedgerKeyTrustLine {
        account_id: account_id(&key),
        asset: line.clone(),
    }));
    let trustline = Rc::new(xdr::LedgerEntry {
        last_modified_ledger_seq: 0,
        data: xdr::LedgerEntryData::Trustline(xdr::TrustLineEntry {
            account_id: account_id(&key),
            asset: line,
            balance: usdc,
            limit: i64::MAX,
            flags: xdr::TrustLineFlags::AuthorizedFlag as u32,
            ext: xdr::TrustLineEntryExt::V0,
        }),
        ext: xdr::LedgerEntryExt::V0,
    });
    env.host().add_ledger_entry(&trustline_key, &trustline, None).unwrap();
    let address = Address::from_str(env, key.address().as_str());
    Party { key, address }
}

/// A vault deployed with the constructor arguments the chain module builds.
fn world() -> World {
    let env = Env::new_with_config(soroban_sdk::testutils::EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    env.ledger().set_sequence_number(10_000);
    let sac = env.register_stellar_asset_contract_v2(
        <Address as soroban_sdk::testutils::Address>::generate(&env),
    );
    let asset = sac.asset();
    let (admin, operator, seller) =
        (add_party(&env, &asset, 0), add_party(&env, &asset, 0), add_party(&env, &asset, 0));
    let args: soroban_sdk::Vec<Val> = vault_constructor_args(
        &admin.account(),
        &operator.account(),
        &seller.account(),
        contract_bytes(&sac.address()),
        USDC / 10,
        100 * USDC,
    )
    .iter()
    .fold(soroban_sdk::Vec::new(&env), |mut args, arg| {
        args.push_back(Val::try_from_val(&env, arg).unwrap());
        args
    });
    let vault = env.register(PrepaidVault, args);
    World { env, vault, usdc: sac.address(), asset, admin, operator, seller, nonce: Cell::new(1) }
}

impl World {
    fn deployment(&self) -> PrepaidDeployment {
        PrepaidDeployment {
            contract: contract_bytes(&self.vault),
            usdc: contract_bytes(&self.usdc),
            custody: Custody::Vault,
        }
    }

    fn party(&self, usdc: i128) -> Party {
        add_party(&self.env, &self.asset, i64::try_from(usdc).unwrap())
    }

    fn usdc_of(&self, party: &Party) -> i128 {
        soroban_sdk::token::Client::new(&self.env, &self.usdc).balance(&party.address)
    }

    fn signed(
        &self,
        signer: &Party,
        invocation: xdr::SorobanAuthorizedInvocation,
    ) -> xdr::SorobanAuthorizationEntry {
        let nonce = self.nonce.get();
        self.nonce.set(nonce + 1);
        let entry = xdr::SorobanAuthorizationEntry {
            credentials: xdr::SorobanCredentials::AddressV2(xdr::SorobanAddressCredentials {
                address: xdr::ScAddress::Account(account_id(&signer.key)),
                nonce,
                signature_expiration_ledger: self.env.ledger().sequence() + 100,
                signature: xdr::ScVal::Void,
            }),
            root_invocation: invocation,
        };
        sign_entry(&entry, self.env.ledger().network_id().to_array(), &[&signer.key]).unwrap()
    }

    /// Invokes the call the chain module built, with exactly `auths`.
    fn invoke(
        &self,
        call: &xdr::InvokeContractArgs,
        auths: &[xdr::SorobanAuthorizationEntry],
    ) -> Result<Val, soroban_sdk::Error> {
        assert_eq!(xdr::ScAddress::from(&self.vault), call.contract_address);
        let args: soroban_sdk::Vec<Val> =
            call.args.iter().fold(soroban_sdk::Vec::new(&self.env), |mut args, arg| {
                args.push_back(Val::try_from_val(&self.env, arg).unwrap());
                args
            });
        self.env.set_auths(auths);
        match self.env.try_invoke_contract::<Val, soroban_sdk::Error>(
            &self.vault,
            &Symbol::new(&self.env, &call.function_name.to_utf8_string_lossy()),
            args,
        ) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(conversion)) => panic!("return value conversion failed: {conversion:?}"),
            Err(Ok(error)) => Err(error),
            Err(Err(invoke)) => panic!("unclassified invoke error: {invoke:?}"),
        }
    }

    /// The events the last call emitted from the vault, decoded by the
    /// chain module.
    fn decoded_events(&self) -> std::vec::Vec<LedgerEvent> {
        let events = self.env.events().all();
        events
            .events()
            .iter()
            .filter(|event| {
                event.contract_id == Some(xdr::ContractId(xdr::Hash(contract_bytes(&self.vault))))
            })
            .map(|event| {
                let xdr::ContractEventBody::V0(body) = &event.body;
                ledger_event(&body.topics, &body.data)
                    .unwrap_or_else(|| panic!("undecoded event: {body:?}"))
            })
            .collect()
    }

    fn entry(&self, key: &xdr::LedgerKey) -> Option<xdr::LedgerEntryData> {
        let snapshot = self.env.to_ledger_snapshot();
        snapshot.ledger_entries.iter().find(|(k, _)| **k == *key).map(|(_, (e, _))| e.data.clone())
    }

    fn deposit(&self, buyer: &Party, amount: i128, id: u8, cap: Option<i128>) {
        let intent = DepositIntent { owner: buyer.chain(), amount, deposit_id: [id; 32], cap };
        let deployment = self.deployment();
        let auth = self.signed(buyer, deployment.deposit_authorization(&intent));
        self.invoke(&deployment.deposit_call(&intent), &[auth]).unwrap();
    }

    fn charge(&self, buyer: &Party, id: u8, amount: i128) -> ChargeRequest {
        ChargeRequest {
            owner: buyer.chain(),
            charge_id: [id; 32],
            amount,
            last_ledger: self.env.ledger().sequence() + 100,
            day: self.env.ledger().timestamp() / 86_400,
        }
    }
}

#[test]
fn test_deposit_with_a_limit_signed_as_built() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 10 * USDC, 1, Some(2 * USDC));
    assert_eq!(
        w.decoded_events(),
        [
            LedgerEvent::CapRaised { owner: buyer.chain(), cap: 2 * USDC },
            LedgerEvent::Deposited { owner: buyer.chain(), amount: 10 * USDC, deposit_id: [1; 32] },
        ]
    );
    let account = w.entry(&w.deployment().account_key(&buyer.account())).unwrap();
    assert_eq!(
        vault_account(&account),
        Some(fermah_pay_stellar_chain::prepaid::VaultAccount {
            balance: 10 * USDC,
            cap: 2 * USDC,
            pending_cap: None,
            exit: None,
        })
    );
    assert!(w.entry(&w.deployment().deposit_key(&buyer.account(), &[1; 32])).is_some());
    // The USDC the vault holds, at the key the worker keeps alive and the
    // observer reconciles.
    let held = w.entry(&w.deployment().vault_balance_key()).unwrap();
    assert_eq!(
        fermah_pay_stellar_chain::prepaid::sac_balance(&held),
        Some(fermah_pay_stellar_chain::prepaid::SacBalance {
            amount: 10 * USDC,
            authorized: true,
            clawback: false,
        })
    );
    // A deposit signed for the transfer to another account is refused.
    let intent =
        DepositIntent { owner: buyer.chain(), amount: USDC, deposit_id: [2; 32], cap: None };
    let elsewhere =
        PrepaidDeployment { custody: Custody::Treasury(w.seller.account()), ..w.deployment() };
    let auth = w.signed(&buyer, elsewhere.deposit_authorization(&intent));
    assert!(w.invoke(&w.deployment().deposit_call(&intent), &[auth]).is_err());
}

#[test]
fn test_charges_with_their_day_signed_by_the_operator() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 10 * USDC, 1, Some(2 * USDC));
    let charges = [w.charge(&buyer, 1, 2 * USDC), w.charge(&buyer, 2, 1)];
    let deployment = w.deployment();
    let auth = w.signed(&w.operator, deployment.charge_batch_authorization(&charges));
    let value = w.invoke(&deployment.charge_batch_call(&charges), &[auth]).unwrap();
    let returned = xdr::ScVal::try_from_val(&w.env, &value).unwrap();
    assert_eq!(batch_outcomes(&returned), Some(std::vec![Decoded::Charged, Decoded::AboveCap]));
    let record = w.entry(&deployment.charge_record_key(&buyer.account(), &[2; 32])).unwrap();
    assert_eq!(fermah_pay_stellar_chain::prepaid::charge_record(&record), Some(Decoded::AboveCap));
}

#[test]
fn test_cooperative_withdrawal_signed_by_buyer_and_operator() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 10 * USDC, 1, None);
    let intent = WithdrawIntent {
        owner: buyer.chain(),
        amount: 4 * USDC,
        destination: buyer.chain(),
        withdrawal_id: [1; 32],
    };
    let deployment = w.deployment();
    let auths = [
        w.signed(&buyer, deployment.owner_withdraw_authorization(&intent)),
        w.signed(&w.operator, deployment.cosigner_withdraw_authorization(&intent)),
    ];
    // The buyer's signature alone is not enough.
    assert!(w.invoke(&deployment.withdraw_call(&intent), &auths[..1]).is_err());
    w.invoke(&deployment.withdraw_call(&intent), &auths).unwrap();
    assert_eq!(w.usdc_of(&buyer), 4 * USDC);
    assert!(w.entry(&deployment.withdrawal_key(&buyer.account(), &[1; 32])).is_some());
}

#[test]
fn test_limit_and_exit_signed_by_the_buyer_and_exit_sent_by_anyone() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 10 * USDC, 1, Some(5 * USDC));
    let deployment = w.deployment();
    let now = w.env.ledger().sequence();

    let auth = w.signed(&buyer, deployment.set_cap_authorization(&buyer.account(), USDC));
    w.invoke(&deployment.set_cap_call(&buyer.account(), USDC), &[auth]).unwrap();
    assert_eq!(
        w.decoded_events(),
        [LedgerEvent::CapLowered {
            owner: buyer.chain(),
            cap: USDC,
            effective_at: now + NOTICE_LEDGERS,
        }]
    );

    let wallet = w.party(0);
    let auth = w.signed(
        &buyer,
        deployment.request_exit_authorization(&buyer.account(), 3 * USDC, &wallet.account()),
    );
    w.invoke(&deployment.request_exit_call(&buyer.account(), 3 * USDC, &wallet.account()), &[auth])
        .unwrap();
    assert_eq!(
        w.decoded_events(),
        [LedgerEvent::ExitRequested {
            owner: buyer.chain(),
            amount: 3 * USDC,
            destination: wallet.chain(),
            unlock_at: now + NOTICE_LEDGERS,
        }]
    );
    let account =
        vault_account(&w.entry(&deployment.account_key(&buyer.account())).unwrap()).unwrap();
    assert_eq!(account.pending_cap, Some((USDC, now + NOTICE_LEDGERS)));
    assert_eq!(account.exit, Some((3 * USDC, wallet.chain(), now + NOTICE_LEDGERS)));
    assert_eq!((account.cap_at(now), account.cap_at(now + NOTICE_LEDGERS)), (5 * USDC, USDC));

    w.env.ledger().set_sequence_number(now + NOTICE_LEDGERS);
    w.invoke(&deployment.exit_call(&buyer.account()), &[]).unwrap();
    assert_eq!(
        w.decoded_events(),
        [LedgerEvent::Exited {
            owner: buyer.chain(),
            destination: wallet.chain(),
            amount: 3 * USDC
        }]
    );
    assert_eq!(w.usdc_of(&wallet), 3 * USDC);
}

#[test]
fn test_revenue_signed_by_the_seller_alone() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 10 * USDC, 1, Some(5 * USDC));
    let deployment = w.deployment();
    let charges = [w.charge(&buyer, 1, 3 * USDC)];
    let auth = w.signed(&w.operator, deployment.charge_batch_authorization(&charges));
    w.invoke(&deployment.charge_batch_call(&charges), &[auth]).unwrap();
    let intent = RevenueWithdrawIntent {
        destination: w.seller.account(),
        amount: 3 * USDC,
        withdrawal_id: [1; 32],
    };
    assert_eq!(deployment.treasury_revenue_authorization(&intent), None);
    let auth = w.signed(&w.seller, deployment.seller_revenue_authorization(&intent));
    w.invoke(&deployment.withdraw_revenue_call(&intent), &[auth]).unwrap();
    assert_eq!(w.usdc_of(&w.seller), 3 * USDC);
}

#[test]
fn test_admin_actions_signed_as_built() {
    let w = world();
    let contract = contract_bytes(&w.vault);
    let run = |action: AdminAction| {
        let auths: std::vec::Vec<_> = action
            .authorizations(contract, &w.admin.account())
            .into_iter()
            .map(|(_, tree)| w.signed(&w.admin, tree))
            .collect();
        w.invoke(&action.call(contract), &auths)
    };
    let now = w.env.ledger().sequence();
    run(AdminAction::ProposeUpgrade { wasm_hash: [7; 32] }).unwrap();
    assert_eq!(
        w.decoded_events(),
        [LedgerEvent::UpgradeProposed {
            wasm_hash: [7; 32],
            effective_at: now + UPGRADE_DELAY_LEDGERS,
        }]
    );
    assert_eq!(
        run(AdminAction::InstallUpgrade).err(),
        Some(soroban_sdk::Error::from_contract_error(Error::UpgradeLocked as u32))
    );
    // The pending proposal, as the observer reads it from the instance.
    let state = instance_state(&w.entry(&w.deployment().instance_key()).unwrap()).unwrap();
    assert_eq!(state.pending_upgrade, Some(([7; 32], now + UPGRADE_DELAY_LEDGERS)));
    run(AdminAction::CancelUpgrade).unwrap();
    let state = instance_state(&w.entry(&w.deployment().instance_key()).unwrap()).unwrap();
    assert_eq!(state.pending_upgrade, None);
    assert_eq!(w.decoded_events(), [LedgerEvent::UpgradeCancelled { wasm_hash: [7; 32] }]);
    run(AdminAction::SetLaunchLimits { limits: Some((5 * USDC, 50 * USDC)) }).unwrap();
    run(AdminAction::SetLaunchLimits { limits: None }).unwrap();
    assert_eq!(
        w.decoded_events(),
        [LedgerEvent::LaunchLimitsChanged { previous: Some((5 * USDC, 50 * USDC)), current: None }]
    );
}

#[test]
fn test_instance_decodes_without_a_treasury() {
    let w = world();
    let buyer = w.party(10 * USDC);
    w.deposit(&buyer, 10 * USDC, 1, None);
    let state = instance_state(&w.entry(&w.deployment().instance_key()).unwrap()).unwrap();
    assert_eq!(
        (state.config.admin, state.config.operator, state.config.seller, state.config.treasury),
        (w.admin.account(), w.operator.account(), w.seller.account(), None)
    );
    assert_eq!((state.totals.liabilities, state.totals.revenue), (10 * USDC, 0));
}

#[test]
fn test_the_chain_module_mirrors_the_vault_constants() {
    use fermah_pay_stellar_chain::prepaid as chain;
    assert_eq!(
        (
            chain::VAULT_NOTICE_LEDGERS,
            chain::MAX_BATCH as u32,
            chain::MAX_CHARGE_WINDOW,
            chain::VAULT_UPGRADE_WINDOW_LEDGERS
        ),
        (NOTICE_LEDGERS, MAX_BATCH, MAX_CHARGE_WINDOW, UPGRADE_WINDOW_LEDGERS)
    );
}
