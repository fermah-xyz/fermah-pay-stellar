# Sponsored transactions

Buyers, the treasury, the operator and the seller hold no XLM. They
authorize contract calls; other accounts submit and pay for them.

## Three separate acts

| Act | Who | Mechanism |
|---|---|---|
| Authorize the call | buyer, treasury, operator, seller (as the contract requires) | a Soroban address authorization entry, signed with the account's key |
| Sign and sequence the transaction | a **submitter** account | the inner transaction's source and signature |
| Pay the fee | a separate **fee source** account | a fee-bump envelope wrapping the signed inner transaction |

The fee-bump signature grants no contract authority, and an authorization
entry does not cover the transaction source, sequence number or fee. A buyer's
signature therefore commits to exactly one call, and nothing about who
submits it or what it costs.

Account and trustline reserves are a fourth, separate cost, paid by a
**sponsor** through sponsored reserves when an account is created (see the
[architecture overview](overview.md#buyer-onboarding-without-xlm)).

## Submitting a call

Implemented in
[`crates/stellar-chain/src/sponsored.rs`](../../crates/stellar-chain/src/sponsored.rs):

1. **Discover.** The call is simulated in recording mode. The authorizations
   the network requires must be exactly the ones built from the pinned
   deployment and the intent (signer and full invocation tree); anything
   else is refused before anyone is asked to sign.
2. **Sign.** Each signer receives an entry with a fresh random nonce,
   generated locally rather than taken from the RPC, and an expiration
   ledger a bounded distance ahead.
3. **Check.** Every returned entry is verified before any fee can be spent:
   expected account, byte-identical invocation tree, live and bounded
   expiration, and a valid signature by the account's own key. Each defect
   has its own refusal reason.
4. **Enforce.** The call is simulated again with the signed entries in
   enforcing mode, which verifies them exactly as the network will and
   yields the resources to declare.
5. **Assemble and wrap.** The declared resource fee is the simulated minimum
   plus a margin (unused fee is refunded). The submitter signs the inner
   transaction, which carries a finite upper time bound; the fee source signs
   a fee bump around it.
6. **Submit and resolve.** Only the signed bytes are ever resent, and the
   outcome is resolved by the fee-bump hash, which is the hash the network
   reports. A lost response, a node reporting the transaction as unknown, or
   an RPC failure never produces a second, different transaction; past the
   time bound, an envelope that was never seen can no longer be included.

Every receipt records both the inner and the outer transaction hash, the
submitter and the fee source.

## Durable submission in the gateway

The gateway's submission engine runs the same steps, with every envelope
recorded before it is sent
([`crates/gateway/src/submission`](../../crates/gateway/src/submission/mod.rs),
schema in [`0002_submissions.sql`](../../db/migrations/0002_submissions.sql)):

- **Install before send.** The complete signed fee-bump envelope, its inner and
  outer hashes, the source sequence and the validity window are written in one
  row before any broadcast. The worker's database role cannot change those
  columns afterwards. A crash at any point leaves either nothing (nothing was
  sent) or the exact bytes to resend.
- **One envelope in flight per source.** A unique index allows at most one
  submission per source account whose outcome is unknown, because the network
  accepts only the next sequence number.
- **Outcomes come only from the hash.** A `sendTransaction` answer, including
  a refusal, never settles a submission: resending an envelope that already
  landed is refused as a stale sequence. A submission is final only when:

| State | Evidence |
|---|---|
| `succeeded` / `failed` | the network returns the transaction for the envelope's hash |
| `expired` | the RPC node has ingested a ledger that closed after the envelope's upper time bound, still does not find its hash, retains history back to when the envelope was recorded, and reads the source's sequence below the envelope's at that ledger or later: it was never included and never can be |
| `quarantined` | the node is past the window and does not find the hash, but the source's sequence was consumed, or the node's history does not reach back far enough to tell |

The local clock is never evidence of expiry: a node that lags behind the
network would report an included envelope as not found and the source's
sequence as unconsumed, both consistently stale. Every "not found" is
therefore judged at the ledger the node has actually ingested.

- **Final means final.** A database trigger refuses any change to a submission
  that is no longer in flight, so a later RPC answer cannot rewrite an
  outcome.
- **Recovery.** After a restart the engine resends the in-flight envelope's
  bytes unchanged and resolves it by hash. After `expired`, the next envelope
  reuses the sequence that was never consumed.

### Inclusion fees

An envelope's inclusion bid is fixed when it is built, like the rest of its
bytes. Each new envelope bids, per operation:

```
min(cap, max(floor, market, 2 × the previous bid if the previous envelope expired))
```

- **Market.** A percentile (the 90th by default) of the Soroban inclusion
  fees the RPC node reports through `getFeeStats` for its recent window of
  ledgers (50 by default in Stellar RPC). The node records each
  transaction's whole inclusion fee, the fee charged less the resource fee
  charged ([`feewindow.go`](https://github.com/stellar/stellar-rpc/blob/397ea6eb1b1471ef15a8d4d98c329ce10425e33b/cmd/stellar-rpc/internal/feewindow/feewindow.go#L164-L233));
  for a fee bump that covers two operations, so reading it as a
  per-operation bid errs high. If the statistics cannot be read, the bid
  rests on the floor and the escalation, and a warning is logged.
- **Escalation.** If the source's latest submission `expired`, proven never
  included, the next envelope from that source bids twice what it bid,
  whatever work it carries. Consecutive expiries double the bid up to the
  cap; once an envelope is included, bids follow the market again. `failed`
  and `quarantined` submissions do not escalate: the first was included, and
  the second's fate is unknown. The previous bid is read back from the
  recorded envelope, so a restart does not reset the escalation.
- **What is paid.** The network charges each operation the ledger's base
  fee or, when the transaction's lane was full, the lowest per-operation bid
  it let into the ledger, and never more than the bid
  ([`TxSetFrame.cpp`](https://github.com/stellar/stellar-core/blob/947aad8413c189d85504acf72207e85eeda9b021/src/herder/TxSetFrame.cpp#L436-L477),
  [`FeeBumpTransactionFrame.cpp`](https://github.com/stellar/stellar-core/blob/947aad8413c189d85504acf72207e85eeda9b021/src/transactions/FeeBumpTransactionFrame.cpp#L592-L616)).
  A bid above the market therefore costs nothing extra outside congestion.
  The cap bounds the fee source's spend during it: at most `2 × cap` stroops
  of inclusion fee per envelope (the inner operation and the fee bump), plus
  the declared resource fee, whose unused part is refunded.

Every bid is logged at `info` with the market reading, the percentile, the
bid it escalated from, the floor and the cap; a bid held at the cap is
logged at `warn`. The settings are in the
[configuration table](../self-hosting/database.md#configuration); setting
the cap equal to the floor gives a fixed bid.

The engine never replaces an envelope in flight with a higher fee bump of
the same inner transaction. Stellar Core accepts such a replacement only at
ten times the queued inclusion fee per operation or more
([`FEE_MULTIPLIER`](https://github.com/stellar/stellar-core/blob/947aad8413c189d85504acf72207e85eeda9b021/src/herder/TransactionQueue.cpp#L51),
[`canReplaceByFee`](https://github.com/stellar/stellar-core/blob/947aad8413c189d85504acf72207e85eeda9b021/src/herder/TransactionQueue.cpp#L242-L276), applied
[here](https://github.com/stellar/stellar-core/blob/947aad8413c189d85504acf72207e85eeda9b021/src/herder/TransactionQueue.cpp#L403-L433)), a step that from the
default floor jumps to 100 000 stroops per operation at once. A replacement
is also a second envelope for the same sequence: each would need its own
record before it is sent, and either could land, so resolution would have to
follow both hashes. Rebuilding with a doubled bid keeps one recorded
envelope per submission, resolved by its one hash, and leaves the expiry
proof as it is. The price is delay: a charge batch whose envelope expired
waits for its validity window (60 seconds by default) and then for the
operator's authorization to lapse (24 ledgers, about two minutes) before
its charges are sent again, well within the charges' own window (720 ledgers,
about an hour by default).

### Local clock

The upper time bound of an envelope is the local clock plus the validity
window. Before building one, the engine reads the latest ledger's close time
(`getLatestLedger`) and builds nothing while the two are further apart than
`PAY_STELLAR_MAX_CLOCK_SKEW_SECS` (20 seconds by default, and required to be
below the validity window): a clock that far behind would build envelopes
the network already considers expired, and one that far ahead would keep
work waiting longer than intended. The worker logs the refusal and tries
again on its next step; what is already in flight is still sent and
resolved. A ledger closes about every five seconds, so the latest close time
trails the true time by a few seconds; a node lagging behind the network
looks like a clock running ahead, and also stops new envelopes until it
catches up.

The check only gates building. Whether an envelope expired is still decided
from the ledger close times the node reports, never from the local clock.
Keep the host clock synchronized (for example with NTP).

## Settlement

The worker ([`crates/gateway/src/worker.rs`](../../crates/gateway/src/worker.rs))
turns signed deposits and admitted charges into submissions and applies the
outcomes:

- **Recorded with what it settles.** A submission is recorded in the same
  database transaction that marks its deposit or charges as submitted and
  records each charge's position in the batch. A crash leaves either both or
  neither, so there is never an envelope in flight without a record of what
  it settles.
- **Outcomes applied once.** A deposit is credited, and a refused charge's
  amount returned, in the same statement that moves the row into its final
  state; final states cannot be left, so each balance change happens once.
- **Charges within their last ledger.** A batch takes the oldest admitted
  charges, in any order and from any buyers, that can still land before their
  last ledger. An admitted charge past its last ledger was never applied and
  never can be; it is refused as `expired` and its amount returned.
- **A lapsed authorization decides, not a missing transaction.** When a
  submission fails, expires or is quarantined, the authorizations in its
  broadcast envelope could still be included by someone else's transaction
  until their expiration ledger. So the worker waits for them to lapse and
  then reads the contract's own state, and uses the read only if the node
  served it at a ledger past that expiration: for a deposit, the marker the
  contract writes when it processes that deposit ID; for a charge, the
  record the contract keeps of it, which holds its outcome. A deposit whose
  marker exists is credited, otherwise it ends `failed` or `expired`. A charge
  whose record exists takes the recorded outcome, however it was applied; one
  with no record is admitted again while within its last ledger, and refused
  as `expired` after it, while a record would still live.
- **Past the record's lifetime, the events decide.** A charge decided only
  after its record would have lapsed, for example after an outage, is decided
  from the contract's `charges` events. Each charge batch's submission
  records the latest ledger the worker saw before the operator signed it;
  no transaction can include the batch's authorization earlier, and after the
  charge's last ledger the contract refuses it whoever sends it. The worker
  reads every event from that ledger through the charge's last ledger: an
  entry for the charge is its settlement, and no entry proves it was never
  applied, so it is refused as `expired`. The search is used only when the
  node served the whole range, which it does only while it still retains the
  first ledger; otherwise the charge is quarantined with the missing range.
  The worker reads the node directly rather than the
  [chain observer](observer.md)'s stored events, so the decision does not
  depend on another process having run. A deposit whose
  transaction expired while the buyer's signature is still valid is sent
  again with the same signed entry, which its nonce lets land at most once.
- **Archived state is restored, not skipped.** Since protocol 23 the network
  restores an archived entry inside the invocation that touches it:
  simulation lists the entry in the transaction's resource extension, which
  the worker sends unchanged, and the deposit or batch pays the restoration.
  Should a node still ask for a separate restore, the worker records and
  sends the restore transaction the simulation describes, through the same
  engine, and retries the deposit or batch after a pause.

| Contract outcome | Charge becomes | Available balance |
|---|---|---|
| `charged` | `charged` | stays debited |
| `insufficient_balance`, `above_limit`, `expired` | `refused` | amount returned |
| `duplicate` | the outcome its record holds | as that outcome |
| `unknown_account` | `quarantined` | stays debited |

`unknown_account` contradicts the gateway's own records, which charge only
accounts a confirmed deposit created. A charge is also quarantined when the
network reports its batch applied yet the contract holds no record of it, or
when its record has lapsed and the events of the whole range cannot be read.
Each is held for an operator rather than guessed at; see
[resolving quarantined charges](../self-hosting/quarantine.md).

## Credentials

Authorization entries can use legacy `Address` credentials or `AddressV2`,
which also commits the signed payload to the signer's address. Both are
signed and verified by the same code and are accepted by the Soroban host;
Stellar testnet accepted `AddressV2` entries for the recorded deposits.

The gateway prepares deposit authorizations, and the worker signs its batch
authorizations, with `AddressV2` credentials. They exist from network
protocol 27 ([CAP-71 XDR](https://github.com/stellar/stellar-xdr/blob/68fa1ac55692f68ad2a2ca549d0a283273554439/Stellar-transaction.x#L585-L603), absent
before), and settlement also relies on archived entries being restored
inside the invocation that touches them, which protocol 23 introduced. So
the gateway and the worker refuse to start unless their RPC endpoint reports
`protocolVersion` 27 or later from `getNetwork`, besides the configured
network's passphrase.

A buyer's wallet must be able to sign an `AddressV2` entry: an SDK that knows
only legacy `Address` credentials cannot decode the prepared entry, or signs
the legacy payload, which the gateway refuses as `invalid_signature`.
Signing the `signature_payload` the gateway returns works with any Ed25519
signer.

## Observed costs on testnet

From the [evidence records](../evidence/README.md):

| Operation | Fee charged |
|---|---:|
| A deposit into a new account | about 0.065 XLM |
| A single charge | about 0.0027 XLM |
| One batch charging 98 buyers | about 0.115 XLM (0.0012 XLM per charge) |
| A withdrawal | about 0.036 XLM |

Most of a charge's fee is rent for the two entries it writes: the buyer's
account, and the charge's record, which lives until about an hour after the
charge's last ledger. A state-changing call also extends the contract
instance, and with it the code, to about 30 days of life once fewer than
about 7 days remain; the call that does so pays that rent, which depends on
how much life the code had left.
