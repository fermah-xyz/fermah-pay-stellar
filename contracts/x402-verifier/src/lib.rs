//! What a contract-account buyer authorizes to pay an x402 commitment.
//!
//! A classic account proves a commitment with an Ed25519 signature the
//! gateway checks itself. A contract account, such as a passkey smart wallet,
//! authorizes only through its own `__check_auth`, which only the network
//! runs. So the buyer signs an authorization entry for [`commit`] over the
//! commitment's text, and the gateway simulates the call with that entry: the
//! network runs the account's `__check_auth`, and the simulation succeeds only
//! if the account accepts it.
//!
//! The contract holds nothing and changes nothing, so a signed entry for it
//! cannot move funds. Whoever holds the entry can still submit it on-chain,
//! which consumes the account's nonce and so cancels the commitment, and
//! publishes its terms. It has no admin and no upgrade, so what an entry for
//! it can authorize never changes.
//!
//! [`commit`]: X402Verifier::commit

#![no_std]

use soroban_sdk::{Address, Env, String, contract, contractimpl};

#[contract]
pub struct X402Verifier;

#[contractimpl]
impl X402Verifier {
    /// Succeeds only with `payer`'s authorization of exactly this call,
    /// `commitment` included, which binds the authorization to every term of
    /// the commitment.
    pub fn commit(_env: Env, payer: Address, _commitment: String) {
        payer.require_auth();
    }
}

#[cfg(test)]
mod test {
    use soroban_sdk::testutils::{
        Address as _, AuthorizedFunction, AuthorizedInvocation, EnvTestConfig, MockAuth,
        MockAuthInvoke,
    };
    use soroban_sdk::xdr::{ScErrorCode, ScErrorType};
    use soroban_sdk::{Address, Env, IntoVal, String, Symbol};

    use super::{X402Verifier, X402VerifierClient};

    /// The host's refusal of an authorization the call required, as a
    /// caller sees it: every non-contract error is narrowed to
    /// `(Context, InvalidAction)` at the call boundary. `commit` does
    /// nothing else that could fail, and each refusal below has a positive
    /// control that differs only in the authorization.
    fn auth_failure() -> soroban_sdk::Error {
        soroban_sdk::Error::from_type_and_code(ScErrorType::Context, ScErrorCode::InvalidAction)
    }

    fn setup() -> (Env, X402VerifierClient<'static>, Address) {
        // No per-test JSON ledger snapshot, which is not wanted in the
        // repository.
        let env = Env::new_with_config(EnvTestConfig { capture_snapshot_at_drop: false });
        let id = env.register(X402Verifier, ());
        let client = X402VerifierClient::new(&env, &id);
        let payer = Address::generate(&env);
        (env, client, payer)
    }

    #[test]
    fn test_commit_requires_the_payers_authorization_of_the_exact_text() {
        let (env, client, payer) = setup();
        let text = String::from_str(&env, "x402 batch-settlement commitment\namount: 30");
        env.mock_all_auths();
        client.commit(&payer, &text);
        assert_eq!(
            env.auths(),
            [(
                payer.clone(),
                AuthorizedInvocation {
                    function: AuthorizedFunction::Contract((
                        client.address.clone(),
                        Symbol::new(&env, "commit"),
                        (payer.clone(), text).into_val(&env),
                    )),
                    sub_invocations: [].into(),
                },
            )]
        );
    }

    #[test]
    fn test_commit_without_the_payers_authorization_is_refused() {
        let (env, client, payer) = setup();
        let text = String::from_str(&env, "x402 batch-settlement commitment");
        assert_eq!(client.try_commit(&payer, &text), Err(Ok(auth_failure())));
        // The positive control: the same call with the payer's authorization.
        env.mock_all_auths();
        client.commit(&payer, &text);
    }

    #[test]
    fn test_an_authorization_of_one_text_does_not_cover_another() {
        let (env, client, payer) = setup();
        let signed = String::from_str(&env, "amount: 30");
        let presented = String::from_str(&env, "amount: 31");
        env.mock_auths(&[MockAuth {
            address: &payer,
            invoke: &MockAuthInvoke {
                contract: &client.address,
                fn_name: "commit",
                args: (payer.clone(), signed.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert_eq!(client.try_commit(&payer, &presented), Err(Ok(auth_failure())));
        // The positive control, with the text the payer authorized.
        env.mock_auths(&[MockAuth {
            address: &payer,
            invoke: &MockAuthInvoke {
                contract: &client.address,
                fn_name: "commit",
                args: (payer.clone(), signed.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.commit(&payer, &signed);
    }
}
