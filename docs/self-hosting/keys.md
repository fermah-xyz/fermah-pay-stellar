# Self-hosting: keys

## What each key can do

| Key | Held by | If it leaks |
|---|---|---|
| Operator | the settlement worker | charges to any buyer of the operator's deployments, within each buyer's balance and the per-charge limit |
| Fee account | the settlement worker | its XLM can be spent |
| Source (channel) accounts | the settlement worker | their sequence numbers can be consumed; they hold no XLM and authorize nothing |
| Treasury | not the worker | the deposited USDC can be moved |
| Admin | not the worker | the contract's code and roles can be replaced |

The worker never needs the treasury's or the admin's key.
- The treasury signs withdrawals together with the buyer.
- The admin signs upgrades, limit changes and role rotations.

Keep both off the worker's host.

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
