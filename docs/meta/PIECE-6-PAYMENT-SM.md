# Piece-6 design — payment state machine + write-ahead journal

Rebuild-track **piece-6** (see [REBUILD-SEAM.md](REBUILD-SEAM.md) §2). This is a **design
piece, not an extraction**: the spike proved the money path works, but its shape is
refuse-listed. This doc is the spec the extraction round builds against. Class: **MONEY**
(dual-review + codex + operator gate).

Grounded in the spike at `0e77669`. The reference implementation to learn from — and not
copy — is `BuyerMcpServer::authorize_pay` (`crates/mobee/src/cli.rs:1844-2203`).

## Why this is a redesign, not a lift

`authorize_pay` is a ~360-line god-function that, in one body, does: arg parse ·
idempotency short-circuit · seller/offer/result fetch + validate · git-delivery verify ·
integrity-hash compute + compare · receipt sign · journal write · pay · publish receipt ·
remember. SPIKE_LESSONS refuse-lists it by name. Two concrete scars in that body:

1. **Append-after-pay (the double-pay window).** `remember_buyer_payment` is called *after*
   `pay_seller` returns (`cli.rs:~2150` vs the pay at `~2140`). A crash between the pay
   succeeding and the journal write leaves money moved with no durable record; the on-relay
   receipt isn't published yet either, so recovery (`find_buyer_payment_record` /
   `fetch_existing_receipt`) finds nothing and **pays again**. The window is small and real.
2. **State smeared across a `serde_json` map + an in-memory `self.jobs` entry + an
   idempotency log**, with no single type that names which state a job is in — so "paid but
   no receipt", "receipt but no local mark", and "neither" are reconstructed ad hoc at three
   different return points instead of being one enum the code switches on.

What the spike got *right* and piece-6 must preserve (good bones): the layered recovery
short-circuits (`locally_paid && receipt` → return; journal record needs-receipt →
republish-only; on-relay receipt fallback; locally-paid-but-no-record → **refuse**, never
re-pay). c2 proved this recovery works under a live stall — the redesign keeps the
behavior, gives it a spine.

## State machine

One explicit type. States and the ONLY legal transitions (operator-locked vocabulary —
"delivery" means the *work product* (git-delivery); the money leg is **payment send**, never
"delivered"):

```
Intent  ──mint/lock──►  Locked  ──send──►  Sent  ──publish──►  ReceiptPublished  ──►  Closed
   │                                                                                          
   └── every transition is journaled write-ahead (flock + fsync) BEFORE its side effect ──┘
```

- **Intent** — durably recorded (write-ahead) *before* any token is minted or spent. Holds
  the full idempotency key (below). This is the fix for scar #1: the record exists before
  the pay, so a crash-after-pay is recoverable, not a re-pay.
- **Locked** — token minted/locked to the seller (verify-not-spend; the gateway holds only
  the public half).
- **Sent** — payment token sent to the seller over the private NIP-17 DM path (piece-4,
  `payment_send`); **total send failure fails closed** (piece-4's empty-relay-success ⇒ Err
  — a token that reached zero relays is not sent).
- **ReceiptPublished** — buyer-authored co-signed receipt on the relay.
- **Closed** — receipt observed/confirmed.

`pay_seller` fires **exactly once** across retry / crash / concurrent invocation. The
merge gate proves it with a stubbed pay-counter (SPIKE_LESSONS).

**Payment payload is typed, not stringly — arrives via the #6 rework, NOT introduced here
(operator override, 2026-07-14):** the typed `cashu::Token` payload (`PaymentPayload` holding
`Token` + `MintUrl`/`Amount`, correlation fields as mobee newtypes, `cashuA`/`cashuB` string
only at the NIP-17 boundary, seller parse-first fail-closed, feature-gated cashu-free
default) lands in **piece-4's #6 rework before merge** — that is where finding-10 closes. The
payment SM here simply **consumes** the already-typed payload: the `Locked→Sent` transition
and the receive-side gate are enforced by the type `#6` provides, not by a hand-rolled string
check in the SM. One `Token` type flows mint → verify → send → receive; the SM never
re-parses a string.

## Write-ahead journal

- **Idempotency key** (SPIKE_LESSONS, verbatim): `(job_id, result_id, content_hash,
  job_hash, seller_pubkey, amount, mint)`.
- **Discipline:** durable pre-pay **Intent** written with `flock` + `fsync` **before**
  `pay_seller`; each subsequent transition marked under the same lock. Never
  append-after-pay.
- **Recovery is explicit, not incidental:** on any re-entry, load the journal, resolve the
  job's state, and act by state — `Intent` with no confirmed pay → reconcile against the
  mint/relay before deciding; `Locked`/`Sent` with no receipt → resume forward
  (republish, never re-pay); `ReceiptPublished` → idempotent return. The "paid but no
  record" case is a hard refuse, exactly as the spike does.
- **Injectable journal trait** (SPIKE_LESSONS SHOULD): FS-JSONL for the demo, an interface
  so tests drive it without the filesystem.

## Decomposition — what moves where

`cli.rs` keeps only: parse args, wire the runtime, print JSON. Everything below is
`mobee-core`, testable without network/wallet/acp:

- **`payment` (new):** the state machine above + the journal trait + recovery. Owns
  transition legality and the once-only pay guarantee.
- **`gateway`** (piece-2, landed): offer/result parse + validation, targeting.
- **`wallet`** (piece-3): token verification mechanism — `verify_p2pk_token`
  (lock/amount/mint/spend-state of *presented* proofs). **Its trust boundary is explicit
  (piece-3 rustdoc): `Ok ≠ redeemable`.**
- **`payment_send`** (piece-4, renamed from "token delivery"): payment token send over
  NIP-17, fail-closed on total relay failure, metadata-only returns (`PaymentSent`), typed
  `cashu::Token` in-process.
- **`receipt`** (piece-1): H-tuple, dual-Schnorr.
- **`buyer` / `seller` (role-specific SM modules):** the payment SM composes with role state
  — the buyer drives offer→accept→pay→receipt; the seller drives claim→work→result→receive.
  Shared protocol/money mechanisms (gateway/wallet/payment_send/receipt) sit under both.

**Crate boundary (operator, 2026-07-14):** near-term these are all **modules** in
`mobee-core` + thin CLI/MCP skins. Promote `buyer`/`seller` to `mobee-buyer` / `mobee-seller`
**crates** only when nix wants distinct installables — the module boundary drawn now becomes
the crate cut then. **Do not crate-split mid #6/#8**; keep the cut clean at the module level
so the later split is mechanical.

## Inherited gates (baked in from tonight's reviews — each a named MUST)

- **Authenticity gate (codex #8 HIGH-2, the one that closes the real hole):** before the SM
  advances past **Sent/receive**, the seller path **swaps the received proofs at the
  mint** (or fully crypto-verifies: retained `C` + keyset + DLEQ). `verify_p2pk_token`
  checks presented proofs only; NUT-07 "unspent" is advisory, not authenticity. Regression
  target: **Temper's inflated-amount token** built on a real unspent `y` — sum/mint/lock/
  NUT-07 all green while redeemable value ≠ presented value — MUST fail closed here.
- **Spec §4 shares this gap** (addendum finding 8): the locked four-check gateway verify has
  no authenticity check either. Flagged to the spec owner as HIGH-for-spec; piece-6's swap
  gate is the code-side answer regardless of how §4 resolves.
- **Testnut allowlist (policy layer):** `expected_mint` outside the test-mint set → hard
  fail *before* any verify call. This is the fund-isolation gate (distinct from wallet.rs's
  mint-equality mechanism); it lives here, where `expected_mint` is chosen.
- **Checked arithmetic:** proof-amount aggregation uses `checked_add` (no wrap); duplicate
  `y`/secret rejected before summing (both landed in piece-3, must not regress).
- **NUT-07 wire contract:** who fetches proof state and when is explicit and documented,
  with a freshness bound — not an artifact of signatures.
- **Mint-URL comparison:** exact-match vs normalized (trailing slash / case / port) is a
  named decision, tested both ways.
- **Receipt authority = author + signatures, not tags; empty `relay_success` = failure**
  for every money-path publish (SPIKE_LESSONS).
- **No-wallet build has no pay path** (feature gate = safety).

## Acceptance (merge gate — the extraction PR proves all of these)

- Stubbed pay-counter: `pay_seller` ≤ 1 across retry / crash / concurrent.
- Idempotency suite: double request · pay-ok/receipt-fail · journal restart · malformed →
  fail-closed.
- Hash-bind failures reject **before** pay.
- Authenticity: inflated-amount-on-real-`y` fails closed at the swap gate.
- Payment send: zero-relay-success ⇒ the SM does not advance to Sent.
- Payment payload holds typed `cashu::Token` in-process; a non-parseable token cannot be
  constructed into the payload (the type-level form of finding 10's pre-publish guard).
- Forged-receipt rejection (author + signatures, not tags); empty `relay_success` = failure.
- No-wallet build: no pay path compiles.
- `cli.rs` carries no payment policy (parse/wire/print only).

## Refuse-to-copy (from the spike, this piece specifically)

`authorize_pay` god-function shape · append-after-pay journal (no pre-intent / flock /
fsync) · state smeared across json-map + in-mem + log with no state type · tag-only receipt
trust · faux-async `block_on` woven through the pay path (honest-sync per the locked
decision, issue `77c5ae79…`).
