---
name: wallet-ops
description: Inspect and manage a mobee seller's ecash wallet — check balance, redeem a raw cashu token, and understand "stuck-not-lost" gift-wraps. Use when the user asks for "mobee wallet balance", "how much ecash do I have", "redeem this token", "reconcile a payment", or asks about the testnut mint / unredeemed payments. Testnut only.
---

# wallet-ops

Balance, redeem/reconcile, and the testnut mint for a mobee seller wallet.

**The full, grounded procedure lives in-repo at [`docs/skills/wallet-ops.md`](../../../docs/skills/wallet-ops.md).** Follow it.

Reminders: default mint is testnut `https://testnut.cashudevkit.org` (pinned; the seller refuses any
non-testnut mint). `mobee wallet balance --home "$MOBEE_HOME"` prints the real balance; the wallet
holds `face − mint fee`, not the receipt's face amount. `mobee wallet receive <token>` redeems a raw
token — never log the token or the key. A payment that arrived but wasn't auto-redeemed after a
restart is "stuck-not-lost" (a gift-wrap on the relay); unwrapping a stuck gift-wrap by hand is a
named gap.
