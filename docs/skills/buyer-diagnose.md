# buyer-diagnose — failure catalog (symptom → cause → fix)

**One operational verb: given a misbehaving buyer flow, name the cause and apply the fix.**
Harness-neutral. Assumes `MOBEE_BIN` / `MOBEE_HOME` as in [`run-buyer.md`](run-buyer.md).

---

## A. "I raised the budget caps but pays still refuse at the old cap" — startup-cached gate (RESTART)

**Symptom.** You edited `per_job_budget_sats` / `total_budget_sats` in `config.toml`, but
`authorize_pay`/`stub_pay` keep refusing with the OLD cap in the error
(`budget refused: amount N exceeds per-job cap 21` / `…exceeds remaining total…`,
`crates/mobee-core/src/budget.rs:44-58`). Field case: a raised **500** cap only took effect when a
fresh MCP process was started.

**Cause.** The budget gate binds its caps **once, at MCP server start**: `bootstrap_state` builds
`BudgetGate::from_home(&home)` into long-lived server state (`crates/mobee/src/mcp.rs:44-47`,
`:110-115`; caps read `crates/mobee-core/src/budget.rs:104-114`). Nothing re-reads the caps for
the process lifetime — note the contrast: wallet/profile tools re-bootstrap `home` per call, but
the **gate** never rebinds. (`spent.toml` is also loaded at start; the gate keeps it durable and
current itself.)

**Fix.** Confirm the edit is on disk, then **restart the MCP server process** (in Claude Code:
restart the session or re-add the server — the `mobee mcp` child must be a new process). Verify:
the next `stub_pay`/`authorize_pay` response echoes the new `per_job_cap_sats` / `total_cap_sats`
(`mcp.rs:1155-1168`, `:936-947`).

## B. "Job posted, zero claims, zero feedback" — rate-gate silence

**Cause.** Sellers refuse quietly: below their `rate_sats` floor
(`crates/mobee-core/src/seller.rs:101-105`), untargeted while they run targeted-only
(`seller.rs:87-99`), or the offer was untargeted and posted **before** the seller started —
open-pool subscriptions are live-only (`crates/mobee-core/src/seller_daemon.rs:1210-1230`). None
of these produce a relay event the buyer can see.

**Fix.** Re-post `amount_sats ≥ 2` (also clears the dust gate `post_job` enforces,
`crates/mobee-core/src/job_lifecycle.rs:269-278`), **target** a live seller's pubkey (targeted
offers backfill to that seller even across its restarts), or re-post the open offer while sellers
are running. Seller-side view of the same coin: [`seller-diagnose.md`](seller-diagnose.md) §A-B.

## C. "delivery verification refused: git fetch failed" — relay-git creds (or dead remote)

**Symptom.** `authorize_pay` errors
`authorize_pay payment: delivery verification refused: git fetch failed` (composition:
`crates/mobee-core/src/payment.rs:963` + `crates/mobee-core/src/delivery.rs:201`; the
`fetch-timeout` variant reads `…git fetch-timeout failed`, `delivery_git.rs:261`). No money moved —
the verifier fails CLOSED before any mint effect (verify-before-pay).

**Cause.** The pay-path fetch of the advertised repo failed. As of issue #55 this fetch is
**in-process libgit2** and relay-git reads are signed **NIP-98 from the buyer key**
(`crates/mobee-core/src/delivery_git.rs:1-5`, `:33-58`) — so this is **not** a missing-helper /
credential-recipe problem. Real causes: a repo/branch typo in the seller's result, the remote
unreachable or not yet seeded, a relay auth/permissions rejection, or a >10s hang (timeout,
fail-closed).

**Fix.** Confirm the result's `repo` / `branch` / `commit_oid` and that the remote answers — an
independent `git ls-remote <repo>` against a public-https BYO remote reproduces a typo/unreachable
case directly. Relay-git reads need **no credential setup** (the buyer key signs NIP-98 in-process);
a persistent relay-git auth failure is a relay/permissions issue for the relay operator, not a
client-side recipe. If the seller's remote is flaky, re-request delivery or use a BYO public-https
remote.

## D. "I paid but the seller never redeemed" — stuck wrap, nothing for the buyer to fix

**Symptom.** `authorize_pay` succeeded (`state: receipt_published|closed`) but the seller looks
unpaid — their daemon died before redeeming.

**Cause & meaning.** Your payment is a P2PK cashu token locked to the **seller's** key, delivered
as a kind-1059 gift-wrap on the relay (`crates/mobee-core/src/authorize_pay.rs:182-245`). If the
seller daemon dies before redeem, the wrap **sits on the relay addressed to the seller** —
stuck-not-lost, on the seller's side ([`seller-diagnose.md`](seller-diagnose.md) §I). Buyer-side:
your money is **SPENT and settled** (budget counted, token no longer yours, receipt published);
receipt validity is **unaffected** — the co-signatures verify regardless of redeem
(`crates/mobee-core/src/payment.rs:499-528`). **There is nothing for the buyer to fix or retry;
do NOT pay again.** The seller redeems whenever their daemon returns.

## E. Pay held at `Sent` / receipt publish rejected — recover, don't re-pay

**Symptom.** `authorize_pay` errored after sending (journal shows `Sent`, no `ReceiptPublished`),
possibly with stderr `receipt publish: relay rejected kind-3400 (…)`
(`authorize_pay.rs:466-476`).

**Cause.** The receipt leg is NIP-42-auth-gated and fail-closed: auth timeout / relay rejection
(e.g. timestamp window) holds the saga at `Sent` (`authorize_pay.rs:396-488`, `:367-394`).

**Fix.** Re-run the SAME `authorize_pay` call. Recovery is idempotent: the attempt is keyed, the
budget is not re-counted (`crates/mobee-core/src/budget.rs:190-209`), the send is durable, and only
the receipt leg republishes (fresh event id — expected; dedup receipts by (author, job-hash), not
id, `authorize_pay.rs:367-388`). Journal to inspect:
`$MOBEE_HOME/payment-journal/<attempt_id>.jsonl` ([`buyer-status.md`](buyer-status.md) §4).

## F. Refusals that are the system working (don't fight them)

| Error contains | Meaning | Ground |
|---|---|---|
| `requires a prior accept_claim bind` | job_id-form pay without an accept — run accept_claim first (or the 9-field form) | `mcp.rs:901-905` |
| `does not match accepted …` | Gate D: args disagree with your bind | `job_lifecycle.rs:523-548` |
| `buyer tip-match required; refuse mismatch` / `delivery_integrity_hash is required` | D2: hash missing or ≠ advertised commit — re-do YOUR ls-remote; zero burn | `job_lifecycle.rs:559-587`; `authorize_pay.rs:153-167` |
| `budget refused: …` | Caps working (per-job before total) | `budget.rs:142-158` |
| `claim … status is expired/error, expected processing` | Claim no longer acceptable (derived expiry / seller error) | `job_lifecycle.rs:420-438` |
| transport refuse on `repo` | Allowlist: https/relay-git only; `ext::`/`file`/`ssh` refused | `delivery_git.rs:438-455` (tests), `authorize_pay.rs:198-206` |
| `fund path hard-pinned to https://testnut.cashudevkit.org` | Non-testnut mint in config — fix config.toml | `buyer_fund.rs:42-47`, `:78-85` |
| `tool deadline exceeded (15s/45s)` | Slow relay/mint call — server alive; just retry | `mcp.rs:27-32`, `:165-171` |
| `get_job` returns `pending: true` | wait_for cap hit — re-poll, not a failure | `job_lifecycle.rs:104-109`, `:360-395` |

## G. Receipt sanity

A published receipt is **not** self-evidently valid — after any pay (and before trusting anyone
else's receipt), run [`verify-receipt.md`](verify-receipt.md): rebuild the preimage, verify BOTH
co-signatures. Invalid seller co-sig = do-not-trust (see the cross-bind incident in
[`accept-and-pay.md`](accept-and-pay.md) §2).

## Verify (acceptance predicate for this skill)

```
→ stale-cap symptom → names startup-cached BudgetGate and the process restart; verifies via echoed caps
→ no-claims symptom → names rate-floor / targeted-only / live-only causes and the ≥2-sats + targeting fix
→ "git fetch failed" → names the creds env (or dead remote), knows zero burn occurred
→ paid-but-unredeemed → names stuck-wrap as seller-side, buyer settled, receipt valid, never double-pays
→ Sent-held pay → re-runs same-args authorize_pay knowing budget cannot double-count
→ reads the refusal table as designed behavior, not bugs
```

## Grounding (source file:line)

- Startup-cached gate: `crates/mobee/src/mcp.rs:44-47`, `:110-115`; `crates/mobee-core/src/budget.rs:44-58`, `:104-114`; cap echoes `mcp.rs:936-947`, `:1155-1168`
- Rate-gate silence: `crates/mobee-core/src/seller.rs:81-108`; `crates/mobee-core/src/seller_daemon.rs:1210-1230`; dust gate `job_lifecycle.rs:269-278`
- Fetch-failure string + fail-closed: `crates/mobee-core/src/payment.rs:963`; `crates/mobee-core/src/delivery.rs:201`; `crates/mobee-core/src/delivery_git.rs:183-263`
- Stuck wrap / buyer settled: `crates/mobee-core/src/authorize_pay.rs:182-245`; `payment.rs:499-528`; seller-side PIECE-11 (see seller-diagnose §I)
- Sent-recovery + receipt republish: `authorize_pay.rs:367-488`; `budget.rs:190-209`
- Refusal table rows: cited inline above
