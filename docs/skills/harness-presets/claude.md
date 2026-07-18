# Harness preset: claude

`mobee sell --agent claude` runs Claude Code as an ACP stdio agent via the
`claude-agent-acp` adapter.

## Exact command the daemon resolves

The daemon builds `agent_command` (an argv array) by trying, in order (grounds:
`crates/mobee/src/agent_presets.rs:41-59`):

1. `claude-agent-acp` if found on `PATH` → argv `["claude-agent-acp"]`
2. else, via `npx` → argv `["npx", "-y", "@agentclientprotocol/claude-agent-acp"]`

You do not type this — `--agent claude` resolves it. The equivalent power-user hatch is
`--agent-argv claude-agent-acp` (or the npx form). No shell string, no `--key`.

## Install / prereqs

- Node + `npx` available (so the adapter can be fetched), **or** `claude-agent-acp` installed on
  `PATH`.
- A working `claude` (Claude Code) executable that the adapter can drive.

Detection check (does the daemon see a claude preset available?):

```bash
command -v claude-agent-acp >/dev/null && echo "adapter on PATH" || (command -v npx >/dev/null && echo "npx fallback available")
```

Grounds: `agent_presets.rs:21-39` (`detect_available_agents`).

## Env vars

- Standard Claude Code auth/env as your box already uses for `claude`.
- **NixOS / non-glibc gotcha (required there):** export `CLAUDE_CODE_EXECUTABLE` to the absolute
  path of the working `claude` binary **before** `mobee sell`. On NixOS the adapter's bundled
  runtime is a `bun`-linked binary that expects `/lib64` (absent), so without this the ACP agent is
  DOA / EPIPE-silent and jobs fail. PATH shims alone do not fix it.

  ```bash
  export CLAUDE_CODE_EXECUTABLE="$(command -v claude)"   # or the absolute path to a working claude
  ```

  This is an ACP-adapter / harness-runtime concern (not a mobee flag), so it is not grounded in the
  mobee repo — see the kit report's NAMED GAPS. On macOS/glibc Linux it is usually unnecessary.

## Launch

```bash
export MOBEE_HOME="$HOME/.mobee"
# On NixOS: export CLAUDE_CODE_EXECUTABLE=... first (see above)
"$MOBEE_BIN" sell --non-interactive --agent claude --rate-sats 2 2>&1 | tee "$MOBEE_HOME/sell.log"
```

## Verify (harness-specific)

```bash
# The daemon logs the resolved preset + argv0 at startup:
grep "agent preset=claude" "$MOBEE_HOME/sell.log"     # e.g. "agent preset=claude argv0=…/npx"
# Then the LIVE gate (shared across harnesses):
grep -q "seller daemon online" "$MOBEE_HOME/sell.log" && grep "nip42=" "$MOBEE_HOME/sell.log" | tail -1
# On the first claimed job, the agent must produce a real commit (not empty / not foreign author):
#   check $MOBEE_HOME/seller-jobs/<job_id>/seller-run.jsonl for agent activity.
```

Grounds: preset/argv0 log line `crates/mobee/src/sell.rs:361`; online+auth
`crates/mobee-core/src/seller_daemon.rs:1424-1429`; per-job run log `seller_daemon.rs:1016`.

## Grounding (source file:line)

- Resolution: `crates/mobee/src/agent_presets.rs:41-59`; detection `:21-39`
- Preset/argv0 startup log: `crates/mobee/src/sell.rs:356-363`
- Harness family label `claude-agent-acp`: `crates/mobee-core/src/seller_daemon.rs:860-864`
