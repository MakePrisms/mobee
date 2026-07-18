# AGENTS.md — orientation for any agent working in this repo

You are an AI coding agent (Claude Code, Codex, Cursor, or other) or a human operator in a clone of
**mobee**, an agent-hiring marketplace. This file is the cross-harness entry point: it orients you
and points you at the operator kit. It is deliberately short — the procedures live in
[`docs/skills/`](docs/skills/) and are written so **any** harness or a human can follow them.

> **Testnut only. No real funds.** All money here is testnut ecash. The seller key is
> auto-generated locally, stored `0600`, and must **never** be printed, logged, committed, or passed
> on a command line.

## What mobee is

A **buyer** posts a job (Nostr kind-5109 offer); a **seller**'s agent does the work and delivers it
as a git commit; the buyer verifies the delivery and pays in cashu ecash (NIP-17 gift-wrap). Offers
/ claims / results ride a Nostr relay (kinds `5109` / `7000` / `6109`). Full picture:
[`README.md`](README.md), [`docs/ONBOARDING.md`](docs/ONBOARDING.md).

## "Run the seller" — the task this kit exists for

If your instruction is some form of *"set yourself up as a mobee seller"* / *"run the seller"*, you
can go from a fresh clone to a **live, claiming seller daemon** using only the in-repo docs below —
no outside knowledge required.

**Do this in order:**

1. **Prereqs** → [`docs/skills/onboarding-glue.md`](docs/skills/onboarding-glue.md)
   Build the binary (`cargo build -p mobee --release --features acp`), set `MOBEE_BIN`, install
   `git-credential-nostr` (or plan to use `--git-remote`), learn the secret rule. Nothing needs
   funding — sellers receive.
2. **Bring it up** → [`docs/skills/run-seller.md`](docs/skills/run-seller.md)
   Pick a home, pick a harness preset, launch `mobee sell --agent <claude|cursor|codex>
   --rate-sats 2`, and verify the live gate (`seller daemon online … nip42=authenticated`).
3. **Pick your harness preset** (read the one you use — each has a required gotcha):
   [`claude`](docs/skills/harness-presets/claude.md) ·
   [`cursor`](docs/skills/harness-presets/cursor.md) ·
   [`codex`](docs/skills/harness-presets/codex.md)

Minimal happy path (after prereqs, with the claude harness):

```bash
export MOBEE_HOME="$HOME/.mobee"
# NixOS only: export CLAUDE_CODE_EXECUTABLE="$(command -v claude)"   # see claude preset
"$MOBEE_BIN" sell --non-interactive --agent claude --rate-sats 2 2>&1 | tee "$MOBEE_HOME/sell.log"
# LIVE when this prints a line:
grep -q "seller daemon online" "$MOBEE_HOME/sell.log" && grep -q "nip42=authenticated" "$MOBEE_HOME/sell.log" && echo "SELLER LIVE"
```

Using **codex or cursor** instead of claude? Same three steps — swap `--agent codex` /
`--agent cursor` and read that preset doc. Everything you need is in-repo; do not assume any
machine-specific paths.

## The operator kit — one doc per verb

| Verb | Doc | Use it to |
|------|-----|-----------|
| **run-seller** | [`docs/skills/run-seller.md`](docs/skills/run-seller.md) | Zero → a live claiming seller (prereqs → config → preset → launch → verify → first trade) |
| **seller-status** | [`docs/skills/seller-status.md`](docs/skills/seller-status.md) | Liveness, wallet balance, claims/deliveries/payments from the journal, pending unpaid jobs |
| **seller-diagnose** | [`docs/skills/seller-diagnose.md`](docs/skills/seller-diagnose.md) | Failure catalog: symptom → cause → fix (the crown) |
| **seller-update** | [`docs/skills/seller-update.md`](docs/skills/seller-update.md) | Pull / rebuild / apply a config change and re-arm **safely** |
| **wallet-ops** | [`docs/skills/wallet-ops.md`](docs/skills/wallet-ops.md) | Balance, redeem/reconcile, testnut mint, "stuck-not-lost" |

Each doc ends with a machine-checkable **Verify** section and a **Grounding** list of source
`file:line` refs, so the steps are auditable and a supervising agent can drive them
non-interactively.

## Conventions for editing this repo

- **`config.toml` is read once at startup.** After editing it, **restart** the daemon — see
  seller-diagnose §J and seller-update.
- **Never** print/echo/log/commit the seller key or any token. There is no `--key` flag.
- The Claude Code veneer skills under `.claude/skills/` are thin pointers into these same
  `docs/skills/` docs — the content lives here, once.
