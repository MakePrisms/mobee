# Buyer quickstart — zero → paid (dev / testnut)

Documented buyer steps only. **Testnut only. No real funds. The key never leaves the box.**

Pinned surface: `dev` tip with composed `authorize_pay` (`BudgetGate` → `PayPathDeliveryVerifier` → `PaymentService::run()`).

Reality class for this path: **REAL-AND-LIVE (testnut)**. `main` remains **BUILT-BUT-OFF** until back-pull.

---

## What this tip exposes

Buyer MCP tools on this tip (exactly three):

| Tool | Role |
|------|------|
| `setup_wallet` | Bootstrap `~/.mobee` (config + autogen key + wallet) and fund against the hard-pinned testnut mint |
| `stub_pay` | Exercise budget caps over a mock pay (no piece-6 `run()`) |
| `authorize_pay` | Real capped pay: delivery-verify (tip-match) then `run()` |

**Not on this tip:** `post_job`, `get_job`, `accept_claim`. For the acceptance loop those marketplace legs are the **seller stub** (named below) — not buyer MCP commands. Do not invent buyer commands that are not in the table.

Defaults written on first bootstrap (`~/.mobee/config.toml`):

- mint: `https://testnut.cashudevkit.org` (hard-pinned; retarget refused)
- relay: `wss://mobee-relay.orveth.dev`
- caps: `per_job_budget_sats = 21`, `total_budget_sats = 100`

---

## 0. Fresh home

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

Every request below is one JSON line on stdin; every response is one JSON line on stdout. Diagnostics go to stderr only.

Initialize once:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"quickstart","version":"0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
```

List tools (optional check — expect exactly `setup_wallet`, `stub_pay`, `authorize_pay`):

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

## 3. Seller stub — post → claim → deliver

**Seller: stub** (not live c2). The stub stands in for marketplace posting / claim / git delivery so the buyer can exercise the composed pay path. Buyer steps never call stub internals except to **read the result packet** the stub prints.

Stub recipe (run in a second shell; does not touch buyer `MOBEE_HOME`):

```bash
# --- seller stub: post_job ---
JOB_ID="job-acceptance-$(date +%s)"
JOB_HASH="$(printf '%s' "$JOB_ID" | sha256sum | awk '{print $1}')"
echo "STUB post_job job_id=$JOB_ID job_hash=$JOB_HASH"

# --- seller stub: claim (ephemeral nostr key; pubkey only on stdout) ---
STUB_DIR="$(mktemp -d /tmp/mobee-seller-stub-XXXXXX)"
SELLER_KEY_FILE="$STUB_DIR/key"   # mode 0600; never print / commit / paste
SELLER_PUB="$(cargo run --release --manifest-path scripts/gen_nostr_keypair/Cargo.toml -- "$SELLER_KEY_FILE")"
echo "STUB claim seller_pubkey=$SELLER_PUB"

# --- seller stub: deliver (https git tip) ---
# Tip-match requires an allowlisted https locator (ext:: / file / ssh / local path refused).
REPO_URL="https://github.com/MakePrisms/mobee.git"
BRANCH="main"
COMMIT_OID="$(git ls-remote "$REPO_URL" "refs/heads/$BRANCH" | awk '{print $1}')"
RESULT_ID="result-acceptance-$(date +%s)"
echo "STUB deliver repo=$REPO_URL branch=$BRANCH commit_oid=$COMMIT_OID result_id=$RESULT_ID"
```

Buyer independent commitment (required — do not blind-copy seller advertising without checking):

```bash
BUYER_TIP="$(git ls-remote "$REPO_URL" "refs/heads/$BRANCH" | awk '{print $1}')"
test "$BUYER_TIP" = "$COMMIT_OID" && echo "buyer tip-match commitment ok"
DELIVERY_INTEGRITY_HASH="$BUYER_TIP"
```

Result packet the buyer feeds to `authorize_pay`:

| Field | Source |
|-------|--------|
| `job_id` | stub `post_job` |
| `job_hash` | stub `post_job` (64-hex SHA-256) |
| `result_id` | stub deliver |
| `seller_pubkey` | stub claim (64-hex x-only pubkey) |
| `repo` | stub deliver (`https://…` only) |
| `branch` | stub deliver |
| `commit_oid` | stub deliver |
| `delivery_integrity_hash` | **buyer** `ls-remote` tip (must equal `commit_oid`) |
| `amount_sats` | buyer choice within caps (e.g. `1`) |

---

## 4. Pay — `authorize_pay` (within caps)

```json
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"authorize_pay","arguments":{
  "job_id":"<job_id>",
  "result_id":"<result_id>",
  "delivery_integrity_hash":"<buyer tip oid>",
  "job_hash":"<64 hex>",
  "seller_pubkey":"<seller pubkey>",
  "amount_sats":1,
  "repo":"https://github.com/MakePrisms/mobee.git",
  "branch":"main",
  "commit_oid":"<same tip oid>"
}}}
```

Pass criteria:

- tool `ok: true`
- `piece6` == `run`
- `verifier` == `PayPathDeliveryVerifier`
- `state` reaches `receipt_published` or `closed` with a `receipt` / `receipt_id`
- `amount_sats` matches the request; spent accounting moves

That receipt object in the response (and the payment journal under `$MOBEE_HOME/payment-journal/`) is the run's receipt evidence.

---

## 5. Negative probes (same composed binary)

### 5a. Over-cap REFUSED

Default per-job cap is `21`. Ask for `22`:

```json
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"stub_pay","arguments":{"amount_sats":22}}}
```

Or the real path (also refused at the gate before mint):

```json
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"authorize_pay","arguments":{
  "job_id":"job-overcap",
  "result_id":"result-overcap",
  "delivery_integrity_hash":"<any 40-hex>",
  "job_hash":"<64 hex>",
  "seller_pubkey":"<seller pubkey>",
  "amount_sats":22,
  "repo":"https://github.com/MakePrisms/mobee.git",
  "branch":"main",
  "commit_oid":"<any 40-hex>"
}}}
```

Expect an error response containing a budget refuse (not a successful pay).

### 5b. `ext::` locator REFUSED

```json
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"authorize_pay","arguments":{
  "job_id":"job-ext",
  "result_id":"result-ext",
  "delivery_integrity_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "job_hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "seller_pubkey":"<seller pubkey>",
  "amount_sats":1,
  "repo":"ext::sh -c evil",
  "branch":"main",
  "commit_oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
}}}
```

Expect refuse (transport allowlist / `ext` / forbidden scheme). No successful pay.

---

## Acceptance checklist

```
fresh home
→ documented steps only (this file)
→ setup_wallet with balance_sats > 0
→ seller stub: post_job → claim → https deliver
→ buyer tip commitment == advertised commit_oid
→ authorize_pay within caps → receipt
→ over-cap REFUSED
→ ext:: REFUSED
```

Seller named in evidence: **stub** (not live c2).
