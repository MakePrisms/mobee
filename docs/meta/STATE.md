# Mobee State

Last updated: 2026-07-14

## Current phase

Rebuild track live on MakePrisms/mobee `main`. The full product is landing **one reviewed
piece at a time**, distilled from the spike (`spike/full-loop @ 0e77669`) — the spike is
source material, not a destination. The seam map ([REBUILD-SEAM.md](REBUILD-SEAM.md)) is the
working plan.

Merged to main: piece-1 (format + receipt, `b5003d4`, PR #2), piece-2 (gateway protocol
types, PR #5, `46499b5`), piece-5 (capture, PR #7, `91adf41`), **piece-3 (CDK-first trade
verification, PR #8, squash `dee436e`)**, and **piece-4 (payment-send, PR #6, squash
`cec8607`)**. Foreground now: **piece-6 payment SM** (design = source of truth
[PIECE-6-PAYMENT-SM.md](PIECE-6-PAYMENT-SM.md); Q1–Q6 + `attempt_id`/reconcile locked
2026-07-14). **PR1 (core hermetic, double-pay/WAL closure) MERGED @ `b741eaf` (PR #10)** —
three-legged MONEY bar (composition + Temper adversarial/durability + codex deep found 3 HIGH
crash-durability, fixed & verified). **PR2 (edge authenticity)** = next money cut: real
buyer-mint `lock_or_reconcile` + seller receive/swap gate + NUT-07 connector + Temper R1/R2
binds. #6a Debug-redact (PR #11) **MERGED** (composition CLEAR + gudnuf).
(Observatory v1.1 = idle-only background per operator.)
Pieces 6–8 are specced, not yet built (piece-6 has a design doc:
[PIECE-6-PAYMENT-SM.md](PIECE-6-PAYMENT-SM.md)).

Two live spikes ran tonight on the real relay (spike-track, reality class PLAY):
**checkpoint (c)** proved the full git-delivery money loop single-key; **checkpoint (c2)**
proved it **arms-length** (distinct keys, distinct harnesses) — see [RUNS-C2.md](RUNS-C2.md).
The **usage-awareness** primitive is mid-spike (cross-harness measurement matrix, 2 of 3
legs — [USAGE-MATRIX-CPB.md](USAGE-MATRIX-CPB.md)). Marketplace scope is settled by gudnuf
(NIP-89 self-announce, open projects, simple timeouts, price-only offers); fair-exchange is
resolved v1-plaintext with the real-price exposure tracked as a deferred problem.

## Active lanes

| Lane | Owner | Status | Notes |
|------|-------|--------|-------|
| Rebuild pieces → main | forge team (Scribe/Anvil/Temper/metadex) | merged: #2 #5 #6 #7 #8 #10 (piece-6 PR1 core `b741eaf`); **PR2 (piece-6 edge authenticity) = next money cut**; #6a Debug-redact = PR #11 merged | each PR: composition + Temper adversarial + codex deep; piece-6 design = PIECE-6-PAYMENT-SM.md |
| Usage-awareness matrix (checkpoint b) | Scribe (compose) + Anvil/Temper (legs) | 2/3 legs (codex ACP-native, cursor ACP-dark); claude leg pending seat pick | transport is harness-dependent — the headline finding |
| Journal-v2 (live-stream) | Scribe (scoped) | design delivered; awaiting gudnuf's exposure pick | + latent finding: v1 journal already live+near-raw |
| Skills/practice accessibility pass | Scribe | inventory done; composition behind checkpoint-b | founding gap: non-Claude kit = instructions.md only |
| Spike `spike/full-loop @ 0e77669` | metadex | source material | distilled piece-by-piece to main |

## Reality ledger (edges)

| Edge | Class | Evidence |
|------|-------|----------|
| Format + receipt hash contract on main | PROVEN | PR #2 @ `b5003d4`; hermetic tests |
| Gateway protocol types on main | PROVEN | PR #5 merged `46499b5`; 29/29 both feature sets |
| CDK-first trade verification (`verify_trade_p2pk`) on main | PROVEN | PR #8 merged `dee436e`; wallet core 38/38, default 30/30; mint/amount/unit/per-proof-P2PK-seller-lock/NUT-07-unspent; not mint authenticity |
| Payment-send (typed `PaymentPayload` / `PaymentSend`) on main | PROVEN | PR #6 merged `cec8607`; typed `cashu::Token` payload, string only at NIP-17 envelope (parse-first `TryFrom`), gift-wrap, fail-closed on empty `relay_success`; finding-10 subsumed |
| Payment SM: double-pay closure + WAL crash-safety (hermetic) on main | PROVEN | PR #10 merged `b741eaf`; pay-once across retry/crash/concurrent via `attempt_id`/reconcile, write-ahead journal (fsync + newline-commit-marker + parent-dir fsync + replay-sync), recovered-Locked refuse; 3-legged bar. NOT authenticity / live-mint (PR2) |
| Arms-length git-delivery trade (2 keys, 2 harnesses, testnut) | PROVEN (PLAY) | checkpoint (c2) — [RUNS-C2.md](RUNS-C2.md); 4 independent verify layers |
| Single-key git-delivery money loop (testnut) | PROVEN (PLAY) | checkpoint (c) |
| Usage transport uniform at the ACP boundary | REFUTED | codex ACP-native, cursor ACP-dark — [USAGE-MATRIX-CPB.md](USAGE-MATRIX-CPB.md) |
| Payment leg (NUT-11 / NUT-07) on real relay | PROVEN (PLAY) | c/c2 testnut settlement + co-signed receipts |
| Open market relay anon write/read (5109/6109/7000/3400) | PROVEN | `wss://buzzrelay.orveth.dev` |
| Real ACP on turtle + Mac (v0.1) | PROVEN | merged 2026-07-12 |
| Proof authenticity at real value | UNPROVEN — deferred | REBUILD-SEAM DP-2 + finding 8; piece-6 swap gate + spec §4 |
| Fair exchange at real prices | UNPROVEN — deferred | REBUILD-SEAM DP-1; v1 keeps plaintext fetch-before-pay |

## docs/meta index (all current as of 2026-07-14)

- [REBUILD-SEAM.md](REBUILD-SEAM.md) — the rebuild plan: inventory, ordered pieces 3–8 with
  acceptance + review class, refuse-to-copy, spec drift, findings, deferred problems.
- [RUNS-C2.md](RUNS-C2.md) — the arms-length reference run (full event chain + verify layers).
- [USAGE-MATRIX-CPB.md](USAGE-MATRIX-CPB.md) — cross-harness usage measurement (checkpoint b).
- [PIECE-6-PAYMENT-SM.md](PIECE-6-PAYMENT-SM.md) — payment state-machine + write-ahead design.
- [SPIKE_LESSONS.md](SPIKE_LESSONS.md) — rebuild constraints + refuse-to-copy list.
- [PROCESS.md](PROCESS.md) — merge train, review authority, no self-merge.
- [GOOSE.md](GOOSE.md) — Goose-embed research (harness-only).

## Open architecture (locked; landing via rebuild pieces)

Locked A / honest-sync (drop faux-async `block_on`) → piece-8. Nix boring targets → piece-8.
`job_id` → `execution_id` spine rename → piece-8. Test posture + iterate-as-we-merge →
standing (PROCESS.md). Buzz-issue refile still held pending a canonical issue home.

## Deferred problems (tracked, not solved)

See [REBUILD-SEAM.md](REBUILD-SEAM.md) § Deferred problems. **DP-1 fair-exchange** (v1
plaintext fetch-before-pay; real exposure at real prices; fix = NIP-17 key-rail encrypted
delivery + PoPs escrow; envelope slot reserved). **DP-2 proof-authenticity at real value**
(piece-6 swap-on-receive gate + spec §4 fifth check). Plus real-funds R1–R3.

## Blocked / waiting

- Usage matrix finalize: claude leg pending a coordination seat pick (claude-agent-acp).
- piece-6 PR2 (edge authenticity): Anvil builder off `b741eaf` to the folded
  PIECE-6-PAYMENT-SM.md § piece-6 → 4-leg bar (Temper primary adv + metadex second-adv + codex
  deep + my composition) → COMPOSED-DONE → gudnuf merge. Scope: real cdk `lock_or_reconcile` +
  R1 (Token≡terms before Locked) + seller swap gate + R2 (`verify_trade_p2pk` on locked.token)
  + NIP-17 send discipline + one edge unit-constructor.
- #6a Debug-redact (PR #11): composition CLEAR, awaits hearth mechanical + gudnuf merge.
- Codex-leg checkpoint settlement: buyer-side token-binding bug (delivered token unparseable);
  fix = pre-publish token-integrity guard (REBUILD-SEAM finding 10 / piece-6 gate); unclaimed.
- Journal-v2: gudnuf's exposure-level pick + the v1-journal live-leak remediation.

## Meta / team

Genesis (mobee-meta, `mobee-meta` IDE seat) closed 2026-07-13 — decisions in
[PROCESS.md](PROCESS.md). The turtle-resident forge builder team now runs the rebuild:
**Scribe** (composition owner, Fable keeper), **Anvil** (builder, codex-acp / gpt-5.6-sol),
**Temper** (red-team, cursor / grok-4.5). Coordination counterpart: keeper:mobee-orchestrator.
gudnuf reviews all PRs; no persona self-merges.

## Recent completions

- 2026-07-12: v0.1 dual-reviewed, merged to main.
- 2026-07-13: genesis closed; piece-1 (format+receipt) merged (PR #2); consolidation to one
  canonical repo; money-path dual-review; SPIKE_LESSONS captured.
- 2026-07-14: checkpoint (c) + (c2) live trades (arms-length proven); rebuild seam map +
  run record + payment-SM design merged; pieces 3/4/5 through the money bar (codex/Temper/
  composition gauntlet); piece-2 (PR #5) merged; usage-awareness schema locked + 2/3
  measurement legs; marketplace scope + fair-exchange settled by gudnuf.
- 2026-07-14 (late): piece-3 reworked CDK-first (delete hand-rolled cashu mirrors, keep
  `verify_trade_p2pk` trade policy) and **MERGED to main** on PR #8 (squash `dee436e`) — four
  fix-window items verified on behavior-CLEAR head `5c596a69` (strict per-proof seller-lock,
  currency-unit bind, rustdoc trust-notch, no-DB dep-graph; Temper CLEAR + Anvil rev-parse +
  gh confirm), then a docs-only comment-trim hygiene pass (`172cdeda`, verified comment-only,
  trap knowledge moved to tests) merged by gudnuf. `verify_trade_p2pk` + typed `CurrencyUnit`
  `TradeLock` now main API surface (offer-unit → `TradeLock.unit` is the #6 residual).
- 2026-07-14 (late): piece-4 (payment-send) **MERGED** on PR #6 (squash `cec8607`, gudnuf
  rebased onto #8 + merged direct from IDE) — typed `cashu::Token` payload, finding-10
  subsumed. piece-6 debate **LOCKED** (Q1–Q6 + `attempt_id`/reconcile money invariant); design
  folded to source of truth PIECE-6-PAYMENT-SM.md; PR1 (core hermetic, double-pay) = Anvil off
  pinned `cec8607`, PR2 (edge authenticity) follows.
- 2026-07-14 (late): **piece-6 PR1 (double-pay/WAL core, hermetic) MERGED @ `b741eaf` (PR #10)**
  — 3-leg MONEY bar (my composition + Temper adversarial/durability + codex deep, which
  found+fixed 3 HIGH crash-durability: post-replay fsync, newline-commit-marker, parent-dir
  fsync); landed artifact verified byte-identical to reviewed head `2efb1e47`. **PR2 (edge
  authenticity) chartered** (real cdk reconcile + seller swap gate + R1/R2 binds; 4-leg bar).
  #6a Debug-redact (PR #11) composition CLEAR, in gudnuf's merge queue.
