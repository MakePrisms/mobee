# Usage-awareness cross-harness matrix — checkpoint (b)

The measurement track's primary deliverable: does the checkpoint-(a) usage schema normalize
across harness families? Composed by Scribe from the per-leg measurement samples. Spike-track,
reality class **PARTIAL** — 2 of 3 legs in (codex, cursor); **claude leg pending a seat
pick**. The headline finding below is **conclusive at 2 legs** and the third leg refines,
not overturns, it.

Locked schema: checkpoint (a) — `total_tokens = input + output + reasoning`; cache/tool are
sibling evidence, **not** summed into total; `measured_cost = rate_table(usage)` ≠ raw total;
`remaining` is seller-local. Per-leg artifacts: `RESEARCH/CODEX_USAGE_MEASURE_CPB.md`,
`RESEARCH/CURSOR_USAGE_MEASURE_CPB.md`.

## ★ Headline finding — usage transport is HARNESS-DEPENDENT

gudnuf's design intent is usage "reported **at the ACP boundary** so it translates to anyone
that can use ACP." The matrix shows that premise does **not** hold uniformly:

- **codex (codex-acp-ng / gpt-5.6-sol): ACP-NATIVE.** `session/prompt result.usage` carries
  the full usage object on the JSON-RPC wire, plus a native `usage_update`. A transparent tee
  of the ACP stdout captures it — no side channel needed. Conclusive (Anvil read the live
  wire, not a CLI surface).
- **cursor (cursor-agent / grok-4.5): ACP-DARK.** The live ACP path emits `{stopReason:
  end_turn}` with **zero** usage; the numbers exist only on the `--print --output-format
  stream-json` CLI **sibling** surface, off the ACP path entirely.

**Consequence for the primitive:** it cannot be "read usage off the ACP wire uniformly." It
must be **"each harness measures on its native surface and normalizes at the boundary,"** and
the adjunct therefore **requires** the `usage_transport: acp-native | side-channel` field
(metadex added it — codex reports `acp-native`; cursor is `side-channel`). This is the
matrix's reason for existing: the divergence the ACP-boundary premise assumed away is real,
named, and now carried in the schema.

## The legs (normalized per the locked total rule)

| | codex (ACP-native) | cursor (side-channel) | claude |
|---|---|---|---|
| native total (as emitted) | `totalTokens 13156` (rolled up, **includes** cache read) | `13693` (in+out only) | pending |
| input | 3162 | 13346 | pending |
| output | 10 | 347 | pending |
| reasoning | 0 (exposed) | null (unavailable) | pending |
| cache_read | 9984 | 41088 | pending |
| cache_write | null | 0 | pending |
| tool | null | null | pending |
| **normalized total (in+out+reasoning)** | **3172** | **13693** | pending |
| transport | acp-native | side-channel (print-mode) | pending |

**The normalizer works.** Both legs mapped a different native shape onto the same locked
rule: codex's native `13156` is a rolled-up figure that folds cache read in (3162+9984+10);
stripped to the rule it is `3172`. cursor's native `13693` is already in+out. Neither
double-counts cache into the normalized total. So `total_tokens` **is** a consistent
cross-harness unit once the rule is applied — the value of the rule is precisely that it
survives two very different native surfaces.

## Divergences (what each harness CAN report — the real cross-harness gap)

- **reasoning_tokens:** codex exposes it (zero this sample — so nonzero output/reasoning
  overlap semantics remain unproven); cursor has no numeric field at all (thinking stream
  exists, uncounted). → the rule's `+reasoning` term is populated on codex, structurally
  absent on cursor.
- **cache split:** codex gives read, no write; cursor gives read + write(0). Sibling fields,
  correctly excluded from total both sides.
- **tool_tokens:** unavailable on both (tool_call events observed, not counted).
- **rate-table readiness (for `measured_cost`):** codex exposes model + noncached-input +
  cache-read + output + reasoning; cursor exposes model + cache-read + cache-write. **Neither
  exposes tool tokens or pricing coefficients.** Both used a provisional identity cost
  (`measured_cost = normalized total`) pending a locked rate table — honest placeholder,
  named.

## Claude leg — the open input

Pending the coordinator's seat pick (raised 17:33). Two candidate surfaces, and which one is
run matters for the matrix:
- **claude-agent-acp (Librarian-style):** the true apples-to-apples ACP third leg — answers
  "is claude ACP-native like codex, or ACP-dark like cursor?" This is the coordinator's to
  arrange (Librarian's runtime), not Scribe's to conscript.
- **keeper-class (subagent token accounting):** a non-ACP fallback surface (per-subagent
  `*.jsonl` usage). A valid data point, but it measures a *different* surface than the two
  ACP legs, so it can't settle the ACP-transport question for the claude family.

Recommendation: run **claude-agent-acp** as the third leg to close the transport axis
cleanly; keeper-class is the fallback if that seat can't be stood up.

## What checkpoint (b) has proven so far

1. The locked total rule normalizes correctly across two structurally different native
   surfaces (conclusive).
2. Usage transport is **not** uniform at the ACP boundary — codex-native, cursor-dark — so
   the primitive is measure-natively-then-normalize, and `usage_transport` is a required
   schema field (conclusive at 2 legs; claude tells us which side it's on).
3. `measured_cost` is not yet real anywhere — no harness exposes a full rate table; identity
   placeholder is the honest v0, and cost-vs-price stays uninstantiated until a rate table
   locks.
