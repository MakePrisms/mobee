# Mobee State

Last updated: 2026-07-13

## Current phase

v0.1 spine on MakePrisms/mobee. **Repo consolidation in flight**
(orchestrator lane): GitHub MakePrisms/mobee → single code SoT;
Librarian owns buzz relay-git mirror; wipe mobee-dev/mobee-core only after
sha-verify. Money-path M4–M6 PASS @ `f3beb95` on metadex spike; real-funds
R1–R3 still tracked.

## Active lanes

| Lane | Owner | Status | Notes |
|------|-------|--------|-------|
| `spike/full-loop` @ `f3beb95` | metadex | ACTIVE — money-path demo integrity PASS | Idempotency + M4–M6 passed Sting; dual-review = **orchestrator** (not hearth — shared-key artifact). Push to mobee-dev; canonical origin channel-binding issue. |
| `spike/headless-buyer` @ 8c73982 | orchestrator holding | Unmerged (gudnuf cherry-pick) | Source for pieces; MOCKED payment |
| Repo consolidation | keeper:mobee-orchestrator | In sequence | After money-path push → migrate spikes w/ history → port hermetic suite → Librarian mirror → wipe |
| Seller gateway (turtle) | keeper:mobee-orchestrator | Verify rig | Ping before claiming pieces (gateway-helper overlap) |
| Meta seat (`mobee-meta`) | this IDE agent | On buzz | Docs PR #1; 4 buzz issues — **do not refile** until canonical issue home locked |
| `docs/meta-genesis` | mobee-meta | PR open | https://github.com/MakePrisms/mobee/pull/1 → MakePrisms/mobee main |
## Reality ledger (edges)

| Edge | Class | Evidence |
|------|-------|----------|
| State machine + co-sign to Settled (in-memory Bus, payment mocked) | PROVEN | headless full-loop → `final state = settled` |
| Live relay-mode headless (`MOBEE_HEADLESS_RELAY` / RelayBus) | BUILT-BUT-UNVERIFIED | Compile-only / unproven live |
| Payment leg (NUT-11 / NUT-07) | BUILT-BUT-UNVERIFIED | Not exercised in headless; Settled ≠ money moved |
| Open market relay anon write/read (5109/6109/7000/3400) | PROVEN | `wss://mobee-relay.orveth.dev` |
| Real ACP on turtle + Mac (v0.1) | PROVEN | Merged 2026-07-12 |
| Testnut money-path on spike (static token) | CONDITIONAL | Dual-review PASS for demo; R1–R3 before real funds |

## Open architecture (buzz issues — hold refile)

Filed on relay-git NIP-34 `mobee` (owner `79284e2b…`). Orchestrator:
**do not refile** until canonical issue home locked post-consolidation.

| Topic | Issue event id | Stance |
|-------|----------------|--------|
| Honest sync (drop faux-async / `block_on`) | `77c5ae79cb2e223bac1ec1a007d54eb79dd6a718c5ffbe6f1fb13115f9bad54e` | Locked A |
| Nix: buyer MCP + seller gateway + harnesses + published binaries | `6d40cd87d4b57232719649a67bc797485090b5f3d7c7528b253e6796bf3b5282` | Locked |
| Rename spine `job_id` → `execution_id` | `9f9e9d0fe25c3054d25b93ddfde7f0504e1890249b5c991e843300a6c42a3e26` | Locked |
| Test posture + iterate-as-we-merge rule | `eb4290e7bea57638e531ef1b457f53949e60331ac863d2b0f425cbbff45e2728` | Locked (policy); follow-ups unclaimed |

## Known issues (pre-existing — do not chase as new regressions)

- `mobee-evals` `scenarios_pass_deterministic_graders` FAILS at older spike HEADs:
  `$.log_payloads[3].data.status` expected `"failed"`/`"completed"`,
  actual `null` — deterministic; predates recent money-path work.
- Integration seam (open): buyer-MCP posts **UNTARGETED** offers; seller
  gateway only claims **TARGETED** — must align before live e2e closes.

## Blocked / waiting

- Live e2e close blocked on targeting seam + gateway up
- Real-funds chapter: R1–R3 (token value/P2PK, durable pre-pay intent, targeted-seller enforce)
- Canonical issue home unsettled during repo consolidation — hold refile
- Await gudnuf on GitHub PR #1 (`docs/meta`)
- Await orchestrator ping before claiming merge pieces

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

1. **Merge train (meta drives, team builds):** await builder claim for piece 1
   (format+receipt) — brief on buzz `06212e2d…`; PR must tell clear story
2. Operator/gudnuf: review https://github.com/MakePrisms/mobee/pull/1 (docs/meta + SPIKE_LESSONS)
3. After piece 1 claimed: draft piece 2 PR story (gateway protocol types) on buzz
4. Do **not** implement product code from mobee-meta seat

## Genesis

**Closed 2026-07-13.** Q1–Q4 recorded in PROCESS.md Decisions.

## Recent completions

- 2026-07-12: v0.1 dual-reviewed, merged to main
- 2026-07-13: genesis closed; buzz announce; docs PR #1
- 2026-07-13: main tour + unclean-cut review with operator
- 2026-07-13: locked A/nix/execution_id; filed 3 buzz issues; docs/meta sync
- 2026-07-13: test posture feedback filed; standing iterate-tests rule in PROCESS
- 2026-07-13: spike lessons from metadex + orchestrator → docs/meta/SPIKE_LESSONS.md
