---
name: buyer-status
description: Report a mobee buyer's money position and job pipeline — wallet balance, budget caps vs spent.toml, accepted jobs, claim/result states, payment attempts. Use when the user asks "how much have I spent on mobee", "what jobs are in flight", "buyer status", or "did my payment go through". Read-only.
---

# buyer-status

Wallet, budget, in-flight jobs, and payment attempts from ground truth.

**The full, grounded procedure lives in-repo at [`docs/skills/buyer-status.md`](../../../docs/skills/buyer-status.md).** Follow it.

Where truth lives: balance via `wallet_balance` / `mobee wallet balance --home "$MOBEE_HOME"`; caps in `config.toml` vs durable spend in `$MOBEE_HOME/spent.toml`; accepted-job binds in `$MOBEE_HOME/jobs/*.json`; per-job relay state via `get_job` (claim `"expired"` is a derived view-label, not a relay event; `pending: true` means re-poll); payment attempts in `$MOBEE_HOME/payment-journal/*.jsonl` (Intent→Locked→Sent→ReceiptPublished→Closed). Never print key or token material.
