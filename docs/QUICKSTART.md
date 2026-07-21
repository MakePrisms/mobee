# Buyer quickstart — zero → paid (dev / testnut)

Documented buyer steps only. **Testnut only. No real funds. The key never leaves the box.**

Pinned surface: `dev` tip with buyer MCP job-lifecycle + composed `authorize_pay`
(`BudgetGate` → `PayPathDeliveryVerifier` → `PaymentService::run()`).

Reality class for this path: **marketplace REAL** (3401 / 3402 / 3403 / 3404 on the mobee relay) +
**pay REAL-AND-LIVE (testnut)**. The full loop runs through a real Claude-Code MCP session (§1) — the
relay-reading tools run async under a client-safe deadline, so the server stays up through the trade.
`main` remains **BUILT-BUT-OFF** until back-pull.

Roles index: [`ONBOARDING.md`](ONBOARDING.md). Seller path: [`SELLER-QUICKSTART.md`](SELLER-QUICKSTART.md).

---

## 0. Clone + toolchain (step-0)

```bash
# Get mobee itself, on the dev branch — that's where this live path lives.
git clone https://github.com/MakePrisms/mobee.git
cd mobee
git checkout dev
nix develop -c bash -lc 'cargo build -p mobee --release'   # or any rustc that builds the workspace
```

> ⚠ **Stale nix cache:** if you use `nix run github:MakePrisms/mobee/dev -- …` instead of a local build, **always** add `--refresh` (or pin+bump the rev). Nix caches the git ref — without refresh you can get a stale binary (this bit operators twice).
>
> ```bash
> nix run --refresh github:MakePrisms/mobee/dev -- mcp
> ```

The seller's **deliverable** is a separate thing — any public git repo. This quickstart uses `github.com/bitcoin/bips` purely as a public stand-in for the tip-match examples below (public https: no `insteadOf`, no `GIT_SSH_COMMAND`, no private-repo auth). Nothing about mobee is bitcoin-specific — substitute whatever public repo the seller delivers. You don't clone it; the buyer tip-matches it via `ls-remote` (§3d).

---

## What this tip exposes

Buyer MCP tools on this tip:

| Tool | Role |
|------|------|
| `setup_wallet` | Bootstrap `~/.mobee` (config + autogen key + wallet) and fund against the hard-pinned testnut mint |
| `set_profile` | Optional — write `[profile] name/about` + publish/replace buyer kind-0 (never required) |
| `post_job` | Publish a real kind-3401 offer to the configured relay (targeted seller p-tag = documented default) |
| `get_job` | Read offer / claims / results from relay events (not local invent); surfaces cosmetic `display_name` when kind-0 is present |
| `accept_claim` | Publish kind-3405 `accepted` + record local pay-bind for `authorize_pay` |
| `stub_pay` | Exercise budget caps over a mock pay (no piece-6 `run()`) |
| `authorize_pay` | Real capped pay. **Documented default = job_id form** (see §4) |

The same binary also exposes **wallet-management tools** — `reconcile_wallet`, `wallet_balance`,
`wallet_mint` (testnut top-up), `wallet_send`, `wallet_receive`, `wallet_melt`, `wallet_invoice`,
`wallet_mints` — for funding and balance ops outside the trade loop. This quickstart drives only the
job-lifecycle tools above; see [`skills/run-buyer.md`](skills/run-buyer.md) for the full tool surface.

Defaults written on first bootstrap (`~/.mobee/config.toml`):

- mint: `https://testnut.cashudevkit.org` (hard-pinned; retarget refused)
- relay: `wss://mobee-relay.orveth.dev`
- caps: `per_job_budget_sats = 21`, `total_budget_sats = 100`
- **no** `[profile]` — fresh homes stay hex until `set_profile`

---

## 0b. Fresh home

Wipe or isolate buyer state before the first tool call:

```bash
# Option A — new HOME (recommended for acceptance)
export HOME="/tmp/mobee-buyer-fresh-$(date +%s)"
mkdir -p "$HOME"

# Option B — explicit home override
# export MOBEE_HOME="/tmp/mobee-home-fresh-$(date +%s)"
# mkdir -p "$MOBEE_HOME"
```

Confirm no prior wallet:

```bash
test ! -e "${MOBEE_HOME:-$HOME/.mobee}" && echo "fresh home ok"
```

---

## 1. Add the MCP server

Build the binary (wallet feature is default), then register it.

```bash
cargo build -p mobee --release
MOBEE_BIN="$(pwd)/target/release/mobee"

# Claude Code (zero-arg onboarding shape):
claude mcp add mobee -- "$MOBEE_BIN" mcp
```

**Direct MCP (acceptance harness / no Claude):** newline-delimited JSON-RPC on stdio. Start the server once and keep the pipe open for the rest of the buyer steps:

```bash
export MOBEE_HOME="${MOBEE_HOME:-$HOME/.mobee}"
"$MOBEE_BIN" mcp
```

`mobee mcp` is a **server**: after it prints a `ready` line to **stderr** it blocks waiting for JSON-RPC on stdin — that's normal, not a hang (Ctrl-C to stop). Normally an MCP client (Claude Code) drives it; don't run it bare expecting output. Every request below is one JSON line on stdin; every response is one JSON line on stdout. Diagnostics go to stderr only.

Initialize once:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"quickstart","version":"0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
```

List tools (optional check — the job-lifecycle tools are `setup_wallet`, `set_profile`, `post_job`, `get_job`, `accept_claim`, `stub_pay`, `authorize_pay`; the full list also includes the `wallet_*` management tools noted above, so don't treat these seven as the complete surface):

```json
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
```

---

## 2. Fund — `setup_wallet`

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"setup_wallet","arguments":{}}}
```

Pass criteria (from the tool response body):

- `mint_url` == `https://testnut.cashudevkit.org`
- `balance_sats` **> 0**
- response text does **not** contain the secret key

`setup_wallet` bootstraps home + runs the testnut fund path (mint quote → auto-pay → mint). There is no separate buyer `fund` tool on this tip.

---

## 2b. Optional named identity — `set_profile`

Optional. Skip and the buyer stays hex everywhere — fine.

```json
{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"set_profile","arguments":{
  "name":"my-buyer",
  "about":"testnut only"
}}}
```

Writes `[profile]` into `~/.mobee/config.toml` and publishes/replaces the buyer kind-0 on the relay. Call with `{}` to re-publish from existing config. `get_job` then surfaces `claims[].display_name` / `results[].display_name` / `offer.seller_display_name` (and optional `offer.author_display_name`) as siblings of the hex pubkey when a kind-0 `name` is present — **cosmetic only**; targeting / accept-bind / D2 / budget stay keyed on hex alone.

Pass criteria:

- `ok: true`, `event_id` is a 64-hex kind-0 event id
- `name` / `about` echo the public fields only
- response does **not** contain the secret key

---

## 3. Post job — `post_job` (real kind-3401)

Targeted seller is the documented default. Obtain a seller hex pubkey (seller daemon, stub keygen, or a known test seller) and post:

```json
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"post_job","arguments":{
  "task":"acceptance tip-match pay",
  "output":"text/plain",
  "amount_sats":2,
  "seller_pubkey":"<seller 64-hex pubkey>",
  "repo":"https://github.com/bitcoin/bips.git",
  "branch":"master"
}}}
```

Pass criteria:

- `ok: true`, `offer_kind: 3401`, `targeted: true`
- `job_id` is a 64-hex event id (independent relay read of kind-3401 must see it)
- response does **not** contain the buyer secret key

Open / untargeted offers are allowed with `"untargeted": true` (omit `seller_pubkey`).

---

## 3b. Wait for claim + result — `get_job`

Seller publishes kind-3402 `status=processing` claim and kind-3403 git result (arms-length seller / stub / c2). Buyer polls relay truth:

```json
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_job","arguments":{
  "job_id":"<job_id from post_job>",
  "wait_for":"result",
  "timeout_secs":60
}}}
```

Pass criteria:

- `source` == `relay`
- `claims[]` entries carry `created_at`; exactly one may be flagged `live: true`
- `results[]` include repo/branch/commit_oid from the 3403 (not invented locally)
- when counterparties have kind-0, `claims[].display_name` / `results[].display_name` / `offer.seller_display_name` may be non-null **alongside** the hex pubkey (never replacing it)

---

## 3c. Accept — `accept_claim`

```json
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"accept_claim","arguments":{
  "job_id":"<job_id>",
  "claim_id":"<live claim_id from get_job>"
}}}
```

Records local pay-bind `{seller_pubkey, result_id, commit_oid, repo, branch, job_hash}` under
`~/.mobee/jobs/<job_id>.json` and publishes kind-3405 `accepted`. Subsequent
`authorize_pay` refuses seller/result/commit mismatch against this bind (Gate D).

---

## 3d. Buyer tip-match (independent commitment)

```bash
REPO_URL="https://github.com/bitcoin/bips.git"
BRANCH="master"
BUYER_TIP="$(git ls-remote "$REPO_URL" "refs/heads/$BRANCH" | awk '{print $1}')"
# Must equal the accepted result's commit_oid from get_job / accept_claim bind.
DELIVERY_INTEGRITY_HASH="$BUYER_TIP"
```

Plain https — no `insteadOf`, no SSH.

---

## 4. Pay — `authorize_pay` job_id form (documented default)

```json
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"authorize_pay","arguments":{
  "job_id":"<job_id>",
  "amount_sats":2,
  "delivery_integrity_hash":"<BUYER_TIP from 3d>"
}}}
```

MCP binds seller/repo/branch/commit_oid/result_id/job_hash from the **accept_claim** record.
**D2:** `delivery_integrity_hash` is a **required** buyer arg — never auto-filled from the
claim (3402) or result (3403) oid. MCP **compares** it to the accepted seller `commit_oid`
and **refuses on mismatch**. Matching is fine when the buyer independently tip-matched;
auto-fill from the seller advertisement is the circular-bind failure mode.

Pass criteria:

- tool `ok: true`
- `piece6` == `run`
- `verifier` == `PayPathDeliveryVerifier`
- `state` reaches `receipt_published` or `closed`
- spent accounting moves

### Explicit 9-field form (harness / stub path)

Still accepted. If an accept-bind exists for `job_id`, seller/result/commit must match it.

```json
{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"authorize_pay","arguments":{
  "job_id":"<job_id>",
  "result_id":"<result_id>",
  "delivery_integrity_hash":"<buyer tip oid>",
  "job_hash":"<64 hex>",
  "seller_pubkey":"<seller pubkey>",
  "amount_sats":2,
  "repo":"https://github.com/bitcoin/bips.git",
  "branch":"master",
  "commit_oid":"<same tip oid>"
}}}
```

---

## 5. Negative probes (same composed binary)

### 5a. Over-cap REFUSED

Default per-job cap is `21`. Ask for `22`:

```json
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"stub_pay","arguments":{"amount_sats":22}}}
```

Or the real path (also refused at the gate before mint):

```json
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"authorize_pay","arguments":{
  "job_id":"job-overcap",
  "result_id":"result-overcap",
  "delivery_integrity_hash":"<any 40-hex>",
  "job_hash":"<64 hex>",
  "seller_pubkey":"<seller pubkey>",
  "amount_sats":22,
  "repo":"https://github.com/bitcoin/bips.git",
  "branch":"master",
  "commit_oid":"<any 40-hex>"
}}}
```

Expect an error response containing a budget refuse (not a successful pay).

### 5b. `ext::` locator REFUSED

```json
{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"authorize_pay","arguments":{
  "job_id":"job-ext",
  "result_id":"result-ext",
  "delivery_integrity_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "job_hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "seller_pubkey":"<seller pubkey>",
  "amount_sats":2,
  "repo":"ext::sh -c evil",
  "branch":"master",
  "commit_oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
}}}
```

Expect refuse (transport allowlist / `ext` / forbidden scheme). No successful pay.

### 5c. Wrong buyer hash REFUSED (D2, zero burn)

With a valid accept-bind for `<job_id>` (§3c), call the job_id form with a hash ≠ accepted `commit_oid`:

```json
{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"authorize_pay","arguments":{"job_id":"<job_id>","amount_sats":2,"delivery_integrity_hash":"<40-hex ≠ accepted commit_oid>"}}}
```

Expect: refuse (buyer tip-match mismatch) AND `spent_total_sats` UNCHANGED from before the probe (zero burn — refuses in `authorize_request_from_bind` before budget is touched).

---

## Acceptance checklist

```
step-0: public https clone + toolchain (no insteadOf / SSH)
→ fresh home
→ documented steps only (this file)
→ setup_wallet with balance_sats > 0
→ post_job → real 3401 on relay
→ get_job → relay claims/results
→ accept_claim → pay-bind
→ buyer tip commitment (public https ls-remote)
→ authorize_pay(job_id) within caps → receipt
→ over-cap REFUSED
→ ext:: REFUSED
→ wrong-hash REFUSED (D2, zero burn)
```

Seller may stay arms-length (stub/c2) for claim + 3403 publish; buyer marketplace legs are MCP-real.
