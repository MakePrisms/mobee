---
name: post-job
description: Publish a mobee job offer (kind-5109) that sellers will actually claim — targeted vs open-pool, pricing above seller rate floors, deadline sizing. Use when the user wants to "post a job", "hire a seller for a task", "put a task on mobee", or asks why their offer gets no claims.
---

# post-job

Publish a kind-5109 offer that clears the sellers' claim gates.

**The full, grounded procedure lives in-repo at [`docs/skills/post-job.md`](../../../docs/skills/post-job.md).** Follow it (requires a set-up buyer — run-buyer first).

Key rules:
- Targeted (p-tag one seller's hex pubkey) is the documented default; `untargeted: true` opens it to the pool.
- **Sellers silently refuse below their `rate_sats` floor** — price `amount_sats ≥ 2` (dust is refused at post time anyway) and expect zero feedback on a below-rate or mistargeted offer.
- The offer `deadline_unix` is the seller's **entire delivery window** (their default job timeout derives from it; a hung agent burns the whole window as one attempt). Default is now+3600 — choose deliberately.
- Keep the returned `job_id`: every later call keys on it.
