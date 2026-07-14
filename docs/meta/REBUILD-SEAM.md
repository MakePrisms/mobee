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
| Token delivery | — | `mobee-core/src/delivery.rs` +254: canonical-JSON payload, `TokenDelivery` trait + memory fake, `NostrTokenDelivery` (NIP-17 14→13→1059), gift-wrap timestamp clamp 180 s (:102, :202) | piece-4 |
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

Then, in order:

### piece-3 — wallet token-verification library · **MONEY**

Lift `mobee-core/src/wallet.rs` @ `0e77669` essentially as-is (it is already
library-shaped): `verify_p2pk_token` + error taxonomy, mint hard-bind, keyset-free proof
extraction, NUT-07 state fetch, NUT-11 P2PK secret shape. Feature `wallet=[cashu,cdk]`
(no `cdk-sqlite` in core). Depends on nothing — proceeds even while PR #5 waits.

Acceptance:
- Builds `--features wallet` and default (default build has **no pay path** — the gate is
  the safety, SPIKE_LESSONS).
- The 4 spike hermetic tests green on `main`: `p2pk_secret_json_matches_nut11_shape`,
  `verify_accepts_amount_mint_lock_and_unspent_state`,
  `verify_rejects_wrong_lock_and_spent_state`,
  `cashu_proofs_from_v4_token_does_not_need_keyset_metadata`.
- Plus a **mint-binding test** (token mint ≠ the trade's expected mint → hard-fail).
  Layering note (Temper money-adv, 2026-07-14): this proves the equality *mechanism* only.
  The testnut *policy* gate — which mints a demo may ever expect — lives where
  `expected_mint` is chosen, and is an explicit acceptance line on pieces 6 and 8 below;
  an allowlist inside `wallet.rs` would smuggle policy into mechanism.
- Reviewer diffs extracted module against spike `wallet.rs` — zero intended behavioral
  divergence; `Cargo.lock` regenerated.

### piece-4 — token-delivery library · **MONEY**

Lift `mobee-core/src/delivery.rs` @ `0e77669`: payload canonical JSON, `TokenDelivery`
trait + memory fake, `NostrTokenDelivery` NIP-17, gift-wrap timestamp clamp. Depends on
piece-2 (core `gateway` feature / nostr dep).

Acceptance: the 3 spike hermetic tests green
(`token_delivery_payload_canonical_json_is_stable`, `memory_delivery_records_token_payloads`,
`gift_wrap_timestamp_tweak_stays_inside_relay_freshness_window`); trait consumable with the
memory fake and no network; review asserts proofs ride only the private DM path, never a
public receipt (spec §4).

### piece-5 — streamed result-content capture · **STANDARD**

Lift the `engine.rs`/`acp_driver.rs` deltas: AgentMessageChunk text capture → result
content (feeds `result_content_hash`, spec §5), audit logging, post-terminal drop. Depends
on nothing.

Acceptance: `agent_message_chunks_are_logged_for_audit`,
`post_terminal_updates_are_dropped`, `stream_without_terminal_appends_failed_and_returns_err`
green on `main`; existing engine/event-log suite not regressed.

### piece-6 — payment state machine + write-ahead journal · **MONEY** (design piece)

**Not an extraction.** New core module per SPIKE_LESSONS: explicit states
`intent → token minted/locked → delivered → receipt published → closed`; durable **pre-pay
intent (flock + fsync) before `pay_seller`**; stable idempotency key
`(job_id, result_id, content_hash, job_hash, seller_pubkey, amount, mint)`; explicit
paid-but-no-receipt recovery (republish, never second pay); injectable journal trait.
Spike's `authorize_pay` (`cli.rs:1844`) and append-after-pay journal are **reference
semantics only** — their shape is refuse-listed. Depends on pieces 1–4 (+5 for real content
hashes). This piece is the R2 precondition ("durable pre-pay intent") on the real-funds
path.

Acceptance (SPIKE_LESSONS merge gates, verbatim targets): stubbed pay-counter proves
`pay_seller` ≤ 1 across retry/crash/concurrent; idempotency suite (double request,
pay-ok/receipt-fail, journal restart, malformed → fail-closed); hash-bind failures reject
before pay; forged-receipt rejection (authority = author + signatures, not tags); empty
`relay_success` = failure; no-wallet build has no pay path.

Added from the 2026-07-14 money-adv pass (Temper) — the SM owns the policy/wire seams the
piece-3/4 mechanism libraries deliberately do not:
- **Testnut allowlist standing gate**: on the demo path, an `expected_mint` outside the
  test-mint set is a hard-fail *before* any verify call (the buyer-MCP triple gate's
  semantics, `cli.rs:1877-1920` at `0e77669`, become a core policy check + test here).
- **NUT-07 wire contract named**: who fetches proof state and when (caller-injected map vs
  composed fetch+verify) is an explicit, documented decision with a freshness bound —
  not an accident of signatures.
- **Duplicate-proof guard**: fixed at mechanism level in #8 (duplicate `y`/secret rejected
  before summing, regression in-PR — codex HIGH, 2026-07-14); the SM must not reintroduce
  unchecked aggregation.
- **Authenticity gate (MUST)**: the seller receive path SWAPS received proofs at the mint
  (or fully crypto-verifies them: retained `C` + keyset + DLEQ) before the payment state
  advances past *delivered*. `verify_p2pk_token` Ok is presented-proof checking only —
  lock/amount/mint/spend-state — NOT redeemability; swap-on-receive is the authenticity +
  exclusive-custody gate the mint itself enforces (codex HIGH-2; regression target:
  Temper's inflated-amount token built on a real unspent `y`).
- **Mint-URL comparison normalization**: define exact-match vs normalized (trailing slash,
  case, port) and test both sides of the decision.
- **Token-sum arithmetic is checked** (`checked_add`, no wrap) — the #8 in-PR fix carries
  the regression test; the SM must not reintroduce unchecked sums.
- **Canonical-form naming**: the delivery payload's "canonical JSON" is a module-local
  stable form, not RFC 8785 JCS — the SM's wire contract names it accurately.

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
(issue `9f9e9d0f…`), and the **targeting-seam alignment** (buyer-MCP posts untargeted;
seller gateway claims only targeted — STATE.md known issue; checkpoint (c) may settle it
earlier). Depends on all prior pieces.

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

**Sprint state (pieces 3/4/5).** PR #7 (piece-5) READY. PR #8 (piece-3) and PR #6
(piece-4): mechanical verification, adversarial pass (wrap overflow fixed via
`checked_add` + regression; cashu stack pinned `=0.17.2` to the reviewed surface;
gift-wrap plaintext-exclusion test added with non-vacuous control), and suite-verified
hold clears all green — awaiting the codex cold leg (coordinator's call) and the operator
gate. #6 remains stacked on #5 (retarget after #5 merges). Piece-5 inventory erratum
absorbed: the delta requires two 2-line enum variants (`driver/acp.rs`, `event.rs`) beyond
the row's named files — inseparable, documented in PR #7.
