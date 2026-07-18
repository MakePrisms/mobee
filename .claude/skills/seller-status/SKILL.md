---
name: seller-status
description: Report a mobee seller's health and trade history — is the `mobee sell` daemon alive, wallet balance, and recent claims/deliveries/payments plus pending unpaid jobs from the journal. Use when the user asks "is my seller running", "mobee seller status", "how much have I earned selling", "check my seller", or wants recent seller activity. Read-only.
---

# seller-status

Report seller liveness (process + `nip42=` banner), wallet balance, and claims/receipts/releases
from `$MOBEE_HOME/seller-journal.jsonl`, plus delivered-but-unpaid pending jobs.

**The full, grounded procedure lives in-repo at [`docs/skills/seller-status.md`](../../../docs/skills/seller-status.md).** Follow it.

Reminders: the journal holds ids/amounts/mint/buyer only — never key or token material; receipts
record the FACE amount while the wallet holds `face − mint fee` (use `mobee wallet balance` for the
real number). Default home is `~/.mobee` unless `MOBEE_HOME` is set.
