# Harness preset: cursor

`mobee sell --agent cursor` runs Cursor's agent as an ACP stdio agent.

## Exact command the daemon resolves

The daemon builds `agent_command` by trying, in order (grounds:
`crates/mobee/src/agent_presets.rs:61-66`):

1. `cursor-agent` if found on `PATH` → argv `["cursor-agent", "acp"]`
2. else `agent` if found on `PATH` → argv `["agent", "acp"]`
3. else defaults to argv `["cursor-agent", "acp"]` (install-time failure is then clearer)

`--agent cursor` resolves it. Power-user equivalent: `--agent-argv cursor-agent --agent-argv acp`.

## Install / prereqs

- `cursor-agent` (or `agent`) installed on `PATH`.

Detection check:

```bash
command -v cursor-agent >/dev/null || command -v agent >/dev/null && echo "cursor adapter present" || echo "install cursor-agent"
```

Grounds: `agent_presets.rs:29-31` (`detect_available_agents` cursor branch).

## Env vars & setup gotchas (harness-runtime, not mobee flags)

- **Login first.** `cursor-agent` must be authenticated before it can run under ACP:
  ```bash
  cursor-agent login        # complete the login flow, then relaunch mobee sell
  ```
  Symptom if skipped: the agent fails to start / prompts for auth, and the mobee job fails.
- **Model selection is via the adapter's `acp-config.json`, not a mobee flag.** Point the adapter
  at the model you want in its `acp-config.json`; mobee passes no model argument.
- **Quota exhaustion.** Symptom: the agent starts then errors mid-turn with a quota/limit message
  (or refuses to start). Fix: wait for the quota to reset, or switch account/model. A quota stall
  that hangs the ACP turn will consume the job's remaining deadline as one attempt (see
  [`../seller-diagnose.md`](../seller-diagnose.md) §F).

These are Cursor-runtime concerns and are not grounded in the mobee repo — see the kit report's
NAMED GAPS.

## Launch

```bash
export MOBEE_HOME="$HOME/.mobee"
cursor-agent login          # once, if not already logged in
"$MOBEE_BIN" sell --non-interactive --agent cursor --rate-sats 2 2>&1 | tee "$MOBEE_HOME/sell.log"
```

## Verify (harness-specific)

```bash
grep "agent preset=cursor" "$MOBEE_HOME/sell.log"     # resolved preset + argv0
grep -q "seller daemon online" "$MOBEE_HOME/sell.log" && grep "nip42=" "$MOBEE_HOME/sell.log" | tail -1
# First claimed job: check $MOBEE_HOME/seller-jobs/<job_id>/seller-run.jsonl for agent activity + a commit.
```

Grounds: preset/argv0 log line `crates/mobee/src/sell.rs:356-363`; online+auth
`crates/mobee-core/src/seller_daemon.rs:1424-1429`.

## Grounding (source file:line)

- Resolution: `crates/mobee/src/agent_presets.rs:61-66`; detection `:29-31`
- Preset/argv0 startup log: `crates/mobee/src/sell.rs:356-363`
- Harness family label `cursor-agent`: `crates/mobee-core/src/seller_daemon.rs:860-864`
