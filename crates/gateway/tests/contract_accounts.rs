//! Buyers whose wallet is a contract account (`C...`). Their authorization
//! entries carry a credential only the account's `__check_auth`
//! interprets, so the API checks everything else itself and asks the
//! network, by an enforcing simulation, whether the signature holds.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Harness, Tenant, assert_refused, authed, start};
use fermah_pay_stellar_chain::prepaid::{PrepaidDeployment, WithdrawIntent};
use fermah_pay_stellar_chain::stellar_xdr::{
    BytesM, Limits, ReadXdr, ScAddress, ScBytes, ScVal, SorobanAuthorizationEntry,
    SorobanAuthorizedFunction, SorobanCredentials, WriteXdr,
};
use fermah_pay_stellar_domain::{AccountAddress, ChainAddress, Network};
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_gateway::ledger::BuyerCall;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    CreateBuyerRequest, CreateChargeRequest, DepositState, PrepareDepositRequest,
    PrepareMandateRequest, PrepareWithdrawalRequest, SubmitDepositRequest, SubmitWithdrawalRequest,
    WithdrawalState,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tonic::Code;
use tonic::transport::Channel;

const CONTRACT: [u8; 32] = [7; 32];
const WALLET: [u8; 32] = [21; 32];

fn wallet() -> ChainAddress {
    ChainAddress::Contract(WALLET)
}

fn treasury() -> AccountAddress {
    AccountAddress::from_public_key([11; 32])
}

async fn ledger(h: &Harness) -> LedgerServiceClient<Channel> {
    LedgerServiceClient::new(h.channel().await)
}

/// A bound tenant with one buyer whose wallet is the contract account.
async fn setup(h: &Harness) -> (Tenant, String) {
    let t = h.tenant("shop", "main", Network::Testnet).await;
    let binding = LedgerBinding {
        contract: stellar_strkey::Contract(CONTRACT).to_string().to_string(),
        treasury: treasury(),
        operator: AccountAddress::from_public_key([12; 32]),
    };
    issuance::bind_ledger_contract(&h.issuer, t.deployment_id, &binding).await.unwrap();
    let buyer = BuyerServiceClient::new(h.channel().await)
        .create_buyer(authed(
            CreateBuyerRequest {
                external_ref: "smart-wallet".to_owned(),
                wallet_address: wallet().to_string(),
            },
            &t.token,
        ))
        .await
        .unwrap()
        .into_inner()
        .buyer
        .unwrap();
    assert_eq!(buyer.wallet_address, wallet().to_string());
    (t, buyer.buyer_id)
}

/// The deployment the API pins: the bound contract, Circle's USDC as the
/// binding records it, and the treasury.
async fn pinned(h: &Harness, t: &Tenant) -> PrepaidDeployment {
    let usdc: String = sqlx::query_scalar(
        "SELECT usdc_address FROM pay_stellar.ledger_contracts WHERE seller_deployment_id = $1",
    )
    .bind(t.deployment_id)
    .fetch_one(&h.owner)
    .await
    .unwrap();
    let usdc = stellar_strkey::Contract::from_string(&usdc).unwrap().0;
    PrepaidDeployment { contract: CONTRACT, usdc, treasury: treasury() }
}

fn decode(xdr: &str) -> SorobanAuthorizationEntry {
    SorobanAuthorizationEntry::from_xdr_base64(xdr, Limits::none()).unwrap()
}

/// `entry` carrying `credential` as its signature: what a smart wallet
/// returns, opaque to anyone but its `__check_auth`.
fn with_credential(entry: &SorobanAuthorizationEntry, credential: ScVal) -> String {
    let mut signed = entry.clone();
    match &mut signed.credentials {
        SorobanCredentials::Address(creds) | SorobanCredentials::AddressV2(creds) => {
            creds.signature = credential;
        }
        _ => panic!("not an address entry"),
    }
    signed.to_xdr_base64(Limits::none()).unwrap()
}

fn credential() -> ScVal {
    ScVal::Bytes(ScBytes(BytesM::try_from(vec![5_u8; 64]).unwrap()))
}

fn call_of(
    entry: &SorobanAuthorizationEntry,
) -> fermah_pay_stellar_chain::stellar_xdr::InvokeContractArgs {
    let SorobanAuthorizedFunction::ContractFn(call) = &entry.root_invocation.function else {
        panic!("not a contract call")
    };
    call.clone()
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_contract_account_deposit_is_accepted_only_when_the_network_accepts_it(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, buyer_id) = setup(&h).await;
    let prepare = |key: &str| PrepareDepositRequest {
        buyer_id: buyer_id.clone(),
        amount: 1_000_000,
        idempotency_key: key.to_owned(),
    };
    let refused = ledger(&h)
        .await
        .prepare_deposit(authed(prepare("dep-refused"), &t.token))
        .await
        .unwrap()
        .into_inner()
        .deposit
        .unwrap();
    let entry = decode(&refused.authorization_entry_xdr);
    let SorobanCredentials::AddressV2(creds) = &entry.credentials else { panic!() };
    assert_eq!(
        creds.address,
        ScAddress::Contract(fermah_pay_stellar_chain::stellar_xdr::ContractId(
            fermah_pay_stellar_chain::stellar_xdr::Hash(WALLET),
        ))
    );

    // The network refuses the authorization: so does the API, and nothing
    // is stored.
    h.ledger.simulations().answer =
        BuyerCall::Refused("HostError: Error(Auth, InvalidAction)".to_owned());
    let status = ledger(&h)
        .await
        .submit_deposit(authed(
            SubmitDepositRequest {
                deposit_id: refused.deposit_id.clone(),
                signed_authorization_entry_xdr: with_credential(&entry, credential()),
            },
            &t.token,
        ))
        .await
        .unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "authorization_refused");

    // The same request, the network accepting.
    h.ledger.simulations().answer = BuyerCall::Accepted { resource_fee: 1_000 };
    let signed = with_credential(&entry, credential());
    let deposit = ledger(&h)
        .await
        .submit_deposit(authed(
            SubmitDepositRequest {
                deposit_id: refused.deposit_id.clone(),
                signed_authorization_entry_xdr: signed.clone(),
            },
            &t.token,
        ))
        .await
        .unwrap()
        .into_inner()
        .deposit
        .unwrap();
    assert_eq!(deposit.state(), DepositState::Signed);

    // Both times the call itself was simulated from the treasury with the
    // buyer's entry alone.
    let simulations = h.ledger.simulations();
    assert_eq!(simulations.seen.len(), 2);
    for (source, call, auth) in &simulations.seen {
        assert_eq!(source, &treasury());
        assert_eq!(call, &call_of(&entry));
        assert_eq!(auth, &vec![decode(&signed)]);
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_only_the_buyers_part_of_a_failed_simulation_refuses_the_entry(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    use fermah_pay_stellar_gateway::submission::DEFAULT_MAX_BUYER_RESOURCE_FEE;
    let h = start(opts, connect, Network::Testnet).await;
    let (t, buyer_id) = setup(&h).await;
    let cases = [
        // The wallet's code would cost the operator too much.
        (
            "costly",
            BuyerCall::Accepted { resource_fee: DEFAULT_MAX_BUYER_RESOURCE_FEE + 1 },
            Some("wallet_too_costly"),
        ),
        (
            "at-the-bound",
            BuyerCall::Accepted { resource_fee: DEFAULT_MAX_BUYER_RESOURCE_FEE },
            None,
        ),
        // The call's own failure, not the buyer's authorization: stored, and
        // the worker decides, as for a classic account.
        ("short", BuyerCall::Refused("HostError: Error(Contract, #10)".to_owned()), None),
        // Archived state: stored, and the worker restores it.
        ("archived", BuyerCall::RestoreRequired, None),
    ];
    for (key, answer, refusal) in cases {
        let deposit = ledger(&h)
            .await
            .prepare_deposit(authed(
                PrepareDepositRequest {
                    buyer_id: buyer_id.clone(),
                    amount: 1_000_000,
                    idempotency_key: key.to_owned(),
                },
                &t.token,
            ))
            .await
            .unwrap()
            .into_inner()
            .deposit
            .unwrap();
        h.ledger.simulations().answer = answer;
        let entry = decode(&deposit.authorization_entry_xdr);
        let submitted = ledger(&h)
            .await
            .submit_deposit(authed(
                SubmitDepositRequest {
                    deposit_id: deposit.deposit_id,
                    signed_authorization_entry_xdr: with_credential(&entry, credential()),
                },
                &t.token,
            ))
            .await;
        match refusal {
            Some(reason) => {
                assert_refused(&submitted.unwrap_err(), Code::FailedPrecondition, reason)
            }
            None => assert_eq!(
                submitted.unwrap().into_inner().deposit.unwrap().state(),
                DepositState::Signed,
                "{key}"
            ),
        }
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_contract_account_entry_for_another_call_is_refused_before_the_network(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, buyer_id) = setup(&h).await;
    let deposit = ledger(&h)
        .await
        .prepare_deposit(authed(
            PrepareDepositRequest { buyer_id, amount: 1_000_000, idempotency_key: "d".to_owned() },
            &t.token,
        ))
        .await
        .unwrap()
        .into_inner()
        .deposit
        .unwrap();
    let mut entry = decode(&deposit.authorization_entry_xdr);
    if let SorobanCredentials::AddressV2(creds) = &mut entry.credentials {
        creds.address = ScAddress::Contract(fermah_pay_stellar_chain::stellar_xdr::ContractId(
            fermah_pay_stellar_chain::stellar_xdr::Hash([22; 32]),
        ));
    }
    let status = ledger(&h)
        .await
        .submit_deposit(authed(
            SubmitDepositRequest {
                deposit_id: deposit.deposit_id,
                signed_authorization_entry_xdr: with_credential(&entry, credential()),
            },
            &t.token,
        ))
        .await
        .unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "authorization_mismatch");
    assert!(h.ledger.simulations().seen.is_empty());
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_contract_account_withdraws_to_itself_with_the_treasury_authorizing_the_transfer(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, buyer_id) = setup(&h).await;
    sqlx::query("UPDATE pay_stellar.buyers SET available = 5000000")
        .execute(&h.owner)
        .await
        .unwrap();
    let mut api = ledger(&h).await;
    // Charges against its balance are admitted like any buyer's.
    api.create_charge(authed(
        CreateChargeRequest {
            buyer_id: buyer_id.clone(),
            amount: 1_000_000,
            idempotency_key: "c-1".to_owned(),
        },
        &t.token,
    ))
    .await
    .unwrap();
    let withdrawal = api
        .prepare_withdrawal(authed(
            PrepareWithdrawalRequest {
                buyer_id,
                amount: 2_000_000,
                destination: String::new(),
                idempotency_key: "w-1".to_owned(),
            },
            &t.token,
        ))
        .await
        .unwrap()
        .into_inner()
        .withdrawal
        .unwrap();
    assert_eq!(withdrawal.destination, wallet().to_string());
    let entry = decode(&withdrawal.authorization_entry_xdr);
    let signed = with_credential(&entry, credential());
    let withdrawal = api
        .submit_withdrawal(authed(
            SubmitWithdrawalRequest {
                withdrawal_id: withdrawal.withdrawal_id,
                signed_authorization_entry_xdr: signed.clone(),
            },
            &t.token,
        ))
        .await
        .unwrap()
        .into_inner()
        .withdrawal
        .unwrap();
    assert_eq!(withdrawal.state(), WithdrawalState::Signed);

    // The simulation carried the buyer's entry and the treasury's
    // authorization of the withdrawal and its transfer out, given by the
    // treasury as the transaction's source.
    let deployment = pinned(&h, &t).await;
    let withdrawal_id = withdrawal_id_of(&entry);
    let intent =
        WithdrawIntent { owner: wallet(), amount: 2_000_000, destination: wallet(), withdrawal_id };
    assert_eq!(entry.root_invocation, deployment.owner_withdraw_authorization(&intent));
    let simulations = h.ledger.simulations();
    let [(source, call, auth)] = simulations.seen.as_slice() else { panic!("one simulation") };
    assert_eq!(source, &treasury());
    assert_eq!(call, &deployment.withdraw_call(&intent));
    assert_eq!(
        auth,
        &vec![
            decode(&signed),
            SorobanAuthorizationEntry {
                credentials: SorobanCredentials::SourceAccount,
                root_invocation: deployment.treasury_withdraw_authorization(&intent),
            },
        ]
    );
}

/// The withdrawal identifier the entry's call carries last.
fn withdrawal_id_of(entry: &SorobanAuthorizationEntry) -> [u8; 32] {
    let call = call_of(entry);
    let Some(ScVal::Bytes(ScBytes(bytes))) = call.args.last() else { panic!("no identifier") };
    bytes.as_slice().try_into().unwrap()
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_contract_account_cannot_authorize_a_mandate(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, buyer_id) = setup(&h).await;
    let status = ledger(&h)
        .await
        .prepare_mandate(authed(
            PrepareMandateRequest {
                buyer_id,
                amount: 100_000,
                period_secs: 86_400,
                cycles: 1,
                idempotency_key: "m-1".to_owned(),
            },
            &t.token,
        ))
        .await
        .unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "unsupported_wallet_address");
}
