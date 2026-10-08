# Self-hosting: keys

## What each key can do

| Key | Held by | If it leaks |
|---|---|---|
| Operator | the settlement worker | charges to any buyer of the operator's deployments, within each buyer's balance and the per-charge limit |
| Fee account | the settlement worker | its XLM can be spent |
| Source (channel) accounts | the settlement worker | their sequence numbers can be consumed, which the worker detects and answers by leaving the source out ([limits](limits.md#source-accounts)); they hold no XLM and authorize nothing |
| Treasury | the settlement worker, only if it pays withdrawals (`PAY_STELLAR_TREASURY_KEY_FILE`) | everything the treasury holds can be moved: up to the sweep ceiling with a [cold reserve](treasury.md) |
| Cold reserve | its signers, never the worker | one key alone moves nothing; the reserve's threshold needs several |
| Admin | not the worker | the contract's code and roles can be replaced |

The worker never needs the admin's key: the admin signs upgrades, limit
changes and role rotations. Keep it off the worker's host.

The treasury signs each withdrawal together with the buyer. A worker given
the treasury's key pays withdrawals as soon as they are signed; without it,
withdrawals wait in `SIGNED` until they lapse. A worker holding the key can
move everything the treasury holds, so on such a host the treasury key is the
most valuable key. A [cold reserve](treasury.md) bounds what it can reach.

## Key references

The worker takes each of its keys as a key reference:

| Setting | Key |
|---|---|
| `PAY_STELLAR_OPERATOR_KEY_FILE` | operator |
| `PAY_STELLAR_FEE_SOURCE_KEY_FILE` | fee account |
| `PAY_STELLAR_SOURCE_KEY_FILE` | source accounts, comma-separated |
| `PAY_STELLAR_TREASURY_KEY_FILE` | treasury, if the worker pays withdrawals |

A key reference is the path of a seed file on the worker's host. The file holds the account's `S...` seed and nothing else, and must be readable by its owner only (mode `0600`); the worker refuses to start otherwise.

A reference of the form `<service>://<key>` names a key in a key management service. The key stays inside the service: the worker sends the 32-byte hash to sign and never holds the key. Two are served:

- `https://...`: a [remote signer](#a-remote-signer) the operator runs in front of any key store, such as a hardware security module, HashiCorp Vault or a cloud key management service;
- `aws-kms://...`: a key in [AWS KMS](#aws-kms), in a build with the `aws-kms` feature.

A reference to any other service is refused at startup.

### A remote signer

A key reference `https://<host>/<path>` names an endpoint that signs for one account. The worker never sees the key, so any key store that can produce an Ed25519 signature can hold it: the operator runs a small service between the worker and the store. The endpoint answers two requests at that URL, in JSON:

| Request | Body | Answer |
|---|---|---|
| `GET` | none | `{"account": "G..."}`: the account the key signs for |
| `POST` | `{"payload": "<64 hex digits>"}`: the 32-byte hash to sign | `{"signature": "<128 hex digits>"}`: its Ed25519 signature |

Any status other than 2xx is a failure. The worker sends the hash of a transaction or an authorization entry, never the transaction itself.

The worker trusts the endpoint for availability only. It checks every signature against the account before it records or sends anything, starting with a test signature at startup ([startup check](#startup-check)), so an endpoint holding the wrong key, or answering wrongly, signs nothing that is used.

How the worker reaches it:

| Setting | Effect |
|---|---|
| `PAY_STELLAR_REMOTE_SIGNER_TOKEN_FILE` | a file holding a token, sent as `Authorization: Bearer <token>` |
| `PAY_STELLAR_REMOTE_SIGNER_CLIENT_CERT_FILE`, `PAY_STELLAR_REMOTE_SIGNER_CLIENT_KEY_FILE` | a PEM client certificate and its key, for mutual TLS |
| `PAY_STELLAR_REMOTE_SIGNER_CA_FILE` | PEM certificates the endpoint's certificate must chain to, instead of the public web roots, for an endpoint under a private authority |

- **Authentication.** An endpoint on another host needs a token, a client certificate or both; the worker refuses to start without one. Files holding a secret (the token, the client key) must be readable by their owner only.
- **Transport.** It is reached over HTTPS. Plain `http://` is accepted only for an endpoint on the same host (`localhost` or a loopback address), such as a sidecar. Redirects are not followed: an answer redirecting elsewhere is a failure, so the request and the client certificate only ever go to the configured URL.
- **No credentials in the URL.** A reference such as `https://user:pass@...` is refused at startup; use the token file or a client certificate, which never appear in logs or errors.
- **Answers.** An answer larger than 4 KiB is a failure; the endpoint's two answers are a few dozen bytes.
- **Timeouts.** A request taking more than 10 seconds fails. A failed signature leaves nothing recorded or sent, and is counted in `pay_stellar_signing_failures_total`, which `PayStellarSigningFailing` watches.
- **Other tools.** `fermah-pay-stellar-contract sign --key` and `address --key` accept the same references.

### AWS KMS

A key reference `aws-kms://<key>` names the key by key id, key ARN, alias name (`alias/...`) or alias ARN. The key must be asymmetric, of spec `ECC_NIST_EDWARDS25519`, for `SIGN_VERIFY`. KMS then signs with pure Ed25519 (`ED25519_SHA_512` over the raw message), which is what a Stellar signature is. At startup the worker reads the key's public key, refuses any other kind of key, and derives the account from it.

```bash
aws kms create-key --key-spec ECC_NIST_EDWARDS25519 --key-usage SIGN_VERIFY \
  --description "pay-stellar operator"
aws kms create-alias --alias-name alias/pay-stellar-operator --target-key-id <key id>
# The tools, built with the aws-kms feature:
cargo build --release -p fermah-pay-stellar-cli --features aws-kms
# The account this key signs for, to fund or to name in the contract:
fermah-pay-stellar-contract address --key aws-kms://alias/pay-stellar-operator
# On testnet, a transaction signed by the key (Friendbot funds the account):
fermah-pay-stellar-testnet key-check --key aws-kms://alias/pay-stellar-operator
```

- **Credentials and region.** They come from the standard AWS sources: environment variables, the shared profile, or the instance or task role. `AWS_REGION` must be the key's region.
- **Permissions.** The worker needs only `kms:GetPublicKey` and `kms:Sign` on its keys.
- **Losing a key.** A KMS key cannot be exported, so its loss or deletion is the account's loss. Keep the treasury and admin behind several signers ([cold reserve](treasury.md), [admin multisig](#an-admin-that-needs-several-signatures)), and rotate a role to a new key before scheduling a key's deletion.
- **Other tools.** `fermah-pay-stellar-contract sign --key` accepts the same references, so a signer of a multisig proposal can keep their key in KMS too.
- **Build feature.** Support is the `aws-kms` feature, off by default so a build that does not use it carries no AWS dependency. Build with it for `aws-kms://` references: `cargo build --release -p fermah-pay-stellar-gateway --features aws-kms` for the worker, and `-p fermah-pay-stellar-cli --features aws-kms` for the tools. A build without it refuses `aws-kms://` references at startup.

## Startup check

Before building any transaction, the worker has each key sign a random payload and checks the signature against the account the key claims. At every later signature, the worker checks the signature again before it records or sends anything.

A key that signs for another account, or a service that answers wrongly, is therefore refused before it can produce a transaction the network would reject. A signing failure leaves nothing recorded and nothing sent, and the next attempt starts afresh.

## An admin that needs several signatures

The admin authorizes pausing, limit changes, code upgrades and role rotations, and on a vault its launch limits. No single key should be able to do any of these. A Stellar account can list extra signers with weights, and a contract's authorization of an account needs the account's medium threshold. So an admin account whose medium threshold is 2, with three keys of weight 1, needs any two of them.

`fermah-pay-stellar-contract` makes an admin change in three steps. Each step may run on a different machine, and no signer needs another signer's key.

```bash
contract() { cargo run -q -p fermah-pay-stellar-cli --bin fermah-pay-stellar-contract -- "$@"; }
NET="--network stellar:testnet --rpc-url https://soroban-testnet.stellar.org"

# 1. Simulate the change and write the proposal (here: pause).
contract propose $NET --contract C... --admin G... \
  --source-key submitter.secret --fee-key fee-source.secret --out pause.json pause

# 2. Each signer, on their own machine: shows the call and adds a signature.
contract sign --proposal pause.json --key /path/to/signer.secret

# 3. Check the signatures against the account's signers and threshold on
#    the ledger, then send the change, fee-bumped.
contract submit $NET --proposal pause.json \
  --source-key submitter.secret --fee-key fee-source.secret
```

The available changes are:
- `pause` and `unpause`;
- `set-limits --min-deposit --max-charge`;
- `set-daily-limits --per-buyer --per-seller` ([daily limits](../architecture/prepaid-contract.md#daily-limits));
- `upgrade --wasm-hash` on a prepaid ledger, after `contract upload --wasm <file>` has put the code on the network and printed its hash;
- `set-role --role --holder`. The new holder must sign too: name it with `sign --account`.

On a [vault](../architecture/vault-contract.md#upgrades), code changes take two proposals a timelock apart, and buyers can exit in between:
- `propose-upgrade --wasm-hash`, after the upload; the observer reports it as a critical `upgrade_proposed` finding;
- `cancel-upgrade`, to drop it;
- `install-upgrade`, once the vault's delay of 120,960 ledgers (about a week) has passed and within the 51,840 ledgers (about three days) after it; it installs exactly the proposed hash. Past that window the proposal lapses and has to be made again. Collect the signatures in time;
- `set-launch-limits --max-balance --max-total`, or `--remove`, bounds what one buyer and all buyers together may hold.

`sign` recomputes what it signs from the proposal's call rather than trusting the proposal's summary. `submit` refuses a proposal whose signatures do not reach the medium threshold, naming the weight that is missing.

A proposal is valid for the ledgers given to `propose --valid-for-ledgers` (a day by default). After that its authorizations expire and a new proposal is needed.

To give an account signers and thresholds, submit a `SetOptions` transaction signed by keys meeting its current high threshold. Order the operations so that the signers are added before the thresholds are raised. On testnet, `fermah-pay-stellar-testnet admin-multisig` does this for the profile's admin, with a sponsor paying the signers' reserves.
