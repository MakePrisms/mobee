---
name: run-seller
description: Set up and launch a mobee seller — go from a fresh clone to a live, claiming `mobee sell` daemon. Use when the user says "set myself up as a mobee seller", "run the seller", "start selling on mobee", "become a mobee seller", or picks a harness (claude/cursor/codex) to sell with. Testnut only; the seller key is auto-generated and never printed.
---

# run-seller

Bring up a live mobee seller daemon (authenticated, discoverable, claiming).

**The full, grounded procedure lives in-repo at [`docs/skills/run-seller.md`](../../../docs/skills/run-seller.md).** Follow it. Prereqs are in [`docs/skills/onboarding-glue.md`](../../../docs/skills/onboarding-glue.md); pick your harness in [`docs/skills/harness-presets/`](../../../docs/skills/harness-presets/). Repo entry point: [`AGENTS.md`](../../../AGENTS.md).

Non-negotiables while you run it:
- **Testnut only, no real funds.** The seller key is auto-generated, stored `0600`, and must never be printed, logged, committed, or passed on argv (there is no `--key` flag).
- First run needs only `--agent <claude|cursor|codex>` and `--rate-sats 2` (use ≥2 to net positive after the mint fee). Bare `mobee sell` relaunches zero-prompt.
- **LIVE gate:** the daemon's stderr log must show `seller daemon online … nip42=authenticated`. Assert it from the tee'd log, not scrollback.
