# Mobee State

Last updated: 2026-07-13

## Current phase

v0.1 on main (`e066a50`) = ACP **execution spine**. Strategy: merge
remaining product to main **one piece at a time**. Architecture
decisions locked + filed as buzz issues (honest sync, nix installables,
`execution_id` rename). Money-path on `spike/full-loop` is past
idempotency gate; metadex on M4–M6 demo-integrity fast-follow.

## Active lanes

| Lane | Owner | Status | Notes |
|------|-------|--------|-------|
| `spike/full-loop` | metadex | ACTIVE | Money-path: Sting PASS on idempotency @ `a2dfa51`; hearth dual-review CONDITIONAL PASS; M4–M6 fast-follow in flight. Do not collide. |
| `spike/headless-buyer` @ 8c73982 | (candidate) | Ready to cherry-pick/rebuild | State machine Settled PROVEN; payment + live relay UNVERIFIED |
| Seller gateway (turtle) | keeper:mobee-orchestrator | Restarting / demo path | Align targeting seam before live e2e |
| Meta seat (`mobee-meta`) | this IDE agent | On buzz | Docs PR #1 open; arch issues filed |
| `docs/meta-genesis` | mobee-meta | PR open | https://github.com/MakePrisms/mobee/pull/1 |

## Reality ledger (edges)

| Edge | Class | Evidence |
|------|-------|----------|
| State machine + co-sign to Settled (in-memory Bus, payment mocked) | PROVEN | headless full-loop → `final state = settled` |
| Live relay-mode headless (`MOBEE_HEADLESS_RELAY` / RelayBus) | BUILT-BUT-UNVERIFIED | Compile-only / unproven live |
| Payment leg (NUT-11 / NUT-07) | BUILT-BUT-UNVERIFIED | Not exercised in headless; Settled ≠ money moved |
| Open market relay anon write/read (5109/6109/7000/3400) | PROVEN | `wss://mobee-relay.orveth.dev` |
| Real ACP on turtle + Mac (v0.1) | PROVEN | Merged 2026-07-12 |
| Testnut money-path on spike (static token) | CONDITIONAL | Dual-review PASS for demo; R1–R3 before real funds |

## Open architecture (buzz issues — not claimed for impl yet)

| Topic | Issue event id | Stance |
|-------|----------------|--------|
| Honest sync (drop faux-async / `block_on`) | `77c5ae79cb2e223bac1ec1a007d54eb79dd6a718c5ffbe6f1fb13115f9bad54e` | Locked A |
| Nix: buyer MCP + seller gateway + harnesses + published binaries | `6d40cd87d4b57232719649a67bc797485090b5f3d7c7528b253e6796bf3b5282` | Locked |
| Rename spine `job_id` → `execution_id` | `9f9e9d0fe25c3054d25b93ddfde7f0504e1890249b5c991e843300a6c42a3e26` | Locked |
| Test posture + iterate-as-we-merge rule | `eb4290e7bea57638e531ef1b457f53949e60331ac863d2b0f425cbbff45e2728` | Locked (policy); follow-ups unclaimed |

Repo: NIP-34 `mobee` / owner `79284e2b167317bc455f2daccfb38c38d4836b7b2bd0d73650b0cff46660263a`.

## Known issues (pre-existing — do not chase as new regressions)

- `mobee-evals` `scenarios_pass_deterministic_graders` FAILS at older spike HEADs:
  `$.log_payloads[3].data.status` expected `"failed"`/`"completed"`,
  actual `null` — deterministic; predates recent money-path work.
- Integration seam (open): buyer-MCP posts **UNTARGETED** offers; seller
  gateway only claims **TARGETED** — must align before live e2e closes.

## Blocked / waiting

- Live e2e close blocked on targeting seam + gateway up
- Real-funds chapter blocked on R1–R3 (token value/P2PK, durable pre-pay intent, targeted-seller enforce) — tracked by hearth; not demo blockers
- Await gudnuf on GitHub PR #1 (`docs/meta`)

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

1. Await gudnuf review/merge of https://github.com/MakePrisms/mobee/pull/1
2. Inventory spike-vs-main → draft ordered merge-piece sequence (incl. when to schedule execution_id rename / honest-sync / nix packages relative to marketplace merges)
3. Hold impl claims on arch issues until operator assigns; do not collide with metadex M4–M6

## Genesis

**Closed 2026-07-13.** Q1–Q4 recorded in PROCESS.md Decisions.

## Recent completions

- 2026-07-12: v0.1 dual-reviewed, merged to main
- 2026-07-13: genesis closed; buzz announce; docs PR #1
- 2026-07-13: main tour + unclean-cut review with operator
- 2026-07-13: locked A/nix/execution_id; filed 3 buzz issues; docs/meta sync
- 2026-07-13: test posture feedback filed; standing iterate-tests rule in PROCESS
