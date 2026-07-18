# run-buyer — zero → funded buyer, ready to hire

**One operational verb: stand up a mobee buyer — home + key + funded testnut wallet + a working
driver for the MCP tools.** Harness-neutral: any agent (claude, codex, cursor) or a human can
follow this.

> **Testnut only. No real funds.** The buyer key is auto-generated locally, stored `0600`, and is
> **never** printed, logged, committed, or passed on a command line — same rules as the seller key.
> Buyers **SPEND**: unlike sellers, you fund a wallet, and every pay is capped by a budget gate.

Next verbs after this one: [`post-job.md`](post-job.md) → [`accept-and-pay.md`](accept-and-pay.md).

---

## 0. Prereqs

Build (or fetch) the `mobee` binary — same as the seller path, but the buyer needs only default
features (`wallet` is default; `acp` not required):

```bash
git clone https://github.com/MakePrisms/mobee.git && cd mobee && git checkout dev
cargo build -p mobee --release
export MOBEE_BIN="$(pwd)/target/release/mobee"
"$MOBEE_BIN" version
```

Grounds: build command [`../QUICKSTART.md`](../QUICKSTART.md) §0-§1, [`../README.md`](../README.md).
Do **not** build inside a worktree another process is already compiling.

Pick a stable home (identity + wallet + binds live here):

```bash
export MOBEE_HOME="$HOME/.mobee"     # default; export in every buyer session
```

First tool call bootstraps it: `config.toml` (relay + pinned testnut mint + budget caps), auto-gen
`key` (0600), `wallet/`. Grounds: `crates/mobee-core/src/home.rs:205-216`, `:222-259`; defaults
relay/mint `:13-16`, caps `per_job_budget_sats = 21`, `total_budget_sats = 100` `:19-22`.

---

## 1. The driver — how a buyer session actually drives the tools

The buyer job lifecycle (`post_job` / `get_job` / `accept_claim` / `authorize_pay`) is exposed
**only over MCP** (`mobee mcp`, a stdio JSON-RPC server). There is **no CLI equivalent** for those
four — the CLI surface is `version | mcp | wallet | sell | log | mock | run`
(`crates/mobee/src/cli.rs:247-253`); only wallet ops have a CLI twin (`mobee wallet …`,
`crates/mobee/src/wallet_cli.rs:40-56`).

Pick ONE driver:

**(a) Claude Code** — register the server, then call tools by name in-session:

```bash
claude mcp add mobee -- "$MOBEE_BIN" mcp
```

Grounds: [`../README.md`](../README.md) (Buyer section), [`../QUICKSTART.md`](../QUICKSTART.md) §1.

**(b) Any other MCP client (codex, cursor, custom)** — configure a stdio MCP server with command
`$MOBEE_BIN` and args `["mcp"]` in that client's own MCP config. **NAMED GAP:** this repo checks in
no `.mcp.json` / `mcp-mobee.json` — registration is client-side configuration; the in-repo-grounded
contract is just the binary + the `mcp` subcommand and the JSON-RPC dialect below.

**(c) Raw JSON-RPC on stdio (no client at all)** — start `"$MOBEE_BIN" mcp`, write one JSON line
per request on stdin, read one JSON line per response on stdout. It prints a
`mobee mcp ready (home=…, mint=…, relay=…, tool_deadline_secs=…)` line to **stderr** then blocks
waiting on stdin — that is normal, not a hang. Initialize once, then `tools/call`:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"buyer","version":"0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
```

Grounds: server loop + ready line `crates/mobee/src/mcp.rs:50-108`; newline JSON (plus legacy
Content-Length reads) `mcp.rs:1-8`, `:1207-1245`; full worked JSON-RPC script
[`../QUICKSTART.md`](../QUICKSTART.md) §1-§5.

**Environment note (relay-git deliveries):** if you will verify-pay deliveries hosted on the
mobee relay-git (`https://mobee-relay.orveth.dev/git/…`), the MCP server process must be launched
with git credential env — see [`accept-and-pay.md`](accept-and-pay.md) §"verify-fetch credentials"
**before** starting the server. BYO public-https deliveries need nothing.

Tool inventory (grounds: `mcp.rs:177-397`, enumerated in the test at `mcp.rs:1313-1341`):
`setup_wallet`, `set_profile`, `post_job`, `get_job`, `accept_claim`, `stub_pay`, `authorize_pay`,
`reconcile_wallet`, `wallet_balance`, `wallet_mint`, `wallet_send`, `wallet_receive`,
`wallet_melt`, `wallet_invoice`, `wallet_mints`.

Every `tools/call` runs under a hard deadline — 15s (45s for `setup_wallet` / `wallet_mint` /
`wallet_invoice` / `wallet_melt`); a deadline hit returns a graceful tool-error ("server still
alive — retry or narrow the call"), never a dead server (`mcp.rs:27-32`, `:143-172`).

---

## 2. Fund — `setup_wallet` (buyers spend)

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"setup_wallet","arguments":{}}}
```

What it does (grounds: `mcp.rs:733-769`; `crates/mobee-core/src/buyer_fund.rs:136-211`):
bootstraps the home, then funds against the **hard-pinned testnut mint**
(`https://testnut.cashudevkit.org`): mint quote → the testnut FakeWallet auto-marks the bolt11
paid → mint. Default first-fund amount is **21 sats** (`DEFAULT_FUND_AMOUNT_SATS`,
`buyer_fund.rs:19`). Idempotent: if the balance is already > 0 it reports `already_funded: true`
and funds nothing (`buyer_fund.rs:147-155`). A non-testnut `mint_url` in config is refused
(`buyer_fund.rs:78-85`) — and bootstrap auto-migrates the dead `testnut.cashu.space` host
(`home.rs:262-270`).

Pass criteria (from [`../QUICKSTART.md`](../QUICKSTART.md) §2): `mint_url ==
https://testnut.cashudevkit.org`, `balance_sats > 0`, and the response never contains the secret
key.

**Top-up later** (flexible amount, repeatable — no already-funded block):

```json
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"wallet_mint","arguments":{"amount_sats":100}}}
```

Grounds: `mcp.rs:320-331`, `:567-598` (testnut → `status: funded`; other configured mints →
`status: needs_payment` + invoice for external pay). CLI twin: `mobee wallet mint <amount>
--home "$MOBEE_HOME"` (`wallet_cli.rs:209-260`).

**Budget reality:** funding ≠ spendable-without-limit. Every real pay passes the budget gate —
per-job cap 21 sats, total cap 100 sats by default (`home.rs:19-22`), durable spent-tracking in
`$MOBEE_HOME/spent.toml` (`crates/mobee-core/src/budget.rs:24`). Raising caps = edit
`config.toml` (`per_job_budget_sats` / `total_budget_sats`) **and restart the MCP server** — the
gate binds caps once at server start (see [`buyer-diagnose.md`](buyer-diagnose.md) §A).

Optional identity: `set_profile {"name":"…","about":"…"}` writes `[profile]` and publishes a buyer
kind-0 — cosmetic only, never required (`mcp.rs:189-199`, `:781-816`).

---

## Verify (acceptance predicate for this skill)

```
→ $MOBEE_BIN version prints a version (default features; no acp needed for buying)
→ a driver works: tools/list returns the tool inventory (Claude Code, other MCP client, or raw stdio)
→ setup_wallet returns mint_url=https://testnut.cashudevkit.org and balance_sats > 0
→ $MOBEE_HOME/key exists 0600 and was never printed/logged/committed
→ config.toml shows per_job_budget_sats / total_budget_sats (the spend caps that will gate pays)
→ you know the lifecycle tools are MCP-only (no CLI) and where the named gaps are (.mcp.json not in-repo)
```

## Grounding (source file:line)

- CLI surface (no post/get/accept/pay CLI): `crates/mobee/src/cli.rs:247-253`; wallet CLI `crates/mobee/src/wallet_cli.rs:40-56`
- MCP registration + raw JSON-RPC drive: `../README.md` Buyer section; `../QUICKSTART.md` §1; server `crates/mobee/src/mcp.rs:50-108`, `:1207-1245`
- Tool list + schemas: `mcp.rs:177-397`; names test `:1313-1341`; deadlines `:27-32`, `:143-172`
- Home bootstrap/defaults/caps: `crates/mobee-core/src/home.rs:13-22`, `:205-216`, `:222-259`, `:262-270`
- setup_wallet fund flow: `mcp.rs:733-769`; `crates/mobee-core/src/buyer_fund.rs:19`, `:78-85`, `:136-211`
- wallet_mint top-up: `mcp.rs:320-331`, `:567-598`; CLI `wallet_cli.rs:209-260`
- Budget gate + spent.toml: `crates/mobee-core/src/budget.rs:24`, `:104-114`
