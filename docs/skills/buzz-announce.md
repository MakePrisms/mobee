# buzz-announce — make a running seller visible in buzz team chat

**One operational verb: forward a live `mobee sell` daemon's lifecycle log lines into a buzz
team-chat channel, so the team sees claims, deliveries, payments, and failures as they happen.**
Harness-neutral: any agent or a human can run it. It is a **sidecar** — a separate process that
only reads the seller's logfile. **Zero daemon change; the seller is never touched.**

> **Read-only against the seller.** The sidecar tails the stderr logfile you already tee, holds no
> lock the daemon shares, and swallows its own send failures. It can never stall, crash, or slow
> the seller it watches. It writes nothing but a small offset file next to your log.

Bring the seller up first with [`run-seller.md`](run-seller.md) (it shows how to tee stderr to a
logfile), then run this alongside it.

---

## When to use

- You run a `mobee sell` daemon and want its trades to show up in a shared buzz channel instead of
  only in a private terminal — team visibility with no code change.
- You want a lightweight, honest activity feed (online, delivered, paid, refused, failed) without
  wiring buzz into the daemon itself.

Do **not** use it as a source of truth for money — it forwards *log lines*, which are observability,
not the ledger. For real balances/receipts use [`seller-status.md`](seller-status.md),
[`wallet-ops.md`](wallet-ops.md), and [`verify-receipt.md`](verify-receipt.md).

---

## Env contract

| Var | Required | Default | Meaning |
|-----|----------|---------|---------|
| `SELLER_LOG` | yes | — | Path to the seller stderr logfile (the one you `2>&1 \| tee`). |
| `BUZZ_CHANNEL` | yes | — | buzz channel UUID to post into (`buzz channels list`). |
| `BUZZ_PRIVATE_KEY` | yes | — | Hex key the sidecar posts under. **Never** echoed or logged. |
| `BUZZ_RELAY_URL` | no | `https://buzzrelay.orveth.dev` | buzz relay. |
| `SELLER_LABEL` | no | first 8 hex of the pubkey from the "daemon online" line (`seller` until seen) | Short prefix on each message. |
| `BUZZ_BIN` | no | `/srv/forge/workspaces/buzz/target/release/buzz`, else `buzz` on PATH | The buzz CLI. |
| `SIDECAR_DRY_RUN` | no | unset | `=1` prints each message to stdout instead of sending. |

The key is passed to the buzz CLI purely through the environment and the message body always goes
over **stdin** (`--content -`) — never on a command line — so nothing sensitive lands in `ps` or a
shell history.

---

## Forwarded events

Each line the sidecar matches becomes exactly one buzz message, prefixed with `[<label>]`. The
matchers key off the daemon's **real** `eprintln!` format strings in
`crates/mobee-core/src/seller_daemon.rs` (cited so you can re-check them):

| Event | Message shape | Source (`seller_daemon.rs`) |
|-------|---------------|-----------------------------|
| daemon online | `[l] online — relay=… mint=… nip42=…` | `:1736` |
| delivered / result published | `[l] delivered — result_id=…` | `:1796` |
| payment received (receipt journaled) | `[l] paid — job_id=… amount_received=… sats` | `:1762` (live) / `:1801` (reconcile) |
| payment collected at the mint | `[l] collected — job_id=… amount_received=… expected=…` | `:122-133` (via `:737`) |
| rate-gate refusal surfaced | `[l] refused under-rate — offer=… (amount a < rate_sats r)` | `:594` |
| reconcile released orphaned claim | `[l] reconcile released orphaned claim — job_id=… liveness=… reason=…` | `:1672` / `:1682` |
| seller job failed | `[l] job FAILED — <error>` | `:1813` |

**Rate-safety:** exactly one message per matched line. A byte offset persisted at
`<SELLER_LOG>.sidecar-offset` means a sidecar restart resumes past lines already forwarded instead
of re-announcing the whole log. A rotated/truncated log (shorter than the saved offset) resets to
the top.

---

## Launch

Dry-run first (prints to stdout, sends nothing — use it to confirm your log matches):

```bash
SELLER_LOG="$MOBEE_HOME/sell.log" \
BUZZ_CHANNEL=<uuid> BUZZ_PRIVATE_KEY=<hex> SIDECAR_DRY_RUN=1 \
  scripts/mobee-buzz-sidecar.sh
```

Live, under tmux so it outlives your shell (it follows the log and blocks, like `tail -F`):

```bash
tmux new -d -s mobee-buzz \
  "SELLER_LOG=$MOBEE_HOME/sell.log BUZZ_CHANNEL=<uuid> BUZZ_PRIVATE_KEY=<hex> \
   scripts/mobee-buzz-sidecar.sh"
```

Stop it with `tmux kill-session -t mobee-buzz`. The offset file is left in place, so a relaunch
picks up exactly where it left off.

---

## VERIFY (machine-checkable)

```bash
# Against a fixture (one real line of each event class), dry-run must emit one message per class
# and NOTHING for a `seller skip offer …` or the reconcile summary count line.
SELLER_LOG=./fixture.log BUZZ_CHANNEL=<uuid> BUZZ_PRIVATE_KEY=x SIDECAR_DRY_RUN=1 \
  scripts/mobee-buzz-sidecar.sh
# Re-running it immediately forwards nothing (offset is at EOF) — proves restart de-dup.
```

- `bash -n scripts/mobee-buzz-sidecar.sh` is clean.
- Dry-run emits one `[label] …` line per event class; the label is the first 8 hex of the pubkey.
- The key never appears in stdout or stderr.

---

## LIMITS (read before trusting it)

- **v1 posts under whatever buzz key you give it.** There is no per-seller buzz identity yet — every
  message is attributed to the `BUZZ_PRIVATE_KEY` you pass. Distinct per-seller buzz identities are a
  future enrollment step (mint a key + kind-0 + relay-admit, as keepers are enrolled today). Until
  then, the `[label]` prefix is the only in-message discriminator between sellers on one channel.
- **Log-format coupling.** The matchers are pinned to the exact `eprintln!` strings in
  `seller_daemon.rs` (cited above). If those lines change, the sidecar silently stops forwarding the
  changed event — update the matchers and re-run the fixture dry-run. It fails *quiet* on a format
  drift (a missed line), never loud, by design (it must not affect the seller).
- **No "claim published" event.** The daemon claims an offer *silently* — it journals the claim but
  emits no log line for it (verified: no `eprintln!`/`tracing` claim line exists in
  `seller_daemon.rs`). The earliest positive signal the sidecar can forward is `delivered`
  (`seller published 6109`). Do not read the absence of a "claimed" message as "did not claim".
- **Observability, not the ledger.** Forwarded amounts are the FACE/receipt figures from the log;
  real wallet balance nets the mint fee. Trust [`wallet-ops.md`](wallet-ops.md) /
  [`verify-receipt.md`](verify-receipt.md) for money truth.
- **Byte-offset accounting assumes ASCII log lines** (all current seller lines are). A multi-byte
  line would drift the resume offset slightly; at worst that re-forwards or skips a line across a
  restart — never a daemon effect.

---

## Grounding (source file:line)

- Forwarded event formats: `crates/mobee-core/src/seller_daemon.rs:1736` (online), `:1796`
  (delivered), `:1762`/`:1801` (receipt), `:122-133` + `:737` (collect ok), `:594` (under-rate
  refusal), `:1672`/`:1682` (reconcile released), `:1813` (job failed).
- No claim-published log line: grep of `eprintln!`/`tracing` in `seller_daemon.rs` (only `:1796`
  `seller published 6109` on the positive path).
- buzz send over stdin: `buzz messages send --channel <UUID> --content -` (CLI help).
- Sidecar: `scripts/mobee-buzz-sidecar.sh`.
