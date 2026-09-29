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
| `expired` | the validity window has passed (plus an ingestion margin), the hash is not found, and the source's sequence never reached the envelope's sequence: it can never be included |
| `quarantined` | the window has passed and the hash is not found, but the source's sequence was consumed: the evidence contradicts itself and an operator decides |

- **Final means final.** A database trigger refuses any change to a submission
  that is no longer in flight, so a later RPC answer cannot rewrite an
  outcome.
- **Recovery.** After a restart the engine resends the in-flight envelope's
  bytes unchanged and resolves it by hash. After `expired`, the next envelope
  reuses the sequence that was never consumed.

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
