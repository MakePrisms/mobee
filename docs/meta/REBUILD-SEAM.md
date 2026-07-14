# Rebuild Seam — `spike/full-loop` → `main`

Maintained by Scribe (forge builder team). One map: what the spike holds, what `main` has
absorbed, the ordered plan for the rest, what must never cross, and where spec and code
disagree.

**Pinned refs (2026-07-14):**

- `main` = `b5003d4` · `spike/full-loop` = `0e77669` · `mobee-specs` = `5db6dcd` ·
  merge-base = `e066a50a`
- The spike is a moving target (checkpoint (c) live e2e in flight on the mobee channel as of
  02:20Z). This map is exact at these refs — **re-pin before executing any piece.**

Companions: [SPIKE_LESSONS.md](SPIKE_LESSONS.md) (rebuild constraints + refuse list),
[PROCESS.md](PROCESS.md) (merge train, review authority), [STATE.md](STATE.md) (lane ledger).

**Review classes used below:**

- **MONEY** — money/crypto path: dual-review (independent cold review + adversarial
  verifier that does not inherit the builder's framing), suite re-run by the verifier on the
  frozen commit, then gudnuf. Real-funds anything additionally gates on R1–R3 (STATE.md).
- **STANDARD** — one reviewer + gudnuf.

Every piece is **rebuild-track**: a reviewed PR to `main`, operator merges, no self-merge
(PROCESS.md).

---

## 1. Inventory — spike `0e77669` vs `main b5003d4`, by subsystem

Topology: the spike forked at `e066a50a`, *before* docs/meta genesis (#1) and piece-1 (#2)
landed — so the spike does not contain piece-1, and `main` advanced independently. Spike
delta vs main: 21 commits (2026-07-12→13), 17 files, +10182/−8.

| Subsystem | On `main` | On spike only | Seam status |
|---|---|---|---|
| ACP spine (driver, mock, engine, event log, dev CLI, evals, Nix) | ✅ v0.1 (pre-fork) | streamed-content capture delta (`engine.rs` +76, `driver/acp_driver.rs` +65) | piece-5 |
| Format check + receipt H-tuple | ✅ piece-1 @ `b5003d4` (`format.rs` 270, `receipt.rs` 93 — clean re-extraction) | spike's own older copies (+221/+86, predate piece-1) | done; spike copies = refuse-list |
| Gateway protocol types (kinds 5109/6109/7000/3400, MOBEE_TAG, v1, `TESTNUT_MINT_URL`, TagSpec/EventDraft/OfferDraft/ParsedOffer, targeting) | ⏳ arriving as piece-2 (PR #5 @ `c0e604c`, +567 distilled) | `mobee-core/src/gateway.rs` +603 (source material) | piece-2 in flight |
| Token verification (wallet) | — | `mobee-core/src/wallet.rs` +413: `verify_p2pk_token` :109 (sum==amount, NUT-11 P2PK lock, NUT-07 per-proof unspent), mint hard-bind :121, `cashu_proofs_from_token` :176 (V3/V4, keyset-free), `fetch_nut07_states_for_proofs` :217. Feature `wallet=[cashu,cdk]`. 4 hermetic tests | **piece-3 (recommended next)** |
| Payment send | — | `mobee-core/src/payment_send.rs` +254: canonical-JSON payload, `PaymentSend` trait + memory fake, `NostrPaymentSend` (NIP-17 14→13→1059), gift-wrap timestamp clamp 180 s (:102, :202) | piece-4 |
| Payment state machine + idempotency journal | — | none as a library — semantics live inside `authorize_pay` (`cli.rs:1844`) + append-after-pay journal | piece-6 (design, not extraction) |
| Git-delivery gates | — | all in `cli.rs`: `verify_git_delivery` :4388 (fetch-before-pay, branch→oid equality), `verify_git_descendant` :4412 (strict descendant, rejects commit==baseline), `ensure_repo_job_protection` :2779 (authoritative kind:30617 by owner+`d`, clone/web echo, buzz-channel bind, no-force-push cover), `parse_relay_git_repo_identity` :4123, ref-pattern grammar mirror :4210 | piece-7 (HOLD — see §2) |
| Buyer MCP + seller gateway loop | — | `crates/mobee/src/cli.rs` +4862 god-file: `BuyerMcpServer` :1389, locked v0 five-tool surface :1579-1583, idempotency journal (`--idempotency-log`/env :1278), `confirm_receipt` :2205, `require_relay_success` :2489, accept idempotency :1754, testnut fee headroom `=8` :36, mint-token non-testnut refusal :1320, gateway subcommands :491 | piece-8 (thin re-skin, last) |
| Dependencies | — | `Cargo.lock` +3560 (nostr-sdk, cashu, cdk, cdk-sqlite, tokio-tungstenite) | regenerate per piece — never copy |
| Junk | — | `.scratch/real-acp-gudnuf/` committed; `.gitignore` is `/target`-only on both branches | refuse-list |

Nothing on `main` is behind the spike except this extraction backlog; nothing on the spike
supersedes piece-1 or the docs.

---

## 2. Ordered back-merge plan

Landed / in flight:

- **piece-1 ✅ merged** — format + receipt as pure core modules (`b5003d4`, PR #2).
- **piece-2 ⏳ open** — gateway protocol types (PR #5 @ `c0e604c`; hermetic, fail-closed
  targeting, nostr builder feature-gated; scope explicitly excludes CLI/money). Awaiting
  gudnuf. STANDARD (types only, no money movement).
  - **#5 operator-review, item 2 (BLOCKING merge, in flight 2026-07-14):** drop the public
    testnut constructor surface from the library API — remove `pub TESTNUT_MINT_URL` +
    `OfferDraft::testnut`/`::untargeted_testnut`; `OfferDraft::new`/`untargeted` take an
    explicit `mint_url`; the testnut URL lives only in `#[cfg(test)]` fixtures; mint policy
    stays out of gateway types. (This item rotted after a 01:38 claim was pulled onto
    checkpoint-c — recorded here so it can't rot silently again.)
  - **piece-2.1 follow-ups (gudnuf #5 review items 1 & 3, NOT merge-blocking, tracked so
    they don't rot):** (1) an **SDK-boundary conversion pass** — where gateway types cross
    into/out of the SDK surface, make the conversion explicit and tested; (3) **per-kind
    draft structs** — replace the single `EventDraft`/`OfferDraft` shape with per-kind draft
    types so each event kind's required fields are type-enforced, not runtime-checked.
    Class STANDARD; own PR(s) after #5 merges.
  - **#6 stack note — RESOLVED:** the #5 item-2 fix landed at `2ed25ea` (explicit-arg
    ctors, `cfg(test)` testnut, grep-clean API removal, 29/29 both feature sets). The
    coordinator ran the #6 stack-check against it — throwaway rebase compiles + 35/35, so
    removing the testnut API did **not** break #6's stack; no API-removal carry needed on
    the retarget. #6's retarget still carries only the two ride-alongs (`.gitignore`
    refuse-#10 + the `canonical_json` public-signature tightening).

Then, in order:

### piece-3 — trade-verification policy over cashu types · **MONEY** · **CDK-FIRST · ✅ MERGED to main @ `dee436e` (#8)**

Superseded the original "lift `wallet.rs` mirrors" plan (operator direction + cdk-surface
map, 2026-07-14, all source-verified at cdk/cashu `=0.17.2`). PR #8 lifted hand-rolled
mirrors of things the `cashu` crate already owns; the rework **deletes the mirrors** and
keeps Mobee to trade policy only. The rework landed in place on PR #8 (**MERGED to main
2026-07-14, squash `dee436e`**) — mirrors gone, Mobee holds only trade policy; the four
fix-window items below all verified on the behavior-CLEAR head `5c596a69`, then a docs-only
comment trim (operator hygiene ask: minimal API-contract rustdoc, trap knowledge in tests +
PR body) landed at `172cdeda` and merged. `verify_trade_p2pk` + the typed `CurrencyUnit`
`TradeLock`/`VerifiedPayment` are now **main API surface** (the offer-unit → `TradeLock.unit`
wiring is the #6 residual, landing against these real types).

**DELETE from Mobee (each has an exact `cashu` source-of-truth):**
- `P2pkSecret` / `parse_p2pk_secret` / `p2pk_secret_json` → `cashu` `nut10::Secret` +
  `impl TryFrom<&Secret> for SpendingConditions` (nut10/spending_conditions.rs:95).
- `CashuProof` DTO → `cashu::Proof` (nut00/mod.rs:366).
- manual `hash_to_curve` for `y` → `Proof::y()` (nut00/mod.rs:411) / `dhke::hash_to_curve`
  / batch `ProofsMethods::ys()`.
- `Nut07State` mirror → `cashu` nut07 `State`/`ProofState`/`CheckStateRequest/Response`.
- bespoke NUT-07 HttpClient → `cdk::wallet::MintConnector::post_check_state` (trait). Core
  stays generic over the public `MintConnector`; tests inject cdk's public generic
  `BaseHttpClient<T>` with a tiny in-test `HttpTransport` fake returning `CheckStateResponse`
  — this mocks the **transport** (exercising cdk's real connector impl), not the connector.
  ⚠ Do NOT try to reuse cdk's `MockMintConnector`: at `=0.17.2` it is behind a module-level
  `#![cfg(test)]`, so it is never compiled for a dependent and is not importable (E0432,
  compiler-confirmed). The transport-fake is the source-compatible path.

**KEEP in Mobee (pure core, `cashu`-only, zero I/O):** a thin
`verify_trade_p2pk(token: &cashu::Token, lock: TradeLock, states) -> VerifiedPayment`
composing `Token::value() == lock.amount` + **`Token::unit() == lock.unit`** +
`Token::mint_url() == lock.mint` + **every proof's secret is P2PK with primary
`data == lock.seller_lock`** (reject non-P2PK / malformed) + **no duplicate `y`/secret +
checked-amount** (the security we added on #8 — carry forward, do not regress);
⚠ **CURRENCY UNIT MUST be bound (codex HIGH, 2026-07-14):** `TradeLock` carries a bare
numeric amount; without a unit check, a same-mint token with `Token::unit() == Msat` and the
equal NUMERIC amount satisfies value+mint+lock — a **1000× value hole**. `TradeLock` gains a
`CurrencyUnit` (or hard-requires `Sat`); `verify_trade_p2pk` asserts `token.unit()` matches
and **fails closed on `None`/other**. Regression: msat token, same number, same mint,
seller-locked → REJECT.
⚠ **`Token::p2pk_pubkeys().contains(seller)` is INSUFFICIENT (Temper BLOCK, probe-reproduced
on cashu 0.17.2, 2026-07-14):** that helper UNIONs pubkeys across proofs AND silently skips
non-P2PK / non-Nut10 secrets, so a mixed token (1 seller-locked + 99 other) or hybrid
(1 seller-P2PK + 99 non-Nut10) yields `contains(seller)=true` → a false Ok. The `Ok` must
mean "every sat is seller-keyed," not "seller appears somewhere." Parse per-secret
`SpendingConditions` and require ALL to be P2PK-locked to the seller. Not a fund-loss under
piece-6's swap-on-receive (which fails closed on non-seller-spendable proofs) — but piece-6
is UNBUILT, so shipping the oversell parks a footgun for any consumer treating `Ok` as
trade-complete. Strict is still pure (cashu types), so there's no hermetic cost.
DLEQ math via `Proof::verify_dleq(mint_pubkey)` (offline). No single cdk fn does
"mint+amount+P2PK" in one call — `Wallet::verify_token_p2pk` is async + Wallet-bound +
checks no amount, strictly worse for a hermetic core, so Mobee keeps this ~5-line pure
composition.

**Two load-bearing traps (cdk-surface map — acceptance MUST cover both):**
1. The pre-pay lock check reads `SpendingConditions` / `Token::p2pk_pubkeys()` — **never
   `Proof::verify_p2pk()`**, which verifies signatures already on the proof and returns
   `SignaturesNotProvided` on an unsigned pre-pay token.
2. Computing `ys` for the spent-check needs **only the secret** (in the token) — hermetic,
   no keyset resolution. `Token::proofs(&keysets)` (which needs a mint keyset fetch) is
   required only for the swap, not for parse/amount/P2PK/ys.

**Feature / hermetic boundary:** `wallet = [cashu, cdk]` for the `MintConnector` trait +
`HttpClient`, **no `cdk-sqlite`** (the #8 rule holds — cdk-sqlite is the only
`WalletDatabase` impl and would drag rusqlite + tokio into core). Core policy is defined
over the `MintConnector` trait; prod injects `HttpClient`, tests inject a
`BaseHttpClient<T>` with an in-test transport fake (see DELETE list) — zero network/db. Default build has **no pay path** (safety gate).

Acceptance:
- Pure-core tests over an injected `BaseHttpClient<T>` + in-test transport fake, zero I/O,
  **no `cdk-sqlite`/`rusqlite` linked (no database — `cargo tree` clean, #8-verified).**
  ⚠ Correction (Anvil, #8, source-verified): cdk's `wallet` feature ITSELF pulls `tokio` in
  0.17.2 (`wallet/mod.rs` uses `tokio::sync::RwLock`), so "no tokio" is not achievable behind
  the wallet feature — the enforceable invariant is **no-DB**, not no-async-runtime. State it
  precisely in the PR (don't claim the impossible). **Seam ruling (Scribe, on codex MED,
  2026-07-14): ACCEPT-DOCUMENTED for #8** — the money-safety property is that the DEFAULT
  (no-wallet) graph stays tokio-free (evidence: `cargo tree` default shows no tokio); tokio
  inside the opt-in `wallet` feature is cdk's unavoidable 0.17.2 shape and does not block two
  money HIGHs on a dependency-hygiene debate. **piece-3.1 hygiene follow-up (deferred):** if
  we later want the `wallet` graph itself tokio-free, define a narrow local check-state trait
  instead of leaning on cdk's `MintConnector` — not blocking, tracked here so it doesn't rot.
- `verify_trade_p2pk` rejects: wrong mint, wrong amount, seller-lock absent, duplicate
  `y`/secret, amount-sum overflow, any proof reported spent, **any proof not P2PK-locked to
  the seller** — each with a test. The last is the Temper-BLOCK strict rule: **mixed-lock**
  (1 seller + N other-pubkey P2PK proofs) and **hybrid** (1 seller-P2PK + N non-Nut10
  secrets) tokens MUST be REJECTED — observe RED on the union-contains head, GREEN after the
  per-proof fix (red-before-green).
- Both traps regression-covered (unsigned token → lock check via SpendingConditions passes/
  fails correctly and never calls verify_p2pk; ys computed without a keyset fetch).
- Testnut fund-isolation is NOT here — it lives at the buyer-**mint** edge where
  `expected_mint` is chosen (piece-6/8); `verify_trade_p2pk` is mint-*matching* mechanism.

### piece-4 — payment-send library · **MONEY**

Lift `mobee-core/src/payment_send.rs` @ `0e77669`: payload canonical JSON, `PaymentSend`
trait + memory fake, `NostrPaymentSend` NIP-17, gift-wrap timestamp clamp. Depends on
piece-2 (core `gateway` feature / nostr dep).

Acceptance: the 3 spike hermetic tests green
(`payment_send_payload_canonical_json_is_stable`, `memory_payment_records_metadata_only`,
`gift_wrap_timestamp_tweak_stays_inside_relay_freshness_window`); trait consumable with the
memory fake and no network; review asserts proofs ride only the private DM path, never a
public receipt (spec §4).

Amended by the 2026-07-14 codex round (each a documented deliberate divergence from the
lift): delivery returns METADATA ONLY — event id + relay success/failed lists, no bearer
material in return types, and token-bearing types do not derive `Debug`/`Serialize`;
total relay failure fails closed (`empty output.success ⇒ Err`, with regression — an Ok
named "Delivered" that nobody received is the mechanism lying); the memory fake is gated
`#[cfg(any(test, feature = "test-support"))]` (it constructed empty success as a happy
path — a production-wireable silent no-op); `buyer_pubkey` derives from the delivery
signing key (sender == buyer in v1; a delegated-sender flow arrives as a designed change);
the plaintext-exclusion test is structural (unwrap via NIP-59, assert rumor kind 14 +
decryptability), not substring-only.

### #6 REWORK — rename + typed Token, IN-PR before merge · **MONEY** · **HELD**

**Operator override (gudnuf/mobee-meta, 2026-07-14): merge #6 in the shape we want, not
"land then rename/type later."** #6 is **HELD** — reworked before merge, then ships. This
**supersedes** the earlier "#6 lands as-is; typed-Token = piece-6 intake" line; the separate
4.1 rename-after-merge is **cancelled** (the rename is in this rework). Keep #6's proven
mechanism (gift-wrap / fail-closed / metadata-only); do **not** pull piece-6 scope (payment
SM, journal, buyer-mint Wallet, seller receive/swap) into it.

In scope for the #6 rework:
1. **Rename** token-send vocabulary to payment-send vocabulary: old send verb →
   `send_payment`, old delivery trait/payload/result names → `PaymentSend` /
   `PaymentPayload` / `PaymentSent`, plus module/path names; SM vocab
   `intent → locked → sent → receipt-published → closed`; **no payment-token
   delivery residual** (reserve "delivery" for git work product).
2. **Typed Token**: payload holds in-process `cashu::Token` (+ `MintUrl`/`Amount`), **not**
   `token: String`; serialize `cashuA/cashuB` **only** at the NIP-17 envelope boundary;
   seller **parses `Token` first, fail-closed, before any SM/state advance**; feature-gate so
   default (no-wallet) builds stay cashu-free. Correlation fields stay mobee newtypes.
   **This closes finding-10 in #6** — the corrupt token becomes unconstructable, not
   deferred to piece-6.

Acceptance (MONEY bar — composition + Temper adversarial + codex; the 5 traps Temper posted
2026-07-14 are the money-adv checklist):
- **blocker** — no public/`pub(crate)` path accepts a `String` token (CLI `--test-token`,
  helpers, parse-skipping tests): a non-token string is **unconstructable** before NIP-17,
  not merely rejected after unwrap.
- **blocker** — default/no-wallet build compiles without cashu; `PaymentPayload` holding
  `Token` must not force cashu into the default graph (`cargo check` default green).
- **high** — cashuA/B string exists only inside the gift-wrap encoder; no plaintext
  rumor/content/log/test echoes the token (the gift-wrap-bytes-carry-no-proof property, at
  the bar #6 already set).
- **high** — seller receive parses `Token` before SM/state advance; garbage after unwrap
  fails closed with no side effects.
- **medium** — rename completeness: grep-gate on old token-send identifiers residual
  in paths, re-exports, docs, error strings.

✅ **MERGED to main 2026-07-14** (PR #6, squash `cec8607`) — gudnuf rebased onto current main
(on top of #8) and merged direct from his IDE, carrying both clean-cut items (rename + typed
`Token`). Landed shape verified on main: module `payment_send.rs`; `PaymentPayload { token:
cashu::Token }` typed; the `String` lives only on `PaymentEnvelope.serialized_token` with
`TryFrom<PaymentEnvelope>` parsing it first (corrupt token **unconstructable**). piece-4.1
rename = **closed (landed in-PR)**, no separate rename PR. Temper runs a bounded post-merge
audit on `cec8607` (5 traps + a `Token` round-trip regression); findings are follow-up PR
material, not blockers — main is the operator's call. **Residual → piece-6:** offer/result
unit → `TradeLock.unit` construction site (receipt `unit` is still `String`); this is Q4 of
the piece-6 kickoff.

### piece-5 — streamed result-content capture · **STANDARD**

Lift the `engine.rs`/`acp_driver.rs` deltas: AgentMessageChunk text capture → result
content (feeds `result_content_hash`, spec §5), audit logging, post-terminal drop. Depends
on nothing.

Acceptance: `agent_message_chunks_are_logged_for_audit`,
`post_terminal_updates_are_dropped`, `stream_without_terminal_appends_failed_and_returns_err`
green on `main`; existing engine/event-log suite not regressed.

### piece-6 — payment state machine + write-ahead journal · **MONEY** (design piece)

**LOCKED 2026-07-14 — the design is the source of truth in
[PIECE-6-PAYMENT-SM.md](PIECE-6-PAYMENT-SM.md)** (folded from the team Q1–Q6 debate + the
`attempt_id`/reconcile money invariant; keeper:hearth locked the round; main pinned
`cec8607`). Not an extraction. Lock highlights (full detail + acceptance in the doc):
- **Three layers:** `PaymentMachine` pure reducer (`state = fold(replay())`, zero I/O) ·
  `PaymentJournal::lock(key) -> Guard` critical section (flock/mutex held across
  decide→effect→record, fsync-before-effect, torn-tail fail-closed, Memory fake behind
  `test-support`) · `PaymentService` orchestrator holding the Guard + firing injected effects.
- **Pay-once proof:** `attempt_id` at Intent + buyer-mint `lock_or_reconcile(attempt_id,
  terms)`; ambiguous-crash recovery = reconcile or refuse, **never blind re-mint** (WAL alone
  is insufficient).
- **States (locked):** `Intent → Locked → Sent → ReceiptPublished → Closed`.
- **NUT-07 (Q2):** reducer zero-I/O; edge fetches from the *token's* mint + `verify_trade_p2pk`
  holding the Guard before every `Locked→Sent`, passes typed `VerifiedPayment` — never a raw
  caller states-map; no TTL.
- **Relay (Q3):** any non-empty `relay_success` ⇒ Sent; empty ⇒ Locked; **no auto-resend**
  (gift-wrap non-deterministic, `PaymentSent` metadata-only); Sent ⇒ hard-refuse re-pay;
  relay-repair deferred.
- **One constructor (Q4):** validated offer → typed `terms{MintUrl, Amount, CurrencyUnit,
  seller_key}` builds `TradeLock` + key + payload; testnut allowlist here; unit parsed
  fail-closed; post-mint `Token` ≡ terms before `Locked→Sent`.
- **Idempotency key (Q6):** `(job_id, result_id, content_hash, job_hash, seller_pubkey,
  amount, unit, mint)` — typed fields (no raw-string compare), streamed content/job hashes
  (pieces 1/5, not serde field-order), Token/proofs/relays OUT (key exists at Intent).

**Two clean MONEY cuts (Q5 = A):**
- **PR1 (core, hermetic) — ✅ MERGED to main `b741eaf` (PR #10, 2026-07-14):** reducer + Guard
  journal + attempt_id/reconcile + injected effect traits + `test-support` fakes + hermetic
  tests (pay-counter ≤1 retry/crash/concurrent · WAL ordering · reconcile-or-refuse recovery ·
  total-send-fail stays Locked · receipt-retry · torn-journal refusal · Token≡terms guard ·
  journal durability: replay-sync + newline-commit-marker + parent-dir fsync). Three-legged
  bar cleared (composition + Temper adversarial/durability + codex deep, which found+fixed 3
  HIGH crash-durability). **No runnable pay path**; claims **double-pay closure ONLY**.
- **PR2 (edge) — CHARTERED 2026-07-14** (next money cut; Anvil builder off `b741eaf`; **4-leg
  bar**: Temper primary adversarial + metadex second-adv [seller-swap scars `741dcaab`] + codex
  deep + my composition; each acceptance item = a named non-vacuous RED→GREEN on the frozen
  head):
  1. real cdk `lock_or_reconcile` (over Wallet persistence — pending proofs / quote state; **NO
     mobee compensation**, cdk late-bind holds); **testnut allowlist at the mint edge**.
  2. **R1** — validate minted `Token` ≡ terms *before* persisting `Locked` (mismatch stays
     `Intent`/reconcilable, never bricks to `AmbiguousSendRefused`).
  3. seller receive/**swap authenticity gate** — swap-at-mint (or full keyset + DLEQ) before
     the SM advances at receive; **NUT-07 alone ≠ authenticity**; inflated-amount-on-real-`y`
     MUST fail closed (finding 8 / DP-2).
  4. **R2** — real `verify_trade_p2pk` on `locked.token()` with the real seller lock before send.
  5. NIP-17 `PaymentSend` adapter wired as the real send effect + client-disconnect (#6a
     pattern) + buyer_pubkey restore-on-parse + seal-sender ≡ envelope-buyer (audit finding-3).
  6. one edge unit-constructor (offer → typed terms; CLI/adapters never parse units).
  Optional R3 `unit=None` regression. Fence: no relay-repair/resend (Q3), no git-delivery
  (piece-7), no CLI/MCP re-skin (piece-8); journal stays trade-state-only. Un-draft only on my
  COMPOSED-DONE; merge = gudnuf. "Money-safe end-to-end on testnut" is claimable **only after
  these probes clear on the frozen head** — not a happy-path demo.

Depends on pieces 1–5 + #6 (all merged). R2 precondition ("durable pre-pay intent") on the
real-funds path. Spike's `authorize_pay` (`cli.rs:1844`) + append-after-pay journal are
reference semantics only — refuse-listed.

The named MUST gates — testnut allowlist (mint edge), NUT-07 wire contract, **authenticity
swap = PR2**, checked-arithmetic (landed piece-3, no regress), mint-URL normalization, receipt
authority (author+signatures; empty `relay_success` = failure), no-wallet-no-pay-path — are
enumerated with their PR assignment in the doc's *Inherited gates* + *Acceptance* sections
(source of truth; not duplicated here, to avoid drift).

### piece-7 — git-delivery gate library · **MONEY** · **HOLD**

Library-ize the five `cli.rs` gate functions (inventory row above) behind the `gateway`
feature. **Held deliberately:** spec §2.5 is the one section still explicitly spike-stage
("additive, not yet locked") *and* checkpoint (c) is exercising exactly this surface live
tonight — extracting now churns. Un-hold when §2.5 locks and checkpoint (c) lessons land.
Resolve inside this piece: ref-pattern mirror → import `buzz_core::git_perms::RefPattern`
or conformance-test against it (Sting residual); the M5 empty-commit SHOULD (§4 drift #1);
seller-membership check (Sting's declared gate for "repo authorization complete").

Acceptance (when un-held): `relay_git_repo_identity_parses_authoritative_owner_and_repo_id`
+ `repo_protection_ref_patterns_match_buzz_grammar` green in core + a conformance suite vs
the relay grammar; fetch-before-pay and strict-descendant semantics byte-equivalent to the
reviewed spike behavior (diff-reviewed); baseline oid journaled via the piece-6 journal.

### piece-8 — thin CLI + buyer-MCP re-skin · **MONEY**

Rebuild the binary surface as adapters over core: gateway subcommands + `BuyerMcpServer`
exposing exactly the locked v0 five tools (post_job, get_job, accept_claim, authorize_pay,
confirm_receipt). Lands the locked arch decisions that only make sense here: honest sync
(kill faux-async `block_on`, issue `77c5ae79…`), `job_id`→`execution_id` spine rename
(issue `9f9e9d0f…`), and the **targeting-seam alignment** — RESOLVED
direction (coordinator / NIP-89-open ruling 2026-07-14, confirmed live: gudnuf's test-kit job
posted with **no p-tag**, correctly): buyers naturally post **untargeted (open)** offers, so
**sellers claim open/untargeted offers** (price / amount / testnut-mint caps unchanged,
fail-closed money rules identical) rather than targeted-only. Piece-8's seller adopts the
open-offer default; the spike's targeted-only filter is what to loosen. Depends on all prior pieces.

Acceptance: `buyer_mcp_tool_schema_exposes_exact_v0_surface` +
`buyer_mcp_authorize_pay_requires_acceptance_and_observed_result` +
`buyer_mcp_payment_journal_blocks_retry_after_receipt_publish_failure` +
`buyer_mcp_receipt_replay_requires_buyer_author_and_valid_signatures` +
`buyer_mcp_journal_hit_without_receipt_requires_receipt_publish` green against the
core-backed implementation; review asserts `cli.rs` carries no policy (parse/wire/print
only); nix "boring targets" build; **testnut triple-gate semantics preserved end-to-end**
(mint-token refusal + offer/options/token binds, with the fund-isolation test at this
policy layer — the testnut-allowlist gate, deliberately distinct from wallet.rs's
mint-equality mechanism; see piece-6 note); **secret intake is env/file, never argv**
(the c2 rig-delta finding made permanent: `--key`-style argv secrets are refuse-class in
the rebuilt CLI).

**UX spec (design-banked, NOT chartered — operator intake gudnuf 2026-07-14; protocol train
keeps priority).** Buyer MCP manages keys + money under the **ALLOWANCE-NOT-BANK** principle:
- **First-run autogen**: MCP generates its own Nostr key + a local cashu wallet at `~/.mobee/`
  (env override). No human key-paste ever (the 7/14 kit bug: a leaked privkey came from a
  human pasting it — the product must make that unnecessary, not merely discouraged).
- **Human money actions = only two**: fund via a Lightning invoice + set **budget caps**
  (per-job max + total max, **MCP-enforced**). The agent spends freely *inside* the caps,
  never past them.
- **Per-job minting inside the piece-6 pay flow** (never a manual mint-token step by the human).
- **Vetted mint list** default (testnut + the piece-6 allowlist policy).
- **`setup_wallet` tool** for conversational first-run.
- **Packaging**: prebuilt binary + zero-arg `claude mcp add` (nix / brew / cargo-dist; MCP
  bundle for Desktop later). No build/mint inside the process Claude launches for MCP.
- **Profile tooling** (operator intake 2026-07-14): kind-0 profile publish/update — and later
  NIP-89 `31990` seller announces — is a **first-class verb** in the shared `~/.mobee` config
  layer (CLI now, e.g. `mobee profile …`; MCP tool later). Agents publish their kind-0s so the
  observatory + census light up with zero further work.

**MCP-vs-CLI split (seam ruling, operator 2026-07-14).** **One binary, one core library, two
surfaces:**
- `mobee <verb>` **CLI** — humans / ops / scripts / the seller-daemon; keeps full parity verbs
  for nostr + wallet ops (tonight proved you debug & operate without an agent in the loop —
  standalone `mint-token` was the diagnostic path).
- `mobee buyer-mcp` **MCP** — the agent-buyer product journey, **self-sufficient for the buyer
  loop** (setup_wallet / fund / budget / post → pay). The buyer never *needs* the CLI.
- **Both surfaces read the same `~/.mobee` state** — keys / wallet / profiles managed once.
  Precedent: buzz's shape (CLI-first core + separate protocol adapters over it).

**MCP transport acceptance (kit-bug lesson made permanent).** MCP stdio is **newline-delimited
JSON-RPC in BOTH directions** (read + write); diagnostics stderr-only; **no `Content-Length` /
LSP framing** on responses. Regression: an `initialize` over newline-delimited stdio returns a
single newline-delimited JSON-RPC result. (The spike's `write_mcp_response` emitted
`Content-Length` and hung Claude Code's MCP client — healthy server, unreadable framing;
uncaught because the spike never drove the server through Claude Code's MCP client. Never
re-copy the Content-Length framing.)

**Ordering logic.** 3/4/5 are independent extractions (any interleave is fine; 3 first —
highest money leverage, no dependency on PR #5, unblocks 6). 6 needs 1+2+3+4. 7 trails the
live spec on purpose. 8 is last because the skin can only be thin once core owns the policy.
The pre-existing evals flake (`scenarios_pass_deterministic_graders`, STATE.md) is
fix-or-drop per SPIKE_LESSONS — attach it to the first piece that touches evals rather than
carrying it silently.

---

## 3. Refuse-to-copy (refreshed 2026-07-14)

Canonical list: [SPIKE_LESSONS.md § Refuse to copy](SPIKE_LESSONS.md) — extended today with
the recon-sourced entries. Full current list:

1. `authorize_pay` god-function shape in `cli.rs` (any equivalent).
2. Static-token payment as the real path.
3. Append-after-pay journal (no pre-intent, no flock/fsync).
4. Tag-only receipt trust.
5. `.scratch/` artifacts committed.
6. **(new)** Spike's own `format.rs`/`receipt.rs` copies — predate piece-1; `main`'s
   re-extraction @ `b5003d4` is canonical. Never overwrite from spike.
7. **(new)** Hand-rolled ref-pattern matcher as a lasting shape (`cli.rs:4210` mirrors
   `buzz_core::git_perms::RefPattern` semantically) — import the relay grammar or
   conformance-test against it.
8. **(new)** `Cargo.lock` bulk copy (+3560 on spike) — regenerate per piece.
9. **(new)** Inline magic policy constants in the binary (e.g. testnut fee headroom `= 8`,
   `cli.rs:36`) — policy constants get a named core home with rationale.
10. **(new)** `/target`-only `.gitignore` — the gap that admitted `.scratch/`; each piece PR
    carries proper ignores.

---

## 4. Spec ↔ code drift — `mobee-specs @ 5db6dcd` vs `spike @ 0e77669` (flag-only)

Context: §2.5 (git delivery) is the **only unlocked spec section** (explicitly "additive,
not yet locked") yet the most recently patched — treat its flags as provisional-contract
drift. All other sections are locked.

**Verified aligned** (no action, recorded so nobody re-audits): buyer-MCP v0 five-tool lock
↔ `buyer_mcp_tool_schema_exposes_exact_v0_surface`; H-tuple order/domain (§5) ↔ receipt
golden `canonical_json_matches_locked_receipt_tuple_order`; gateway 4-check verify (§4) ↔
`wallet.rs:109` + mint hard-bind :121; announcement identity pin + grammar (§2.5) ↔
`ensure_repo_job_protection`:2779 + :4210 (Sting M2 re-review PASS, hearth independent PASS
@ `0e77669`); testnut hard-bind ↔ triple gate (`cli.rs:1877-1886`, :1915-1920, :1320);
fail-closed mode/echo/oid-slot/fetch-before-pay (§2.5 pay gates) ↔
`verify_git_delivery`:4388-4404.

**Flagged (code lags spec):**

1. **Empty-commit rejection (M5 SHOULD) absent** — `verify_git_descendant`
   (`cli.rs:4412-4462`) rejects commit==baseline and non-descendants, but never compares
   trees; an empty commit atop baseline passes. Spec permits deferring the SHOULD but
   forbids narrating "paid for work" while empties pass. Resolve in piece-7.
2. **Seller-membership pre-ACCEPT check (§2.5 SHOULD) absent** — protection check covers
   the announcement + protect rules, not seller push rights. Sting's declared gate before
   any "repo authorization complete" claim. Piece-7 (or earlier increment on spike).
3. **Ref-pattern grammar is a semantic mirror, not the relay's own** — contract-satisfied
   today (Sting re-review), standing drift risk. Piece-7 resolves (import or conformance).

**Flagged (spec lags code / naming):**

4. **`result_content_hash` (§5 tuple field) vs `delivery_integrity_hash` (§2.5 role
   rename)** — code keeps `result_content_hash_hex` naming while the slot carries the
   commit oid for git per C1. Shape identical; the naming should converge when §2.5 locks.
5. **Kind integers not frozen** (§8: 5109/6109/3400 are first-cut candidates pending a DVM
   registry pick) vs hardcoded constants `gateway.rs:5-12`. Fine pre-wire-freeze; the pick
   must precede any public NIP-89 listing.
6. **Specs-repo prose lags its own lock state** — README says "pending lock … not yet
   built" though §7 forks and buyer-MCP v0 are locked and shipped; file titles carry
   07-10 dates over 07-13 bodies. Owner: specs repo (c260cc43 key).

**Adjacent (code-vs-code, spec-relevant):**

7. **Targeting seam** — spec flow is claim-as-proposal → buyer accepts exactly one, with
   pre-assigned `p=seller` skipping the round; buyer-MCP posts *untargeted* offers while
   the seller gateway claims only *targeted* ones (STATE.md known issue). Live-e2e blocker
   until aligned; checkpoint (c) will hit it first.
8. **STATE.md lags tonight's motion** — piece-1 listed "Building" (merged @ `b5003d4`),
   spike pinned at `f3beb95` (now `0e77669`), "await PR #1" (merged). mobee-meta's file —
   flagged, not fixed here.

**Unaudited tonight** (nobody should read silence as "checked"): bare-`git` output-tag
guard (H1); `branch` ≠ default-branch enforcement point; baseline-journal crash semantics
(M6 passed at the `f3beb95` gate; mechanism is redesigned in piece-6 regardless); NIP-17
default vs nutzap alternative coverage; §5 sign-order (seller-first) enforcement.

---

## Addendum — 2026-07-14 live runs (checkpoints c, c2) + sprint

**Live-run status.** Checkpoint (c): full git-delivery loop live on the real relay,
single-key configuration (buyer == seller == owner). Checkpoint (c2): **arms-length
PROVEN** — distinct keys, distinct harnesses (metadex buyer / forge-team Anvil seller),
member-derived push, testnut settlement, distinct-sig co-signed receipt; four independent
verification layers; SETTLED-AND-VERIFIED at the coordinator gate. Reality class of both:
PLAY (spike-track) — the *contract* holds; the code remains spike code. Full evidence
chain: [RUNS-C2.md](RUNS-C2.md).

**Findings → plan deltas** (each already reflected in the piece sections above where
noted):

1. argv-only secret intake in the spike CLI → piece-8 permanent must-fix (env/file-only —
   already in piece-8 acceptance). A reviewed rig-local `--key-file` shim (0600-checked)
   was the c2 workaround; its shape is the piece-8 starting point.
2. Seller accept-window vs turn-based-buyer latency: the 300s default expired under a
   live buyer's turn cadence; 900s cleared it. Piece-8 names a window-vs-latency-class
   contract (offer deadlines and seller waits sized to the counterparty's declared class).
3. Anonymous relay reads failed before EOSE ×3 — independent verification of a *public*
   receipt chain currently requires authenticated reads, which is backwards. External to
   this plan: relay-side thread (keeper:buzz).
4. Member-push **positive** path proven (non-owner member pushed under `refs/heads/*`
   protects); the **negative** path (non-member seller → protocol-visible refusal) remains
   unproven — drift flag #2 and piece-7 unchanged.
5. Seller-side git helpers (`prepare_git_job_workspace`, `commit_and_push_git_delivery`)
   are dead code at `0e77669` — the seller git path is not first-class in the CLI; the c2
   seller did the git work outside the CLI and published via the cwd-output path. Piece-8
   scope note.
6. **Recovery-positive evidence for piece-6:** after a receipt-gate stall, the buyer
   reconstructed its journal from public events + the seller-verified delivery id and
   idempotently published *only* the missing receipt — no second pay (seller observed
   exactly one delivery). The paid-but-no-receipt recovery semantic held under a live
   interruption.
7. Repo commits carry git identities, not nostr identities — the receipt is the binding
   between seller pubkey and paid commit. Marketplace attribution UX note.
8. **Proof authenticity is unverified in BOTH the spike code and spec §4's locked
   four-check verify** (codex + Temper, 2026-07-14): a spec-conformant verifier accepts an
   inflated-amount token built on a real unspent `y` — sum/mint/lock/NUT-07 all pass while
   redeemable value ≠ presented value. Mechanism boundary documented on
   `verify_p2pk_token` in #8; the closing gate is piece-6's swap-on-receive MUST (above);
   spec fix flagged to the spec owner (§4 wants a fifth check or an explicit
   trust-boundary sentence + the swap gate named at settlement).
9. **Bearer material in return types** (codex #6 round, 2026-07-14): the delivery module
   returned the full token payload with `Debug`+`Serialize` derived — any success-path log
   or persistence of the return value writes spendable proofs to disk, and the module's
   own test fake normalized zero-relay success as a happy path. Standing rule from the
   ruling: token-bearing types do not derive `Debug`/`Serialize`; APIs whose names claim
   delivery fail closed on total relay failure. Both fixed in-PR (piece-4 amended
   acceptance above).
10. **Token corruption reaches the wire uncaught** (live codex-leg trade, 2026-07-14): the
    buyer pay path delivered a payload the seller parsed as neither V3 nor V4
    (`Unsupported token`) — the saved `cashuB`/V4 token did not enter the delivery payload
    intact (token-binding bug, buyer-side). The seller correctly fail-stopped (no receipt on
    an unparseable token), so nothing was lost — but the pay path sent a corrupted token
    with no pre-publish check. **Standing gate → piece-6:** the payment SM's Funded→Delivered
    transition MUST assert the token about to enter the delivery payload is a well-formed
    cashu token (`cashuA`/`cashuB` prefix + parses) **before** publish, fail-closed, logging
    prefix-class + length + delivery-id only (never bearer material). At testnut a corrupt
    token merely blocks; at real value an *attacker-crafted* malformed-but-plausible token is
    a worse class — this guard is the buyer-side sibling of the seller's swap-on-receive
    (finding 8). Both builders independently converged on this fix; recorded so the rebuild
    inherits it as a gate, not a one-trade patch. **✅ CLOSED-SUBSUMED on main (PR #6, squash
    `cec8607`, 2026-07-14):** the fix is the typed `cashu::Token` payload that landed in the
    **#6 rework** (see the #6 REWORK section) — `PaymentPayload` holds `cashu::Token`; the
    string exists only on `PaymentEnvelope.serialized_token`, parsed first via `TryFrom`
    (corrupt token unconstructable), so the stringly hole never reaches main. Only possible
    follow-up = a `Token` round-trip regression (Temper post-merge check), test-only. Not deferred to piece-6, and main does NOT ship
    a stringly payload. metadex/Anvil need not hunt the original encode site (unconstructable
    once typed).

**Sprint state (current 2026-07-14).** MERGED to main: PR #5 (piece-2 gateway types,
`46499b5`) · PR #7 (piece-5 capture, `91adf41`) · **PR #8** (piece-3) **✅ MERGED
2026-07-14** (squash `dee436e`) — CDK-first `verify_trade_p2pk` over cashu types (mirrors
deleted); four fix-window items verified on behavior-CLEAR head `5c596a69` (strict per-proof
seller-lock · currency-unit bind · rustdoc trust-notch · no-DB dep-graph; Temper CLEAR +
Anvil rev-parse + gh confirm), then a docs-only comment-trim hygiene pass (`172cdeda`,
verified comment-only vs `5c596a69`, trap knowledge moved to tests) merged by gudnuf.
**PR #6** (piece-4) **✅ MERGED 2026-07-14** (squash `cec8607`, gudnuf rebased onto #8 + merged
direct from his IDE — rename → `payment_send` + typed `cashu::Token`; finding-10
CLOSED-SUBSUMED; Temper post-merge audit on `cec8607` = follow-up only). **Foreground:
piece-6 payment SM** (design = source of truth [PIECE-6-PAYMENT-SM.md](PIECE-6-PAYMENT-SM.md);
Q1–Q6 + `attempt_id`/reconcile locked). **PR1 (core hermetic double-pay/WAL) ✅ MERGED
`b741eaf` (PR #10)** — 3-leg bar (composition + Temper adv/durability + codex deep found+fixed
3 HIGH crash-durability). **PR2 (edge authenticity) CHARTERED** — 4-leg bar (Temper primary +
metadex second-adv + codex deep + composition), Anvil builder off `b741eaf` (scope in
§piece-6). #6a Debug-redact (PR #11) MERGED (my composition CLEAR + gudnuf). Separate: PR #9 (network
observatory, STANDARD) COMPOSED-DONE + un-drafted for gudnuf. Every money PR runs the full
bar: independent-verifier mechanical + composition diff-read + Temper adversarial + codex
deep; each fix a documented deliberate divergence. Piece-5 inventory erratum absorbed: the
delta requires two 2-line enum variants (`driver/acp.rs`, `event.rs`) beyond the row's
named files — inseparable, documented in PR #7.

---

## Deferred problems (the to-do — named, not solved)

Named here because they are real and out of v1 scope by explicit operator decision — not
because they are closed. A future round picks each up from this list.

### DP-1 — fair exchange: buyer fetches work before paying

**Status:** gudnuf-ruled 2026-07-14. v1 keeps **plaintext fetch-before-pay** (shape (a)) —
the buyer resolves and reads the git deliverable before `authorize_pay`. This is the
documented, c2-proven money invariant (§2.5, [RUNS-C2.md](RUNS-C2.md)).

**The problem (gudnuf's words: "remember this will be a problem later"):** fetch-before-pay
is fine while prices are tiny (testnut, 1 sat) — the buyer's incentive to fetch-then-decline
is negligible. At **real prices it is real exposure**: a buyer can acquire the deliverable
and refuse to pay, and the seller has already surrendered the work. Fair exchange wants the
opposite — work acquirable only *after* payment, and never lost by the honest party.

**Designed fix (reserved, not built):** encrypted delivery over the **NIP-17 key rail** — the
seller delivers the work encrypted; the decryption key is released only against payment, so
neither party can strand the other. **PoPs escrow** covers the symmetric half (buyer's funds
committed before the seller commits the key). The result envelope **reserves the schema slot
now** so the flip never changes envelope shape (metadex, v1 values):

```json
{ "delivery_mode": "plaintext-fetch-before-pay",
  "encrypted_delivery_supported": false,
  "encrypted_delivery_required": false }
```

The later flip changes `delivery_mode` + flags, not the envelope shape. Until then §2.5
fetch-before-pay stands as the v1 invariant; this entry is the tracked to-do against it.

### DP-2 — proof authenticity at real value (see addendum finding 8)

Deferred only in the sense that the closing gate is **piece-6's** swap-on-receive MUST, not
yet built; and **spec §4** shares the gap (HIGH-for-spec, spec owner). At testnut the
inflated-amount attack costs nothing; at real value it is the same class of exposure as
DP-1. Do not narrate "money-safe at real value" until both the code gate (piece-6) and the
spec sentence (§4) land.
