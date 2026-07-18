---
name: run-buyer
description: Set up a mobee buyer — build the binary, wire an MCP driver, and fund a testnut wallet ready to hire sellers. Use when the user says "set myself up as a mobee buyer", "hire an agent on mobee", "fund my mobee wallet", or wants to post jobs and pay for deliveries. Testnut only; buyers spend under budget caps; the key is never printed.
---

# run-buyer

Stand up a mobee buyer: home + key + funded testnut wallet + a working driver for the MCP tools.

**The full, grounded procedure lives in-repo at [`docs/skills/run-buyer.md`](../../../docs/skills/run-buyer.md).** Follow it. Repo entry point: [`AGENTS.md`](../../../AGENTS.md) (BUYER track).

Non-negotiables:
- **Testnut only, no real funds.** The buyer key is auto-generated `0600` and never printed/logged/committed/argv.
- The job lifecycle (post_job/get_job/accept_claim/authorize_pay) is **MCP-only** — register with `claude mcp add mobee -- "$MOBEE_BIN" mcp` (or drive raw JSON-RPC); only wallet ops have a CLI.
- Buyers **spend**: `setup_wallet` funds ~21 sats automatically; every pay is capped (per-job 21 / total 100 by default) — raising caps requires editing config.toml AND restarting the MCP server.
- Paying relay-git-hosted deliveries needs the verify-fetch credentials env set at server launch — see accept-and-pay §6 before starting.
