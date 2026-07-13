# Mobee State

Last updated: 2026-07-13

## Current phase

v0.1 on main (`e066a50`). Strategy: merge remaining product to main
**one piece at a time**. Buyer-MCP spike active (do not collide);
headless e2e is a candidate piece; live e2e still blocked on targeting
seam + gateway.

## Active lanes

| Lane | Owner | Status | Notes |
|------|-------|--------|-------|
| `spike/full-loop` @ d160c75 + WIP | metadex | ACTIVE — money-path gate BLOCKED | Idempotency fix in flight (Sting). Do not collide. |
| `spike/headless-buyer` @ 8c73982 | (candidate) | Ready to cherry-pick/rebuild | Based on 556530c. Full loop → Settled hermetically via in-memory Bus. |
| Seller gateway (turtle) | keeper:mobee-orchestrator | Restarting post-pause | Demo path depends on this |
| Meta seat (`mobee-meta`) | this IDE agent | On buzz | Profile set; announced on `dd4821c9…` |

## Reality ledger (edges)

| Edge | Class | Evidence |
|------|-------|----------|
| State machine + co-sign to Settled (in-memory Bus, payment mocked) | PROVEN | headless full-loop → `final state = settled`; tests green |
| Live relay-mode headless (`MOBEE_HEADLESS_RELAY` / RelayBus) | BUILT-BUT-UNVERIFIED | Compile-only / unproven live |
| Payment leg (NUT-11 / NUT-07) | BUILT-BUT-UNVERIFIED | Not exercised in headless; Settled ≠ money moved |
| Open market relay anon write/read (5109/6109/7000/3400) | PROVEN | `wss://mobee-relay.orveth.dev` |
| Real ACP on turtle + Mac (v0.1) | PROVEN | Merged 2026-07-12 |

## Known issues (pre-existing — do not chase as new regressions)

- `mobee-evals` `scenarios_pass_deterministic_graders` FAILS at 556530c:
  `$.log_payloads[3].data.status` expected `"failed"`/`"completed"`,
  actual `null` — all 3 scenarios, deterministic. Predates both spikes.
- Integration seam (open): buyer-MCP posts **UNTARGETED** offers; seller
  gateway only claims **TARGETED** — must align before live e2e closes.

## Blocked / waiting

- Live e2e close blocked on targeting seam + gateway up
- Money-path gate on `spike/full-loop`: **BLOCKED** on `authorize_pay` idempotency (pay-ok / receipt-fail → double-spend). metadex fixing → Sting re-review → hearth dual-review.

## Meta identity

| Field | Value |
|-------|-------|
| Key file | `~/.config/buzz/mobee-meta.key` |
| Hex pubkey | `fe2ec5a8493b9484ad30d2e95115134d6e81e5cfe265f32f61a2ece5a6a2c1de` |
| npub | `npub1lchvt2zf8w2gftfs6t54z9gnf4hgrew0ufjlxtmp5tkwtf4zc80q2dj77u` |
| Membership | admitted 2026-07-13 |
| Channel | `dd4821c9-c6dc-429f-8e0f-51fabb695c20` (`mobee`) |
| Announce event | `f43596ea9c6c502376eb44f27f1f5f6d354b622a6626e7cd94445e9c4d95f865` |

## Strategy (locked)

Merge the full product to `main` **one piece at a time**. Spikes
(`full-loop`, `headless-buyer`, etc.) are source material for that
sequence — not long-lived destinations. Each PR: small, reviewable,
reality-classed, gudnuf reviews, no self-merge.

## Next actions

1. Await gudnuf review/merge of https://github.com/MakePrisms/mobee/pull/1 (`docs/meta-genesis`)
2. Inventory spike-vs-main → draft ordered merge-piece sequence for operator review
3. Hold product claims until money-path gate clears or operator names a non-colliding piece

## Genesis

**Closed 2026-07-13.** Q1–Q4 recorded in PROCESS.md Decisions.

## Recent completions

- 2026-07-12: v0.1 dual-reviewed, merged to main
- 2026-07-13: headless e2e verified (state machine path); genesis closed; buzz announce
