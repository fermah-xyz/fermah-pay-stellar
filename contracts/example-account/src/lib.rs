//! A minimal contract account: it authorizes whatever its one Ed25519 key
//! signs. Real smart wallets add policies, several signers or passkeys; to
//! the ledger contract and the gateway they all look alike, an address that
//! authorizes through `__check_auth`. This one exists to exercise that path
//! in tests and on testnet, not to hold funds.

#![no_std]

use soroban_sdk::auth::{Context, CustomAccountInterface};
use soroban_sdk::crypto::Hash;
use soroban_sdk::{BytesN, Env, Vec, contract, contracterror, contractimpl, contracttype};

#[contract]
pub struct ExampleAccount;

#[contracttype]
#[derive(Clone)]
enum Key {
    Signer,
}

#[contracterror]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum AccountError {
    NoSigner = 1,
}

/// About 30 days of ledgers, the life the instance is kept at; extended
/// whenever fewer than about 7 remain, on each authorization.
const LIFE_LEDGERS: u32 = 518_400;
const EXTEND_BELOW_LEDGERS: u32 = 120_960;

#[contractimpl]
impl ExampleAccount {
    pub fn __constructor(env: Env, signer: BytesN<32>) {
        env.storage().instance().set(&Key::Signer, &signer);
        env.storage().instance().extend_ttl(EXTEND_BELOW_LEDGERS, LIFE_LEDGERS);
    }
}

#[contractimpl]
impl CustomAccountInterface for ExampleAccount {
    type Signature = BytesN<64>;
    type Error = AccountError;

    /// Accepts exactly a valid Ed25519 signature by the signer of the
    /// payload the host computed for this authorization; the host traps on
    /// any other.
    #[allow(non_snake_case)]
    fn __check_auth(
        env: Env,
        signature_payload: Hash<32>,
        signature: BytesN<64>,
        _auth_contexts: Vec<Context>,
    ) -> Result<(), AccountError> {
        let signer: BytesN<32> =
            env.storage().instance().get(&Key::Signer).ok_or(AccountError::NoSigner)?;
        env.crypto().ed25519_verify(&signer, &signature_payload.into(), &signature);
        env.storage().instance().extend_ttl(EXTEND_BELOW_LEDGERS, LIFE_LEDGERS);
        Ok(())
    }
}
