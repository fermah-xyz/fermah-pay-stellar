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
- **Charges in contract order.** A batch takes each buyer's admitted charges
  in sequence order, starting at the buyer's next unconsumed sequence. A buyer
  with a charge in flight or in quarantine gets no new batch until it is
  resolved.
- **A lapsed authorization decides, not a missing transaction.** When a
  submission fails, expires or is quarantined, the authorizations in its
  broadcast envelope could still be included by someone else's transaction
  until their expiration ledger. So the worker waits for them to lapse and
  then reads the contract's own state, and uses the read only if the node
  served it at a ledger past that expiration: for a deposit, the marker the
  contract writes when it processes that deposit ID; for a charge, the
  account's last consumed sequence. A deposit whose marker exists is
  credited, otherwise it ends `failed` or `expired`; a charge whose sequence
  is still free is admitted again for the next batch; a charge whose sequence
  was consumed outside the gateway's batch is quarantined. A deposit whose
  transaction expired while the buyer's signature is still valid is sent
  again with the same signed entry, which its nonce lets land at most once.
- **Archived state is restored, not skipped.** A buyer account idle long
  enough for its entry to be archived makes simulation of any call touching
  it ask for a restore. The worker records and sends the restore transaction
  the simulation describes, through the same engine, and retries the refused
  deposit or batch after a pause.

| Contract outcome | Charge becomes | Available balance |
|---|---|---|
| `charged` | `charged` | stays debited |
| `insufficient_balance`, `above_limit` | `refused` | amount returned |
| `duplicate`, `out_of_order`, `unknown_account` | `quarantined` | stays debited |

The last row contradicts the gateway's own records: it sends an account's
sequence only after every earlier one settled, and charges only accounts a
confirmed deposit created. The charge is held for an operator rather than
guessed at. A charge is also quarantined when its batch's outcome cannot be
established.

## Credentials

Authorization entries can use legacy `Address` credentials or `AddressV2`,
which also commits the signed payload to the signer's address. Both are
signed and verified by the same code and are accepted by the Soroban host;
Stellar testnet accepted `AddressV2` entries for the recorded deposits.

## Observed costs on testnet

From the [evidence records](../evidence/README.md):

| Operation | Fee charged |
|---|---:|
| A deposit into a new account | about 0.069 XLM |
| A single charge | about 0.002 XLM |
| One batch charging 100 buyers | about 0.048 XLM |
| A withdrawal | about 0.035 XLM |
| The first write after deployment | about 12.8 XLM |

The first state-changing call after deployment extends the contract
instance and its code (about 15 KB) to about 30 days of life, and pays that
storage rent once; the extension is repeated only when fewer than about 7
days remain.
