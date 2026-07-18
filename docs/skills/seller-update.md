# seller-update — pull, rebuild, re-arm safely

**One operational verb: move the seller to a newer build (or apply a config change) without losing
money or double-running a job.** Harness-neutral.

Assumes `MOBEE_BIN` / `MOBEE_HOME` set as in [`run-seller.md`](run-seller.md).

---

## When you need this

- A new `dev` build you want to run.
- Any `config.toml` change — the running daemon caches config at startup, so edits require a
  restart to take effect (see [`seller-diagnose.md`](seller-diagnose.md) §J).

---

## What "restart safe" means (read before bouncing)

Restarting a seller mid-life is safe by design (piece-11). Bounce implications by job state:

| Job state at bounce | What happens on restart | Money impact |
|---------------------|-------------------------|--------------|
| **Processing** (agent running, not yet delivered) | The orphaned claim is **RELEASED** (kind-7000), not resumed — v1 does not resume lost in-memory execution | None — money never received; buyer may re-post |
| **Delivered, unpaid** (kind-6109 out, payment not yet redeemed) | The in-memory binding is lost; on restart the job is RELEASED. The buyer's gift-wrap **stays on the relay** (stuck-not-lost) | Revenue forfeiture risk **only if** you bounce in the deliver→pay window — money-safe (no double-pay, no false receipt) |
| **Paid** (receipt journaled) | Terminal; unaffected | None |

So: **bouncing while idle or between trades is free. Avoid bouncing in the brief deliver→pay
window** (payment usually lands within seconds of delivery). Grounds:
[`../meta/PIECE-11-CLAIM-LIFECYCLE.md`](../meta/PIECE-11-CLAIM-LIFECYCLE.md) states; release-on-restart
`crates/mobee-core/src/seller_daemon.rs:469-489`, `:1382-1394`; in-memory binding `:240-253`.

---

## Procedure

### 1. Pick a safe moment

```bash
tail -n 15 "$MOBEE_HOME/sell.log"
# Safe: last line is "seller daemon online …", or a "seller receipt …" (trade closed), or idle.
# Wait: you just saw "seller published 6109 result_id=…" with no following "seller receipt …"
#       → a payment may be in flight; give it a moment before bouncing.
```

### 2. Stop the daemon (graceful)

```bash
pkill -TERM -f "mobee sell"     # SIGTERM; wait for exit
sleep 2
pgrep -af "mobee sell" || echo "stopped"
```

### 3. (Rebuild path) Pull dev and rebuild

> **Do not rebuild inside a worktree another process is already compiling.** One Cargo target dir
> per workspace at a time.

```bash
git -C /path/to/mobee fetch origin && git -C /path/to/mobee checkout dev && git -C /path/to/mobee pull
( cd /path/to/mobee && cargo build -p mobee --release --features acp )
export MOBEE_BIN="/path/to/mobee/target/release/mobee"
"$MOBEE_BIN" version                              # note the new version
"$MOBEE_BIN" sell --bogus 2>&1 | grep -q "Usage:" && echo "sell present"
```

(Config-only change: skip the rebuild; just edit `config.toml`, confirm the edit is on disk, and
relaunch.)

### 4. Re-arm (zero-prompt relaunch reads config.toml)

```bash
"$MOBEE_BIN" sell 2>&1 | tee "$MOBEE_HOME/sell.log"     # bare relaunch; no --agent/--rate-sats needed
```

The seller identity (pubkey), wallet, and journal all persist in `$MOBEE_HOME`, so you resume the
same seller — discoverability, relay-git namespace, and receive address are unchanged.

---

## 5. VERIFY after restart

```bash
# (a) new build actually running:
"$MOBEE_BIN" version

# (b) back to the LIVE gate:
grep -q "seller daemon online" "$MOBEE_HOME/sell.log" && grep "nip42=" "$MOBEE_HOME/sell.log" | tail -1

# (c) any orphans were reconciled (expected if a job was processing at stop time):
grep "seller reconcile" "$MOBEE_HOME/sell.log" | tail

# (d) config change actually took (example: git_remote):
grep "seller starting" "$MOBEE_HOME/sell.log" | tail -1     # echoes agent/rate/claim_open_pool/git_remote
grep -E "rate_sats|git_remote|claim_open_pool" "$MOBEE_HOME/config.toml"
```

The `seller starting pubkey=… agent=… rate_sats=… claim_open_pool=… git_remote=… (never-echo: key
omitted)` line (`crates/mobee/src/sell.rs:180-188`) confirms the effective config the new process is
running — cross-check it against your intended change. It must **not** print the key.

---

## Verify (acceptance predicate for this skill)

```
→ states restart safety per job state (processing→released; delivered-unpaid→stuck-not-lost money-safe; paid→terminal)
→ picks a safe moment (not in the deliver→pay window) using the log
→ stops gracefully, (optionally) rebuilds from dev with --features acp, relaunches zero-prompt
→ verifies: new `mobee version`, "seller daemon online … nip42=authenticated", reconcile line, effective config
→ never prints/leaks the key on restart (the "seller starting" line omits it)
```

## Grounding (source file:line)

- Restart-reconcile / release-on-restart: `crates/mobee-core/src/seller_daemon.rs:469-489`, `:1382-1394`; `../meta/PIECE-11-CLAIM-LIFECYCLE.md`
- Delivered-unpaid in-memory (forfeiture money-safe): `seller_daemon.rs:240-253`; PIECE-11 "Known limitations"
- Zero-prompt relaunch from config: `crates/mobee/src/sell.rs:196-209`
- "seller starting …" effective-config line (key omitted): `sell.rs:180-188`
- Build command + `acp`: `../README.md`, `../SELLER-QUICKSTART.md` §0
