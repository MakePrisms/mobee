# Harness preset: codex

`mobee sell --agent codex` runs OpenAI Codex as an ACP stdio agent via the codex ACP adapter
(`codex-acp` / codex-acp-ng).

## Exact command the daemon resolves

The daemon builds `agent_command` by trying, in order (grounds:
`crates/mobee/src/agent_presets.rs:68-88`):

1. `codex-acp` if found on `PATH` → argv `["codex-acp"]`
2. else a known dogfood build location (forge-internal only; do not rely on it off-box)
3. else, via `npx` → argv `["npx", "-y", "@agentclientprotocol/codex-acp"]`

`--agent codex` resolves it. Power-user equivalent: `--agent-argv codex-acp` (or the npx form).

## Install / prereqs

- `codex-acp` on `PATH`, **or** Node + `npx` (to fetch `@agentclientprotocol/codex-acp`).
- A working `codex` CLI that the adapter drives (see version gate below).

Detection check:

```bash
command -v codex-acp >/dev/null && echo "adapter on PATH" || (command -v npx >/dev/null && echo "npx fallback available")
```

Grounds: `agent_presets.rs:32-37` (`detect_available_agents` codex branch).

## The codex gotcha: refusals/errors are usually NOT mobee

Two non-mobee causes dominate codex failures. **Discriminate before blaming mobee by running a raw
`codex exec` outside the mobee path:**

```bash
codex exec "say hello" 2>&1 | tail -n 20
```

- A **spend-cap / quota / budget** message → a shared **workspace SPEND CAP** on the codex account
  (one cap shared by all seats). Only the account **owner** can raise it. Not a mobee problem.
- A **version / unsupported-client** message → the codex CLI is behind a server **version gate**.
  Upgrade the codex CLI, then retry.
- A clean `hello` → codex is fine; the fault is in the mobee path, so continue in
  [`../seller-diagnose.md`](../seller-diagnose.md).

These are codex-runtime concerns, not grounded in the mobee repo — see the kit report's NAMED GAPS.

**mobee side you can rule out:** codex-acp names its permission options by `kind`
(`allow_once`/`reject_once`/…), not the literal `allow`/`reject` that claude-acp uses. mobee
auto-answers by `kind`; a past regression where it didn't hung the whole `session/prompt` turn
until the job deadline — that is fixed and tested. Grounds:
`crates/mobee-core/src/driver/acp_driver.rs:824-832`.

## Env vars

- Whatever your `codex` CLI already needs to authenticate (API key / login) in the environment that
  launches `mobee sell`.

## Launch

```bash
export MOBEE_HOME="$HOME/.mobee"
codex exec "say hello" >/dev/null 2>&1 && echo "codex reachable"   # discriminator first
"$MOBEE_BIN" sell --non-interactive --agent codex --rate-sats 2 2>&1 | tee "$MOBEE_HOME/sell.log"
```

## Verify (harness-specific)

```bash
grep "agent preset=codex" "$MOBEE_HOME/sell.log"      # resolved preset + argv0
grep -q "seller daemon online" "$MOBEE_HOME/sell.log" && grep "nip42=" "$MOBEE_HOME/sell.log" | tail -1
# First claimed job: check $MOBEE_HOME/seller-jobs/<job_id>/seller-run.jsonl for agent activity + a commit.
```

Grounds: preset/argv0 log line `crates/mobee/src/sell.rs:356-363`; online+auth
`crates/mobee-core/src/seller_daemon.rs:1424-1429`. The kind-6109 usage block tags codex jobs
`harness=codex-acp-ng`, `usage_transport=acp-native` (`seller_daemon.rs:860-864`).

## Grounding (source file:line)

- Resolution: `crates/mobee/src/agent_presets.rs:68-88`; detection `:32-37`
- Permission-by-`kind` (no-hang) regression test: `crates/mobee-core/src/driver/acp_driver.rs:824-832`
- Harness family label `codex-acp-ng` / `acp-native`: `crates/mobee-core/src/seller_daemon.rs:860-864`
- Preset/argv0 startup log: `crates/mobee/src/sell.rs:356-363`
