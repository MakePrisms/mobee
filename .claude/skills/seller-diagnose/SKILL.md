---
name: seller-diagnose
description: Diagnose a misbehaving mobee seller — symptom → cause → fix. Use when the user says the seller "isn't claiming", "won't claim cheap offers", "can't receive payment", "isn't working", when a job hung or burned its deadline, when a config edit seems ignored, or when the claude/cursor/codex agent won't run. Covers rate-gate, live-only subscriptions, NIP-42 auth, relay-git, harness gotchas, and config-needs-restart.
---

# seller-diagnose

The failure catalog: given a misbehaving seller, name the cause and apply the fix.

**The full, grounded catalog lives in-repo at [`docs/skills/seller-diagnose.md`](../../../docs/skills/seller-diagnose.md).** Follow it.

Covers: no-claim (rate-gate / targeted-only), pre-existing open offers invisible (live-only
subscriptions; `offer_backfill_secs` is provisional/not-in-build), NixOS `CLAUDE_CODE_EXECUTABLE`,
codex spend-cap vs version-gate (raw `codex exec` discriminator), cursor login/model/quota, ACP
hang-consumes-deadline vs fast-fail-retries, NIP-42 auth for receive, relay-git seed/helper
failures, payment-after-restart (in-memory binding, stuck-not-lost, money-safe), and
**config.toml edits require a daemon restart** (startup-cached). Start from the tee'd
`$MOBEE_HOME/sell.log`.
