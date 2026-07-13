# How Mobee Gets Built

## What this is

Mobee is an agent-hiring marketplace (installable Rust app). A buyer
hires a seller agent for a job. Specs are locked; build to spec;
spec-vs-code drift is a finding to report, not to patch around.

Specs: https://forgefleet.dev/mobee/spec/ (+ in-repo copies).

## Repos

| Repo | What it is | Rule |
|------|------------|------|
| **mobee** (this repo / mobee-dev) | Product, real-wire | Source of truth for shipping |
| **mobee-core** | Hermetic Phase-0 reference | Reference only — no shared types with product |

Do not conflate the two codebases.

## Roles

| Role | Who | Responsibility |
|------|-----|----------------|
| **Operator** | Human | Priorities, taste, final say; **drives** the merge sequence |
| **Meta-agent** | This seat (`mobee-meta`) | Drafts piece sequence, claims non-colliding work on buzz after announce, drafts worker prompts, tracks STATE. Never product code. Operator input required before claim/execute. |
| **Lane owner** | `keeper:mobee-orchestrator` | Seller side / gateway; coordination counterpart on buzz |
| **Buyer-MCP** | metadex | Buyer MCP on Mac; owns `spike/full-loop` |
| **Infra** | infraguy | Relay / box deploys |
| **Reviewer** | gudnuf | Reviews all PRs to main |
| **Work agents** | Spawned as needed | Narrow scope, acceptance criteria, no collision with active spikes |

## Reality classes

Every verification claim must be labeled:

- **PROVEN** — ran against the real path named; evidence cited
- **BUILT-BUT-UNVERIFIED** — code exists; named edge not exercised

Known unproven edges (as of 2026-07-13): payment leg (NUT-11/NUT-07),
live relay-mode headless (`MOBEE_HEADLESS_RELAY`).

"Settled" in a mocked-payment / in-memory-bus run proves the state
machine + co-signing path only — not money movement.

## Process

1. Announce + claim work on buzz channel `dd4821c9` **before** building.
2. Every change → PR to `main`. gudnuf reviews. No self-merges.
3. Do not collide with active spikes (see STATE.md).
4. Spec drift → report as finding; do not paper over.
5. Persist significant outputs to disk; update `docs/meta/STATE.md` at
   session boundaries.

## Meta-state location

`docs/meta/` in this repo (PROCESS.md, STATE.md). Product source stays
elsewhere in the tree; meta tracks how we build, not what we ship in
behavior.

## Coordination

- Cross-machine: buzz @ `wss://buzzrelay.orveth.dev`, channel `dd4821c9`
- Open market relay (demo home): `wss://mobee-relay.orveth.dev`
- Team relay: `wss://buzzrelay.orveth.dev` (members only)
- Meta identity: `~/.config/buzz/mobee-meta.key` (pubkey in STATE.md)
- Identify senders by pubkey, not display name
  (`keeper:mobee-orchestrator` posts as `c260cc43…`, shared with `keeper:hearth`)

## Decisions

| Date | Decision |
|------|----------|
| 2026-07-13 | Meta seat uses fresh buzz key `mobee-meta` (Q1=B). Admitted + announced. |
| 2026-07-13 | Meta-state lives in `docs/meta/` inside the product repo (Q2). |
| 2026-07-13 | Priority = merge full product to main one piece at a time (Q3=D). Spikes are input to that merge sequence, not destinations. |
| 2026-07-13 | Claims: meta drafts sequence + claims non-colliding pieces after buzz announce; **operator drives** (input + final say) (Q4=A). |

## Conventions

- Prefer small, reviewable PRs off `main`
- Cherry-pick / rebuild spikes rather than stacking on foreign spikes
- Label PROVEN vs BUILT-BUT-UNVERIFIED in every status report
- Pre-existing known issues (see STATE.md) are not treated as regressions
  of new work unless newly introduced
