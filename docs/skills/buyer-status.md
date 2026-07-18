# buyer-status — wallet, budget, and in-flight jobs from ground truth

**One operational verb: report the buyer's money position and job pipeline.** Read-only.
Harness-neutral. Assumes `MOBEE_BIN` / `MOBEE_HOME` set as in [`run-buyer.md`](run-buyer.md).

---

## 1. Wallet balance

MCP: `wallet_balance {}` → per-mint `balance_sats` + `total_sats` (`crates/mobee/src/mcp.rs:311-318`,
`:543-564`). CLI twin (no server needed):

```bash
"$MOBEE_BIN" wallet balance --home "$MOBEE_HOME"
```

(`crates/mobee/src/wallet_cli.rs:146-207`.)

## 2. Budget position — caps vs spent

Caps live in `config.toml`; durable spend lives in `$MOBEE_HOME/spent.toml`:

```bash
grep -E "per_job_budget_sats|total_budget_sats" "$MOBEE_HOME/config.toml"
cat "$MOBEE_HOME/spent.toml" 2>/dev/null || echo "no spend yet (file created on first pay)"
# fields: spent_sats = <n>, attempt_ids = [...]  (attempts already counted — retry-idempotent)
```

`remaining = total_cap − spent_sats`. Grounds: file + shape
`crates/mobee-core/src/budget.rs:24`, `:66-72`, `:104-114`, `:128-130`; caps default 21/100
`crates/mobee-core/src/home.rs:19-22`. Every `stub_pay`/`authorize_pay` response also echoes
`spent_total_sats` / `remaining_sats` / both caps (`mcp.rs:936-947`, `:1155-1168`).

> A raised cap in `config.toml` does NOT apply to a running MCP server — the gate binds caps at
> server start. See [`buyer-diagnose.md`](buyer-diagnose.md) §A.

## 3. In-flight jobs

**Which jobs have you accepted?** Local binds, one file per accepted job:

```bash
ls "$MOBEE_HOME/jobs/" 2>/dev/null        # <job_id>.json per accept_claim
# each: {job_id, claim_id, result_id, seller_pubkey, commit_oid, repo, branch, job_hash, amount_sats, …}
```

(`crates/mobee-core/src/job_lifecycle.rs:28`, `:159-177`, `:589-604`.)

**What state is a job in?** `get_job {"job_id": …}` per job — relay truth:

| Signal | Meaning |
|--------|---------|
| `claims[]` empty | Nothing claimed yet (check pricing/targeting — [`buyer-diagnose.md`](buyer-diagnose.md) §B) |
| claim `status: processing`, `live: true` | Seller working; `live_claim_id` names it |
| claim `status: error` | Seller failed/released the claim (their kind-7000 error) |
| claim `status: "expired"` | **Derived label, not a relay event**: a `processing` claim past the offer deadline. View-level only — nothing was published to make it so, and it flips purely on `now` vs `deadline_unix`. It is excluded from `live_claim_id` and `accept_claim` refuses it; a late delivery may still appear in `results[]` but is no longer acceptable |
| `results[]` non-empty | Delivery advertised (repo/branch/commit_oid) — go to [`accept-and-pay.md`](accept-and-pay.md) |
| `accepted` present | Your local bind (mirrors `jobs/<job_id>.json`) |
| `pending: true` | Only when `wait_for` was set: the ~10s wait cap hit first — re-poll, not an error |

Grounds: view + states `job_lifecycle.rs:96-157`, claims filter (processing/error)
`:807-825`, expiry derivation `:703-733` (constant `:35-37`), expired-accept refuse `:420-438`,
pending semantics `:104-109`, `:360-395`.

## 4. Payment attempts (the money trail)

One write-ahead journal per pay attempt:

```bash
ls "$MOBEE_HOME/payment-journal/" 2>/dev/null    # <attempt_id>.jsonl
```

States progress `Intent → Locked → Sent → ReceiptPublished → Closed`
(`crates/mobee-core/src/payment.rs:224-243`; journal dir `authorize_pay.rs:248-251`). A pay held
at `Sent` = money sent but the receipt leg not yet confirmed — re-running `authorize_pay` with the
same args recovers idempotently (attempt-keyed; budget not re-counted, `budget.rs:190-209`).
`reconcile_wallet {}` retires stuck wallet Send-sagas conservatively (`mcp.rs:301-309`,
`:951-973`). Verified deliveries are custodied in `$MOBEE_HOME/custody`
(`authorize_pay.rs:252-253`) — possession survives the seller deleting the branch.

## Verify (acceptance predicate for this skill)

```
→ reports total_sats (wallet) and spent_sats vs caps (budget) from ground truth files/tools
→ lists accepted jobs from $MOBEE_HOME/jobs/ and per-job relay state via get_job
→ reads claim states correctly, incl. "expired" as a derived view-label (not a relay event)
→ lists payment attempts from $MOBEE_HOME/payment-journal/ and names any held at Sent
→ prints no key or token material
```

## Grounding (source file:line)

- wallet_balance MCP/CLI: `crates/mobee/src/mcp.rs:311-318`, `:543-564`; `crates/mobee/src/wallet_cli.rs:146-207`
- Budget caps/spent/remaining: `crates/mobee-core/src/budget.rs:24`, `:66-72`, `:104-130`; `crates/mobee-core/src/home.rs:19-22`; echoes `mcp.rs:936-947`, `:1155-1168`
- Binds dir + shape: `crates/mobee-core/src/job_lifecycle.rs:28`, `:159-177`, `:589-604`
- get_job states / expiry / pending: `job_lifecycle.rs:96-157`, `:703-733`, `:807-830`, `:104-109`, `:360-395`, `:420-438`
- Payment journal + states + recovery: `crates/mobee-core/src/authorize_pay.rs:248-253`; `crates/mobee-core/src/payment.rs:224-243`; `budget.rs:190-209`; reconcile `mcp.rs:301-309`, `:951-973`
