# Self-hosting: keys

## What each key can do

| Key | Held by | If it leaks |
|---|---|---|
| Operator | the settlement worker | charges to any buyer of the operator's deployments, within each buyer's balance and the per-charge limit |
| Fee account | the settlement worker | its XLM can be spent |
| Source (channel) accounts | the settlement worker | their sequence numbers can be consumed; they hold no XLM and authorize nothing |
| Treasury | the settlement worker, only if it pays withdrawals (`PAY_STELLAR_TREASURY_KEY_FILE`) | everything the treasury holds can be moved |
| Admin | not the worker | the contract's code and roles can be replaced |

The worker never needs the admin's key: the admin signs upgrades, limit
changes and role rotations. Keep it off the worker's host.

The treasury signs each withdrawal together with the buyer. A worker given
the treasury's key pays withdrawals as soon as they are signed; without it,
withdrawals wait in `SIGNED` until they lapse. A worker holding the key can
move everything the treasury holds, so on such a host the treasury key is the
most valuable key.

## Key references

The worker takes each of its keys as a key reference:

| Setting | Key |
|---|---|
| `PAY_STELLAR_OPERATOR_KEY_FILE` | operator |
| `PAY_STELLAR_FEE_SOURCE_KEY_FILE` | fee account |
| `PAY_STELLAR_SOURCE_KEY_FILE` | source accounts, comma-separated |

A key reference is the path of a seed file on the worker's host. The file holds the account's `S...` seed and nothing else, and must be readable by its owner only (mode `0600`); the worker refuses to start otherwise.

A reference of the form `<service>://<key>` names a key in a key management service, and is refused at startup until that service's backend is added. Each backend keeps the key inside the service: the worker sends the 32-byte hash to sign and never holds the key.

## Startup check

Before building any transaction, the worker has each key sign a random payload and checks the signature against the account the key claims. At every later signature, the worker checks the signature again before it records or sends anything.

A key that signs for another account, or a service that answers wrongly, is therefore refused before it can produce a transaction the network would reject. A signing failure leaves nothing recorded and nothing sent, and the next attempt starts afresh.

## An admin that needs several signatures

The admin authorizes pausing, limit changes, code upgrades and role rotations. No single key should be able to do any of these. A Stellar account can list extra signers with weights, and a contract's authorization of an account needs the account's medium threshold. So an admin account whose medium threshold is 2, with three keys of weight 1, needs any two of them.

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
- `upgrade --wasm-hash`;
- `set-role --role --holder`. The new holder must sign too: name it with `sign --account`.

`sign` recomputes what it signs from the proposal's call rather than trusting the proposal's summary. `submit` refuses a proposal whose signatures do not reach the medium threshold, naming the weight that is missing.

A proposal is valid for the ledgers given to `propose --valid-for-ledgers` (a day by default). After that its authorizations expire and a new proposal is needed.

To give an account signers and thresholds, submit a `SetOptions` transaction signed by keys meeting its current high threshold. Order the operations so that the signers are added before the thresholds are raised. On testnet, `fermah-pay-stellar-testnet admin-multisig` does this for the profile's admin, with a sponsor paying the signers' reserves.
