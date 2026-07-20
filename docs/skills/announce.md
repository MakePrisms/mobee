# announce — daemon-native seller lifecycle events → pluggable sinks

**One operational verb: have the `mobee sell` daemon emit a structured JSON event for every
lifecycle transition and pipe it to a pluggable sink command — so a team surface (buzz, Discord,
Slack, or anything you write) shows claims, deliveries, payments, refusals, and failures as they
happen.** Buzz is the first-class target; the sink contract is generic.

This is the **daemon-native** feed. It is a sibling of the log-tailing sidecar
([`buzz-announce.md`](buzz-announce.md) / `scripts/mobee-buzz-sidecar.sh`), which scrapes stderr
and needs no rebuild. The key difference: the daemon hands each sink a **typed JSON event**, so
this feed also carries the **`claimed`** transition — the daemon's first-ever claim signal, which
the log-tailing sidecar cannot see (before this, a claim was journaled silently).

> **Never money-adjacent, never blocking, always fail-soft.** An announce event carries only
> ids/amounts/reasons already public on the relay or in the seller log — never a token, key, or
> NIP-17 plaintext. Each event is dispatched on its OWN detached thread with a bounded wait, so a
> slow, hung, missing, or failing sink can never delay, stall, or crash the daemon. Nothing here
> feeds the pay gate, the journal, or the receipt bind.

---

## When to use

- You run a `mobee sell` daemon (rebuilt from `dev`) and want a first-class, structured activity
  feed — including **claims** — on a team channel, with the daemon itself doing the emitting.
- You want to route seller events somewhere the log-tailing sidecar doesn't reach (a custom
  webhook, a metrics pipe, a bot) by writing a tiny stdin-reading script.

Use the **log-tailing sidecar** ([`buzz-announce.md`](buzz-announce.md)) instead when you **cannot
rebuild** the seller (it attaches to an already-running daemon's stderr, zero code change) — at the
cost of no `claimed` event and coupling to stderr formats.

Do **not** treat the feed as a source of truth for money — events are observability, not the
ledger. For real balances/receipts use [`seller-status.md`](seller-status.md),
[`wallet-ops.md`](wallet-ops.md), and [`verify-receipt.md`](verify-receipt.md).

---

## Config

Add a top-level `[seller_announce]` section to `~/.mobee/config.toml` (absent ⇒ feature OFF, zero
behavior change):

```toml
[seller_announce]
# argv array (no-shell, like agent_command). Empty/absent ⇒ feature OFF.
command = ["/abs/path/to/mobee/sinks/buzz-announce.sh"]
# optional: upper bound (ms) the daemon waits for one sink before killing it. Default 2000.
# Emission is always off the event loop, so this bounds only the sink thread, never the daemon.
timeout_ms = 2000
```

The daemon spawns `command` once per lifecycle transition with the event JSON on the process's
**stdin**. The sink's own environment (e.g. `BUZZ_PRIVATE_KEY`, `DISCORD_WEBHOOK_URL`) is inherited
from the daemon's environment — no secret is ever put in argv.

> Placed top-level as `[seller_announce]` (not nested under `[seller]`) for the same reason as
> `[seller_memory]`: nesting would touch a money-path file. Cosmetic; behavior is identical.

---

## Event schema (JSON on stdin)

One JSON object per invocation, additive-versioned (`v`, currently `1`) like the episode schema —
new fields are only ever added, so a sink written today keeps parsing future events. The envelope
(`v`, `event`, `ts`, `seller_pubkey`) is always present; the rest is per-transition.

| `event` | Fires when | Notable fields |
|---------|-----------|----------------|
| `online` | daemon subscribed + past NIP-42 auth | `relay`, `mint`, `nip42` |
| `claimed` | offer claimed (kind-7000 + journaled) — **new signal** | `job_id`, `buyer_pubkey`, `amount`, `claim_id`, `deadline_unix` |
| `delivered` | result published (kind-6109) | `job_id`, `result_id`, `commit`, `git_remote`, `branch`, `amount` |
| `collected` | kind-1059 redeemed at mint + receipt journaled | `job_id`, `result_id`, `amount_received`, `expected`, `mint` |
| `refused` | offer refused at classify | `job_id`, `reason_code` (machine), `reason`, `amount` (if parsed) |
| `reconcile_released` | orphaned claim released on startup | `job_id`, `liveness`, `reason` |
| `job_failed` | claimed job failed before/at delivery | `job_id`, `reason` |

`reason_code` on `refused` is the stable enumerated `OfferSkip` code (`RateGate`, `NonTestnutMint`,
`DeadlineExpired`, `ContributionUnsupported`, …) — safe to branch on. Example line:

```json
{"v":1,"event":"claimed","ts":1737200001,"seller_pubkey":"abcd…","job_id":"job0011","buyer_pubkey":"buyer99","amount":25,"claim_id":"claimAA","deadline_unix":1737200600}
```

---

## The three shipped sinks (`sinks/`)

Each reads ONE JSON event from stdin, formats it into a one-line human message with `jq`, and is
fail-soft (any error → logged to stderr, swallowed, exit 0). All support `ANNOUNCE_DRY_RUN=1` to
**print** the formatted output instead of sending — use it to verify before wiring live.

| Sink | Target | Required env |
|------|--------|--------------|
| `sinks/buzz-announce.sh` | buzz team chat (**first-class**) | `BUZZ_CHANNEL`, `BUZZ_PRIVATE_KEY` (also `BUZZ_RELAY_URL`, `BUZZ_BIN`) |
| `sinks/discord-webhook.sh` | Discord channel webhook | `DISCORD_WEBHOOK_URL` |
| `sinks/slack-webhook.sh` | Slack incoming webhook | `SLACK_WEBHOOK_URL` |

The buzz sink uses the SAME message shape as the log-tailing sidecar (`[<label>] claimed — …`),
where `<label>` is the first 8 hex of `seller_pubkey`. Keys/URLs are read only from the
environment and never echoed; the buzz body always goes over stdin (`--content -`).

Verify formatting (prints, sends nothing):

```bash
echo '{"v":1,"event":"claimed","ts":1,"seller_pubkey":"abcd1234deadbeef","job_id":"j1","buyer_pubkey":"b1","amount":25,"claim_id":"c1","deadline_unix":100}' \
  | ANNOUNCE_DRY_RUN=1 sinks/buzz-announce.sh
# → [abcd1234] claimed — job_id=j1 buyer=b1 amount=25 sats deadline=100
```

---

## Writing your own sink

The contract is intentionally tiny:

1. Your command receives **one JSON event on stdin** per invocation (one event per process spawn).
2. Parse it (`jq`, or any JSON reader), do your thing, and **exit** — the daemon does not read your
   stdout and kills you at `timeout_ms` if you outlive it.
3. **Be fail-soft**: never block waiting on stdin beyond reading the one object; on any error, log
   and exit cleanly. You must never assume you can slow the daemon — you cannot, but a hung sink
   still wastes a thread and gets killed at the bound.
4. **Never print secrets.** Read them from the environment (inherited from the daemon); never put
   them in argv.

Skeleton:

```bash
#!/usr/bin/env bash
set -uo pipefail
event="$(cat)"                      # one JSON event
kind="$(printf '%s' "$event" | jq -r '.event')"
# ... branch on "$kind", extract fields, deliver ...
exit 0
```

---

## Relationship to the log-tailing sidecar

| | announce (this) | log-tailing sidecar ([`buzz-announce.md`](buzz-announce.md)) |
|---|---|---|
| Source | daemon emits typed JSON | scrapes daemon stderr |
| Rebuild needed | **yes** (dev build) | **no** (attaches to a running seller) |
| `claimed` event | **yes** | no (daemon claims silently in older builds) |
| Coupling | stable JSON schema (`v`) | exact `eprintln!` format strings |
| Wiring | `[seller_announce] command` in config | tail a tee'd logfile |

Prefer announce when you can rebuild; fall back to the sidecar for no-rebuild / already-running
sellers. They can run side by side (announce also still emits the stderr lines the sidecar keys on,
including a new `seller claimed offer …` line).

---

## LIMITS (read before trusting it)

- **Observability, not the ledger.** Amounts are FACE/receipt figures; real wallet balance nets
  the mint fee. Trust [`wallet-ops.md`](wallet-ops.md) / [`verify-receipt.md`](verify-receipt.md).
- **v1 posts under whatever key/URL you give the sink.** No per-seller identity yet — the `[label]`
  prefix (first 8 of the pubkey) is the only in-message discriminator between sellers on one
  channel. Per-seller buzz identities are a future enrollment step.
- **A sink is best-effort and can drop events.** A hung sink is killed at `timeout_ms` and that
  event is lost (logged once on the daemon's stderr). The feed is not a guaranteed-delivery queue;
  the journal/relay are the durable record.
- **One process per event.** High-refusal open-pool sellers can spawn many short-lived sink
  processes; keep the sink cheap. (Refused events skip pure noise — non-offers and dedup re-sees.)

---

## VERIFY (machine-checkable)

```bash
# sinks are syntactically clean
bash -n sinks/buzz-announce.sh sinks/discord-webhook.sh sinks/slack-webhook.sh

# each sink formats a sample event (prints, sends nothing)
for ev in online claimed delivered collected refused reconcile_released job_failed; do
  echo "{\"v\":1,\"event\":\"$ev\",\"ts\":1,\"seller_pubkey\":\"abcd1234deadbeef\",\"job_id\":\"j1\",\"buyer_pubkey\":\"b1\",\"amount\":25,\"claim_id\":\"c1\",\"deadline_unix\":100,\"result_id\":\"r1\",\"commit\":\"oid\",\"git_remote\":\"g\",\"branch\":\"br\",\"amount_received\":24,\"expected\":25,\"mint\":\"m\",\"relay\":\"r\",\"nip42\":\"authenticated\",\"reason_code\":\"RateGate\",\"reason\":\"x\",\"liveness\":\"Expired\"}" \
    | ANNOUNCE_DRY_RUN=1 sinks/buzz-announce.sh
done

# daemon-side unit tests (event shape, capturing sink, hung-sink non-blocking, fail-soft)
cargo test -p mobee-core --locked announce
```

---

## Grounding (source file:line)

- Event type + non-blocking detached dispatch + bounded-wait: `crates/mobee-core/src/announce.rs`.
- Config (`[seller_announce]`, top-level, feature-off default): `crates/mobee-core/src/home.rs`
  (`SellerAnnounceConfig`).
- Emission call sites (one per transition): `crates/mobee-core/src/seller_daemon.rs` — `claimed`
  in `on_offer_event`, `refused` in the skip path, `delivered` in `execute_active_job`,
  `collected` in `try_apply_payment`, `job_failed` in `fail_active`, `reconcile_released` in
  `reconcile_on_startup`, `online` in `run_forever_hooked`.
- Sinks: `sinks/buzz-announce.sh`, `sinks/discord-webhook.sh`, `sinks/slack-webhook.sh`.
- Log-tailing sibling: `docs/skills/buzz-announce.md`, `scripts/mobee-buzz-sidecar.sh`.
