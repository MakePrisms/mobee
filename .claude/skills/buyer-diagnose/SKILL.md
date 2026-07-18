---
name: buyer-diagnose
description: Diagnose a misbehaving mobee buyer flow — symptom → cause → fix. Use when pays refuse at a stale budget cap, a posted job gets zero claims, authorize_pay says "delivery verification refused - git fetch failed", a paid seller never redeemed, or a payment is stuck at Sent. Covers the startup-cached budget gate, rate-gate silence, relay-git fetch creds, and stuck wraps.
---

# buyer-diagnose

The buyer failure catalog: name the cause, apply the fix.

**The full, grounded catalog lives in-repo at [`docs/skills/buyer-diagnose.md`](../../../docs/skills/buyer-diagnose.md).** Follow it.

Headlines: raised budget caps need an **MCP server process restart** (the gate binds caps at start — field case: a raised 500 cap only took effect in a fresh process); zero claims = rate-floor / targeting / live-only silence (price ≥ 2 sats, target a live seller); "git fetch failed" on relay-git = missing credential-helper env at server launch (zero burn — verify-before-pay failed closed); paid-but-seller-never-redeemed = stuck wrap on the seller's side, buyer money is spent AND settled, receipt validity unaffected, never pay again; Sent-held pays recover by re-running the same authorize_pay (attempt-keyed, budget never double-counts). Receipt doubts → verify-receipt.
