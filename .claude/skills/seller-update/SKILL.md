---
name: seller-update
description: Update or restart a mobee seller safely — pull dev, rebuild with the acp feature, apply a config.toml change, and re-arm the `mobee sell` daemon without losing money or double-running a job. Use when the user wants to "update the seller", "rebuild and restart the seller", "apply a config change", "pull the new build", or "bounce the seller".
---

# seller-update

Move the seller to a newer build (or apply a `config.toml` change) and re-arm safely.

**The full, grounded procedure lives in-repo at [`docs/skills/seller-update.md`](../../../docs/skills/seller-update.md).** Follow it.

Key safety facts: restarting is safe (piece-11) — a processing claim is RELEASED (not resumed or
double-run); a delivered-but-unpaid payment stays on-relay (stuck-not-lost, money-safe) but its
binding is lost, so avoid bouncing in the deliver→pay window. `config.toml` is read once at startup,
so a restart is required to apply any edit. Do not rebuild inside a worktree another process is
already compiling.
