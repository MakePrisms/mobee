# Mobee State

Last updated: 2026-07-13

## Current phase

Piece 1 on main (`b5003d4`, PR #2). Merge-train **piece 2** (gateway
protocol types) briefed on buzz — awaiting builder assignment.
Money-path cherry-pick waits on piece 2.

## Active lanes

| Lane | Owner | Status | Notes |
|------|-------|--------|-------|
| Piece 1 format+receipt | metadex | MERGED PR #2 | main @ `b5003d4` |
| Piece 2 gateway types | (unassigned) | Briefed | buzz `c7b820df…`; awaiting orchestrator assign |
| `spike/full-loop` @ `f3beb95` | metadex / gudnuf | Money-path CLEAR | Cherry-pick after piece 2 |
| Repo consolidation | orchestrator + Librarian | Mirror verified | Step-5 wipe held |
| Meta seat (`mobee-meta`) | this IDE agent | Driving | No product impl |
| docs/meta | merged | Done | PR #1 @ `4b8e29b` |

## Reality ledger (edges)

| Edge | Class | Evidence |
|------|-------|----------|
| Format + receipt hash contract on main | PROVEN | PR #2; hermetic tests |
| State machine + co-sign to Settled (in-memory Bus, payment mocked) | PROVEN | headless full-loop |
| Live relay-mode headless | BUILT-BUT-UNVERIFIED | Compile-only |
| Payment leg (NUT-11 / NUT-07) | BUILT-BUT-UNVERIFIED | Not on main money path yet |
| Open market relay anon write/read (5109/6109/7000/3400) | PROVEN | `wss://mobee-relay.orveth.dev` |
| Real ACP on turtle + Mac (v0.1) | PROVEN | Merged 2026-07-12 |
| Testnut money-path on spike | CONDITIONAL | Dual-review PASS; R1–R3 before real funds |

## Open architecture (buzz issues — hold refile)

| Topic | Issue event id | Stance |
|-------|----------------|--------|
| Honest sync | `77c5ae79…` | Locked A |
| Nix installables | `6d40cd87…` | Locked |
| `execution_id` rename | `9f9e9d0f…` | Locked |
| Test posture | `eb4290e7…` | Locked (policy) |

## Known issues (pre-existing)

- Eval flake on older spike HEADs — re-verify on main as needed
- Targeting seam: untargeted offers vs targeted claims — piece 2 must encode invariant

## Blocked / waiting

- Piece 2 builder unassigned
- Money-path cherry-pick blocked on piece 2
- Canonical buzz issue home unsettled — hold refile

## Meta identity

| Field | Value |
|-------|-------|
| Key file | `~/.config/buzz/mobee-meta.key` |
| Hex pubkey | `fe2ec5a8493b9484ad30d2e95115134d6e81e5cfe265f32f61a2ece5a6a2c1de` |
| Channel | `dd4821c9-c6dc-429f-8e0f-51fabb695c20` |

## Strategy (locked)

Merge to `main` one piece at a time. Meta drives; team builds. Spikes are
source material, not destinations.

## Next actions

1. **Piece 2:** await builder claim (gateway protocol types)
2. After piece 2: money-path cherry-pick / thin CLI (gudnuf + orchestrator)
3. Fold Sting/Goose lock into docs/meta (meta PR)
4. Close stale open docs PRs (#3/#4) once STATE is current

## Genesis

**Closed 2026-07-13.**

## Recent completions

- 2026-07-13: **PR #2 merged** — format+receipt @ `b5003d4`
- 2026-07-13: **PR #1 merged** — docs/meta @ `4b8e29b`
- 2026-07-13: piece 2 brief posted on buzz
