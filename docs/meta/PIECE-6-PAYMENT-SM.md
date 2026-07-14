# Piece-6 design — payment state machine + write-ahead journal

Rebuild-track **piece-6** (see [REBUILD-SEAM.md](REBUILD-SEAM.md) § piece-6). This is a
**design piece, not an extraction**: the spike proved the money path works, but its shape is
refuse-listed. This doc is the **locked source of truth** the build round works against —
folded from the piece-6 debate the team ran 2026-07-14 (Q1–Q6 + the `attempt_id`/reconcile
money invariant), locked by keeper:hearth. Class: **MONEY** (composition + Temper adversarial
+ codex deep + operator gate). Main pinned for this train: `cec8607`.

Grounded in the spike at `0e77669`. The reference to learn from — and not copy — is
`BuyerMcpServer::authorize_pay` (`crates/mobee/src/cli.rs:1844-2203`).

**Consumes (already on main — call, do not rebuild):**
- `verify_trade_p2pk` / `TradeLock` / `VerifiedPayment` (#8, `dee436e`) — trade-binding +
  NUT-07 verify over cashu types; `Ok ≠ authenticity` (piece-3 rustdoc).
- `PaymentSend` / typed `PaymentPayload` (holds `cashu::Token`) / `PaymentSent` (#6,
  `cec8607`) — NIP-17 payment send, fail-closed on total relay failure, metadata-only return.
- format + receipt H-tuple + streamed content/job hashes (pieces 1/5).

## Why this is a redesign, not a lift

`authorize_pay` is a ~360-line god-function that, in one body, does: arg parse ·
idempotency short-circuit · seller/offer/result fetch + validate · git-delivery verify ·
integrity-hash compute + compare · receipt sign · journal write · pay · publish receipt ·
remember. SPIKE_LESSONS refuse-lists it by name. Two concrete scars:

1. **Append-after-pay (the double-pay window).** `remember_buyer_payment` is called *after*
   `pay_seller` returns (`cli.rs:~2150` vs the pay at `~2140`). A crash between the pay
   succeeding and the journal write leaves money moved with no durable record; recovery finds
   nothing and **pays again**. Small and real.
2. **State smeared** across a `serde_json` map + an in-memory `self.jobs` entry + an
   idempotency log, with no single type naming which state a job is in.

Good bones to preserve: the layered recovery short-circuits (`locally_paid && receipt` →
return; needs-receipt → republish-only; on-relay receipt fallback; locally-paid-but-no-record
→ **refuse**, never re-pay). c2 proved this recovery under a live stall — the redesign keeps
the behavior and gives it a spine.

## Architecture — three layers, never conflated

The single most important structural lock. Three layers with hard boundaries:

1. **`PaymentMachine` — pure reducer.** `state = fold(replay())`; `next = decide(state,
   event)`. **Zero I/O.** Owns transition legality only. This is what stays hermetic and what
   the pay-counter test pins. No stored-state API, no `load_state` to drift — state is always
   a fold over the journal.
2. **`PaymentJournal::lock(key) -> Guard` — the critical section.** The Guard holds
   exclusivity for the whole transition and exposes `current()`/`replay()` + `append_sync()`.
   Bare `append` + `replay` cannot express the lock lifetime (two callers could each replay
   Intent, each decide, then serialize two appends → double); the Guard can. Transition
   legality does **not** live here — only durability + mutual exclusion.
3. **`PaymentService` — orchestrator.** Holds the Guard and, inside the guarded section, runs
   the reducer and fires the **injected effects** (buyer-mint `lock_or_reconcile`, NUT-07
   fetch + `verify_trade_p2pk`, `PaymentSend`, receipt publish). Effects never live in the
   reducer or the journal.

This reconciles "zero-I/O reducer" with "guard owns concurrency" — different layers, one
guarded section.

## State machine

States and the ONLY legal transitions (operator-locked vocabulary — "delivery" means the
*work product* (git-delivery); the money leg is **payment send**, never "delivered"):

```
Intent  ──mint/lock──►  Locked  ──send──►  Sent  ──publish──►  ReceiptPublished  ──►  Closed
   │                                                                                          
   └── every transition journaled write-ahead (Guard: flock + fsync) BEFORE its side effect ─┘
```

- **Intent** — durably recorded (write-ahead) *before* any token is minted or spent. Holds
  the idempotency key (below) **and a stable `attempt_id`**. The record-before-effect is the
  fix for scar #1.
- **Locked** — token minted/locked to the seller via the buyer-mint edge's
  `lock_or_reconcile(attempt_id, terms)` (verify-not-spend; gateway holds only the public
  half).
- **Sent** — payment token sent to the seller over the private NIP-17 DM path
  (`payment_send`); **total send failure fails closed** (empty-relay-success ⇒ stays Locked).
- **ReceiptPublished** — buyer-authored co-signed receipt on the relay.
- **Closed** — receipt observed/confirmed.

The five public states are locked. Persist `attempt_id` and the effect result the next state
needs; do **not** invent an in-flight public state to hide crash ambiguity — the Guard plus
reconcile handle it.

## The pay-once proof — `attempt_id` + `lock_or_reconcile`

WAL alone does **not** close double-mint across a crash *between* the Intent write and the
mint effect. The proof is two parts:

- `attempt_id` is minted at **Intent** and written before any effect.
- The buyer-mint edge exposes **`lock_or_reconcile(attempt_id, terms)`**: idempotent by
  `attempt_id` — a second call with the same `attempt_id` **reconciles** to the existing
  lock/mint result rather than minting again.
- **Recovery at an ambiguous boundary (crash after effect, before the next append)
  reconciles via `lock_or_reconcile(attempt_id, …)` or refuses — NEVER blind re-mint.**
  Without `attempt_id`/reconcile, local WAL can prove safety only by permanent refusal, not
  honest forward recovery.

`mint/lock` fires **exactly once** across retry / crash / concurrent invocation. PR1 proves
it with a stubbed pay-counter (≤ 1).

## Scope split — trade state (ours) vs wallet recovery (cdk)

Source-verified at cdk `0.17.2` (wallet swap/melt sagas, compensation, pending-proof reclaim,
NUT-13 restore all exist). The split is explicit so core never reinvents what cdk owns:

- **mobee journal = TRADE state only** — the five states keyed by the 8-tuple. cdk cannot know
  job / result / seller identity, so **double-pay closure at the trade level stays ours**. The
  journal never manages proofs or duplicates wallet state.
- **Wallet-level crash recovery = DELEGATED to cdk at the PR2 mint edge.**
  `lock_or_reconcile(attempt_id, terms)` is implemented by querying cdk `Wallet` persisted
  state (pending proofs, quote status) — **not** mobee-written compensation logic. `attempt_id`
  is the thread tying the trade journal to wallet state.
- **Core only refuses-or-delegates** — no wallet-recovery machinery lives in `mobee-core`;
  reconcile is an **edge trait**. This *trims* PR1 (core carries the trade SM + journal, not a
  wallet-recovery engine).

## Write-ahead journal (Q1 lock)

- **`PaymentJournal::lock(key) -> Guard`.** Guard = `current()`/`replay()` + `append_sync()`.
- **FS guard (prod):** one append-only JSONL file; **`flock` held across decide → effect →
  record**; **flush + fsync BEFORE the side effect**; a torn / malformed tail on replay is a
  **fail-closed refusal**, never silent truncate-as-success.
- **Memory guard (tests):** mutex-backed, same contract, behind a **`test-support` feature**
  (`cfg(any(test, feature = "test-support"))`, matching #6's `MemoryPaymentSend`) — never bare
  `cfg(test)`, never a second product path. Hermetic tests run on the *real* SM path.
- **Recovery = pure fold over `replay()` filtered to the key**, then act by state:
  - `Intent` with no confirmed lock → **reconcile** via `lock_or_reconcile(attempt_id, …)`,
    never blind re-mint.
  - **recovered (replayed) `Locked` → REFUSE to auto-send.** The crash-after-send-before-`Sent`
    window is indistinguishable from never-sent (no event id is journaled until Sent), and
    re-send is non-deterministic (gift-wrap), so core stops and defers to observe / reconcile /
    human. A **newly reached** `Locked` (this run performed the lock, no send attempted yet)
    may attempt send **once**; empty `relay_success` stays Locked.
  - `Sent` → retry only the **idempotent receipt** leg (never re-pay, never re-send the DM).
  - `ReceiptPublished` / `Closed` → idempotent return.
  The newly-reached-vs-recovered distinction is a **runtime** fact (did *this* run attempt the
  send), **not a persisted sixth state**. "Locked/Sent but no record" is unreachable (record
  precedes effect). **Residual (known, PR2+/relay-repair):** a crash at `Locked` strands the
  trade pending manual reconcile — same observe/refuse/human class as the single-relay-drop
  residual; not a PR1 liveness bug.

**Idempotency key (Q6 lock)** — the seven SPIKE_LESSONS fields **+ `unit`**, all typed:

```
(job_id, result_id, content_hash, job_hash, seller_pubkey, amount, unit, mint)
```

- Typed / canonical fields only: `CurrencyUnit` (not string), `Amount`, `MintUrl` (normalized),
  seller pubkey normalized/typed — **no raw-string compares in the key** (the #8 Sat/Msat and
  mint-normalization scars must not re-enter here). `amount` without `unit` is ambiguous.
- `content_hash` / `job_hash` are the **streamed content/job hashes from pieces 1/5**, not a
  serde field-order "canonical_json".
- **Out of the key:** `Token`, proofs, serialized payload, `PaymentSent` id, relay set. The
  key exists at **Intent, before any effect** — the token is an outcome, not an identity, and
  secret-bearing / random transport material would destroy stable recovery. Buyer identity is
  hard-bound by the authored-job / accepted flow *before* key construction, not duplicated in
  the key.

## NUT-07 freshness (Q2 lock) — pure reducer, pinned orchestration

The reducer does **zero I/O**. Orchestration is pinned:

- The injected edge fetches NUT-07 spend-state from the **token's mint** (never a
  caller-chosen alternate) **while holding the Guard**, immediately before **every**
  `Locked→Sent` attempt, and calls `verify_trade_p2pk`.
- It passes a typed **`VerifiedPayment`** into the transition — **never a raw caller
  states-map** (a caller-supplied map is a spoofable surface; `VerifiedPayment` is produced by
  the verify, not asserted by the caller). No reuse across attempts, **no wall-clock TTL** —
  fresh-for-this-transition only.
- NUT-07 "unspent" is best-effort hygiene, **not** authenticity. The invariant that carries
  real money is pay-once (journal + reconcile) **plus** the seller **swap-at-receive** gate
  (PR2) — not fetch timing.

## Payment send — relay rule (Q3 lock)

- **Any non-empty `relay_success` ⇒ Sent**, and the complete `PaymentSent` metadata is
  persisted. **Empty `relay_success` ⇒ stays Locked** (unchanged from #6).
- **No auto-resend of the secret-bearing payment, ever.** Partial relay failures are
  diagnostics. `PaymentSent` on main is metadata-only and gift-wrap is **non-deterministic**
  (`custom_created_at` + ephemeral wrap key), so a payload-rebuilt "identical retry" is
  impossible — re-wrapping yields a new event id, not the same delivery.
- **Existence of `Sent` ⇒ hard refuse re-mint / re-pay**, regardless of incomplete relays.
- **Relay-repair is future work, explicitly out of PR1**: it would require `PaymentSend` to
  store/expose the exact signed event JSON and rebroadcast *that* — metadata is on-record as
  insufficient for it.
- **Residual (medium, logged not blocked):** if the single accepted relay drops the event
  before the seller reads it, recovery is **observe / refuse / human**, not resend.

## One constructor (Q4 lock) — terms are the single source

One payment-policy constructor in core, **before Intent**: `validated offer → terms`.

- `terms` is typed: `{ MintUrl, Amount, CurrencyUnit, seller_key }`. The **same** `terms`
  builds `TradeLock`, the journal key, and the payload/lock checks — one mapper, nowhere else.
- CLI / connector / `PaymentSend` **never parse unit strings**. The offer's wire unit-string
  is parsed to `CurrencyUnit` **here, fail-closed** (reject unknown, never default to Sat).
- **Testnut allowlist lives here** (the fund-isolation gate, at the buyer-mint edge where
  `expected_mint` is chosen — distinct from wallet.rs's mint-equality mechanism).
- **Post-mint bind:** after `lock_or_reconcile`, the minted `Token`'s unit / amount / mint
  must **equal `terms`** before `Locked→Sent`. **This guard is PR1 transition legality**
  (hermetically tested with a fake mint returning a mismatched Token → refuse; finding-2,
  coordinator-ruled onto PR1); the *real* mint that produces the Token is PR2.
  (`PaymentPayload` on main still carries a unit-less `amount_sats`; the boundary must not
  reopen the Sat/Msat hole when composing key + lock + payload.)

## PR slices (Q5 lock) — two clean MONEY cuts

- **PR1 — core, hermetic (this build).** `PaymentMachine` reducer + `PaymentJournal` Guard +
  `attempt_id`/reconcile contract + injected **effect interfaces** (traits) + `test-support`
  fakes + hermetic tests. **No runnable production pay path.** Its MONEY bar claims
  **double-pay closure only** — nobody says "money closed" at PR1.
- **PR2 — edge, authenticity slice (follow-up).** Concrete buyer-mint `lock_or_reconcile`
  (real `Wallet`) + seller **receive / swap authenticity gate** + concrete NUT-07 connector +
  testnut wiring. Its own full MONEY bar; the authenticity review is undiluted here.

Crate boundary (operator, 2026-07-14): near-term all **modules** in `mobee-core` + thin
CLI/MCP skins. Promote `buyer`/`seller` to crates only when nix wants distinct installables —
the module cut drawn now becomes the crate cut then. **Do not crate-split during piece-6.**

## Inherited gates (named MUSTs)

- **Authenticity gate (PR2 — codex #8 HIGH-2, closes the real hole):** before the SM advances
  past **receive**, the seller path **swaps the received proofs at the mint** (or fully
  crypto-verifies: retained `C` + keyset + DLEQ). `verify_trade_p2pk` checks presented proofs
  only; NUT-07 "unspent" is advisory. Regression: **Temper's inflated-amount token** on a real
  unspent `y` (sum/mint/lock/NUT-07 green, redeemable value ≠ presented) MUST fail closed.
- **Spec §4 shares this gap** (addendum finding 8): the four-check gateway verify has no
  authenticity check; PR2's swap gate is the code-side answer regardless of how §4 resolves.
- **Checked arithmetic** (landed piece-3, must not regress): `checked_add`, duplicate
  `y`/secret rejected before summing.
- **Receipt authority = author + signatures, not tags; empty `relay_success` = failure** for
  every money-path publish (SPIKE_LESSONS).
- **No-wallet build has no pay path** (feature gate = safety); the `PaymentPayload`-holds-
  `Token` type must not force cashu into the default graph.

## Acceptance

**PR1 (core, hermetic) — the merge gate proves all of these:**
- Stubbed pay-counter: `mint/lock` ≤ 1 across retry / crash / concurrent.
- `attempt_id` reconcile: crash-after-effect-before-record recovers by reconcile-or-refuse,
  never blind re-mint. Red-before-green: a blind-re-mint variant must fail the counter, and a
  truncate-tail-as-success variant must fail the torn-journal test (non-vacuous). **The
  recovered-`Intent` test asserts `reconcile(attempt_id)` is INVOKED and yields the existing
  lock (honest forward recovery) — not merely that the counter ≤ 1, since a refuse-always impl
  also gives ≤ 1 and must not pass as recovery.**
- WAL ordering: fsync-before-side-effect enforced; torn/malformed journal tail ⇒ fail-closed
  refusal on replay (not truncate-as-success).
- Recovery suite by state: Intent-no-lock → reconcile; recovered-`Locked` → refuse auto-send;
  `Sent`-no-receipt → republish receipt only; `ReceiptPublished` → idempotent return.
- Total-send failure: empty `relay_success` ⇒ stays Locked (does not advance to Sent);
  existence of Sent ⇒ hard refuse re-mint/re-pay.
- **Token ≡ terms guard (finding-2, coordinator-ruled onto PR1):** before `Locked→Sent` the SM
  asserts the (in PR1, faked) minted `Token`'s unit / amount / mint **≡ `terms`**, fail-closed.
  Hermetic mismatch regression, red-before-green: a fake mint returning a wrong
  unit/amount/mint must make `Locked→Sent` **refuse**. Closes the Sat/Msat-confusion class
  inside the double-pay PR; the *real* mint that produces the Token stays PR2. Pure transition
  legality over data already at the boundary — not a runnable pay path.
- Receipt retry idempotent; forged-receipt rejection (author + signatures, not tags).
- Journal fake is behind `test-support`, not a shippable second path; tests run the real SM.
- **No wallet-recovery machinery in core** — reconcile is an injected edge trait; core only
  refuses-or-delegates (cdk owns wallet recovery at the PR2 edge). The journal persists only
  the five-state trade record + `attempt_id`, never proofs / quote state / compensation.
- **No runnable production pay path compiles in PR1.**
- `cli.rs` carries no payment policy (parse/wire/print only).

**PR2 (edge) — its own MONEY bar:**
- Authenticity: inflated-amount-on-real-`y` fails closed at the seller swap gate.
- Post-mint bind exercised against the **real** minted `Token` (the guard itself lands in
  PR1 — see PR1 acceptance; PR2 runs it with real cashu Tokens from the concrete mint). Offer
  unit parsed fail-closed (unknown rejected, never defaulted) at the one constructor.
- **Validate minted `Token` ≡ terms *before persisting `Locked`* (Temper residual-1):** a
  mismatch must leave the key at `Intent` (reconcilable), NOT journal `Locked` then refuse.
  PR1's guard runs *after* the `Locked` append (`run` §Locked block), so a mismatch is
  money-safe but **bricks the key** to `AmbiguousSendRefused` on re-entry; PR2 reorders the
  validate before the `Locked`-journal so a bad mint result is recoverable, not bricked.
- **Seller-P2PK authenticity bind (Temper residual-2):** the real `verify_payment` adapter
  calls `verify_trade_p2pk` on `locked.token()` with the real seller lock before send. PR1's
  fake `verify_payment` ignores `locked`; the SM already threads `locked` through every
  effect, so PR2 only wires the real #8 verify — no SM change.
- Testnut allowlist: mint outside the test set → hard fail before any verify/mint.
- Optional: extend the mismatch regression with a `unit = None` case (Temper residual-3).
- No-wallet build still has no pay path.

## Refuse-to-copy (from the spike, this piece specifically)

`authorize_pay` god-function shape · append-after-pay journal (no pre-intent / flock / fsync)
· state smeared across json-map + in-mem + log with no state type · tag-only receipt trust ·
caller-supplied NUT-07 state map (spoofable) · payload-rebuilt "identical" gift-wrap resend
(non-deterministic — impossible) · wallet-recovery / compensation machinery in `mobee-core`
(cdk owns it at the edge; core only refuses-or-delegates) · faux-async `block_on` woven through the pay path
(honest-sync per the locked decision, issue `77c5ae79…`).
