# seller-diagnose — failure catalog (symptom → cause → fix)

**One operational verb: given a misbehaving seller, name the cause and apply the fix.** This is the
kit's crown. Each entry is concrete and, where possible, machine-checkable. Harness-neutral.

Assumes `MOBEE_BIN` / `MOBEE_HOME` set as in [`run-seller.md`](run-seller.md), and that you tee the
daemon's stderr to `$MOBEE_HOME/sell.log`. **Assert from the log, not terminal scrollback.**

Quickest first cut:

```bash
tail -n 40 "$MOBEE_HOME/sell.log"        # what did the daemon last say?
pgrep -af "mobee sell" || echo "not running"
```

---

## A. "The daemon is running but never claims a cheap offer" — rate gate / targeting

**Cause.** Two admission rules run before any claim (grounds:
`crates/mobee-core/src/seller.rs:81-108`):
1. **Claim floor:** the daemon claims only when `offer.amount >= rate_sats`. A below-rate offer is
   refused.
2. **Targeted-only by default:** it claims only offers `#p`-tagged to its own pubkey. An
   untargeted/open-pool offer is refused unless `claim_open_pool = true`.

**Why it can look "silent."** A *targeted* offer below rate produces a logged skip:
`seller skip offer <id>: rate-gate: offer amount N sat below seller rate_sats M`
(`seller_daemon.rs:315`, reasons `:205-222`). But an *untargeted* offer with open-pool OFF produces
**no log line at all** — the relay subscription is targeted-only, so the offer is never even
delivered to the daemon (`seller_daemon.rs:1210-1230`). That absence is the "silent no-claim".

**Fix.**
```bash
grep "seller skip offer" "$MOBEE_HOME/sell.log" | tail            # any logged rate-gate skips?
grep -E "rate_sats|claim_open_pool" "$MOBEE_HOME/config.toml"     # what did the daemon start with?
```
- Below-rate: lower `--rate-sats` (but keep `>= 2` to net positive; `1` is dust and refused up
  front — [`../SELLER-QUICKSTART.md`](../SELLER-QUICKSTART.md) §7).
- No log at all for an open offer: you are targeted-only. Relaunch with `--claim-open-pool` to claim
  untargeted offers. Then **restart** (config is read at startup — see §J).

---

## B. "Daemon is up but it ignores offers that already existed" — the backfill window

**How offer pickup works.** The targeted filter (`#p == self`) backfills stored offers addressed
to you in full. The **open-pool** (untargeted) filter backfills a bounded window:
**`offer_backfill_secs`** in `config.toml` (default **1200** = 20 min) widens its `since` to
`now − N` with a flood cap (`limit(500)`), so a daemon that starts AFTER an open offer was posted
still claims it if the offer is within the window. `offer_backfill_secs = 0` = live-only
(`since(now)` + `limit(0)` — no stored open offers).

**Money-safety on backfilled offers (always-on, not window side-effects):** an offer past its own
deadline is **refused with a logged reason** — never claimed, never given a fresh deadline (this
guard covers targeted history too); an offer already claimed live by another seller, or already
settled/delivered, is skipped with a logged reason (fail-closed on relay-read errors); the rate
gate applies as always.

**If an old open offer still isn't picked up:** it is older than the window (raise
`offer_backfill_secs` and **restart** — config is read at startup, §J), past its deadline (look
for the logged `expired` skip — the buyer must re-post), or already claimed elsewhere (logged
skip). Targeting the offer to your pubkey (`#p`) always backfills regardless of the window.

---

## C. "The agent never starts / EPIPE / dies instantly" — NixOS ACP runtime (claude harness)

**Symptom.** On NixOS (or any non-glibc box) the ACP agent is dead-on-arrival: an immediate EPIPE,
a silent exit, or no session at all. The daemon then fails the job.

**Cause.** The harness's bundled runtime is a `bun`-linked binary that expects `/lib64` (absent on
NixOS), so the ACP adapter cannot exec the underlying Claude Code binary. This is a **harness /
runtime** concern, not a mobee flag.

**Fix.** Export `CLAUDE_CODE_EXECUTABLE` (absolute path to the working `claude` executable) in the
environment that launches `mobee sell`, before starting the daemon. See
[`harness-presets/claude.md`](harness-presets/claude.md). PATH shims alone do not fix it. Verify the
agent actually produced a commit: `ls "$MOBEE_HOME/seller-jobs/<job_id>/"` and check
`seller-run.jsonl` for agent activity.

---

## D. "codex harness refuses or errors" — spend cap or version gate, not mobee

**Symptom.** The codex agent returns refusals or internal errors; it looks like mobee is broken.

**Cause.** Two non-mobee causes dominate: (1) a shared **workspace SPEND CAP** on the codex account
(one cap shared by all seats; only the owner raises it), and (2) a server **version gate** rejecting
an old codex CLI.

**Discriminate before blaming mobee — run a raw `codex exec` outside mobee:**
```bash
codex exec "say hello" 2>&1 | tail -n 20
# - "spend cap" / quota / budget message      → workspace spend cap: owner must raise it
# - version / unsupported-client message       → upgrade the codex CLI, then retry
# - clean "hello"                               → codex is fine; the problem is in the mobee path
```
Grounds (mobee side, so you can rule it out): codex permission options are auto-answered by `kind`,
not by literal string — a regression that once hung the whole turn until the deadline is fixed
(`crates/mobee-core/src/driver/acp_driver.rs:824-832`). See
[`harness-presets/codex.md`](harness-presets/codex.md).

---

## E. "cursor harness won't run" — login, model selection, quota

**Symptoms & fixes** (harness-runtime, not mobee):
- **Not logged in.** `cursor-agent` must be logged in first (`cursor-agent login`). Symptom: auth
  prompt / immediate failure. Fix: log in, then relaunch `mobee sell --agent cursor …`.
- **Wrong/again model.** Model is selected via the adapter's `acp-config.json`, not a mobee flag.
- **Quota exhausted.** Symptom: the agent starts then errors mid-turn with a quota/limit message,
  or refuses to start. Fix: wait for quota reset or switch account/model.

See [`harness-presets/cursor.md`](harness-presets/cursor.md).

---

## F. "A job hangs and burns the whole deadline / retries didn't fire" — ACP idle timeout

**Cause (by design).** The per-job timeout IS the job's **remaining deadline**
(`--job-timeout-secs` → offer deadline → default 600s), and the ACP driver's idle/response wait is
that same window (`seller_daemon.rs:899-908`, `:1009-1015`; `acp_driver.rs:169` waits on
`recv_timeout(idle_timeout)`). Retries only fire while the deadline still has room and the attempt
budget (3) is not spent (`seller_daemon.rs:50`, `:919-940`). Therefore:
- A **HANG** (agent produces no ACP response) consumes the entire remaining window as **one
  attempt**; when it finally times out, the deadline is spent, so **no retry fires**.
- A **fast-fail transient** (e.g. a 529 that errors quickly) leaves deadline room, so the daemon
  retries (up to 3 attempts, all within the deadline).

**Fix.** For agents prone to long turns, set a longer `--job-timeout-secs` on launch (persists to
config; **restart** to apply — §J). A hung agent is not a mobee bug; it is the agent not responding.
Inspect `$MOBEE_HOME/seller-jobs/<job_id>/seller-run.jsonl`.

---

## G. "relay writes refused" / "seller can't receive payment" — NIP-42 auth

**Cause.** mobee-relay requires **NIP-42 authentication for all writes**, and it **p-gates**
payment reads (kind-1059) — an unauthenticated subscription for your gift-wraps is dropped. The
seller authenticates automatically on connect; the load-bearing evidence is the startup banner.
Grounds: `seller_daemon.rs:1140-1157` (p-gate rationale), `:1158-1193` (auth wait),
`:1424-1429` (banner).

**Fix.**
```bash
grep "nip42=" "$MOBEE_HOME/sell.log" | tail -1
# nip42=authenticated → good.
# nip42=no-challenge  → the relay issued no challenge in the window; auth completes on first REQ,
#                       but if this relay p-gates 1059, receive is degraded. Bounce the daemon and
#                       re-check (§J / seller-update.md). A hard AuthenticationFailed is fatal and
#                       the daemon exits — check relay reachability and that the key file is valid.
```

---

## H. Relay-git delivery fails at startup — announce / seed / helper

**Symptom.** Startup exits before "seller daemon online" with one of:
- `git-credential-nostr not found (set MOBEE_GIT_CREDENTIAL_NOSTR or install helper)`
- `mobee-hosted delivery not seeded after NIP-34 announce (ls-remote 404)`

**Cause & fix.**
- Helper missing → install `git-credential-nostr` or set `MOBEE_GIT_CREDENTIAL_NOSTR`
  (see [`onboarding-glue.md`](onboarding-glue.md) §2), or switch to BYO delivery:
  `mobee sell … --git-remote https://…/<public-repo>.git`.
- Seed 404 → relay-git global name collision on the repo id, or the seed side-effect failed; use a
  BYO `--git-remote`, or retry when relay-git is reachable.
Grounds: `crates/mobee/src/sell.rs:131-157` (announce + seed probe), `:379-422`; helper resolution
`crates/mobee-core/src/seller_git.rs:331-351`, `:427-431`.

Also: only `https` / relay-git remotes are accepted; `ssh`/`file`/`ext` are refused
(`seller_git.rs:269`, `:393-501`). A `--git-remote` must be public https.

---

## I. "Payment arrived but wasn't redeemed after a restart" — in-memory binding (money-safe)

**Cause (known limitation, not a safety bug).** The delivered-but-unpaid → payment binding lives
only in the daemon's in-memory `awaiting_payment` list; it is **not journaled**
(`seller_daemon.rs:240-253`). If the daemon restarts in the deliver→pay window (or the 16-slot cap
evicts the oldest), the incoming gift-wrap no longer binds a delivered job, so it is buffered and
never redeemed. On the next startup that job (claim, no receipt, no release) is **RELEASED**.

**Stuck-not-lost.** The buyer's payment is a NIP-17 gift-wrap (kind-1059) addressed to your seller
pubkey; it **stays on the relay** — the sats are not lost, just not auto-redeemed. This forfeits
**revenue, never safety**: no receipt is released and there is no double-pay. Grounds:
[`../meta/PIECE-11-CLAIM-LIFECYCLE.md`](../meta/PIECE-11-CLAIM-LIFECYCLE.md) "Known limitations".

**Fix / mitigation today.**
- Avoid restarting in the deliver→pay window (payment usually lands within seconds of delivery).
- Manual redeem/reconcile: if you hold a raw cashu token, redeem it with
  `mobee wallet receive <token>` (see [`wallet-ops.md`](wallet-ops.md)). **NAMED GAP:** there is no
  in-repo tool to unwrap a *stuck gift-wrap* on the relay back into a redeemable token; the real fix
  — journaling the delivered-unpaid binding so a payment survives a restart — is a named follow-up,
  not yet built.

---

## J. "I edited config.toml but nothing changed" — config is startup-cached (RESTART required)

**Symptom.** You changed a value in `config.toml` (rate, git-remote, open-pool, budget) and the
running daemon keeps using the OLD value. Field evidence: an MCP server kept enforcing a stale
budget cap after the config was raised — the change did not take until the process was restarted.

**Cause.** The daemon reads its config **once, at startup**. `SellerDaemon::open` snapshots
`home.config` (`seller_daemon.rs:256-278`); all announce/discoverability run in `sell.rs` *before*
the run loop starts (`sell.rs:78-192`, loop entry `:189`); the run loop never reloads config
(`seller_daemon.rs:1330-1539`), and delivery reads the config captured at open time
(`seller_daemon.rs:605`). So edits made after launch are invisible to the live process.

**Fix.** **Restart the daemon** to pick up any `config.toml` change — see
[`seller-update.md`](seller-update.md) for a safe bounce (delivered-unpaid survives on-relay;
processing claims are released, not double-run).

```bash
grep -E "rate_sats|git_remote|claim_open_pool" "$MOBEE_HOME/config.toml"   # confirm the edit is on disk
# then bounce the daemon; re-verify "seller daemon online … nip42=authenticated" and the new value
```

> Config **hot-reload** is a NAMED future story — it is not built. Do not design it here; today the
> answer is always "restart to apply".

---

## Restart safety (why bouncing is safe) — piece-11

Restarting a seller is **safe**. On startup the daemon reconciles the journal: any orphaned
in-flight claim (a `claim` with no `receipt` and no `release`) is **RELEASED** — it reneges the
claim rather than resuming/double-running lost in-memory work, and publishes a best-effort kind-3404
so the buyer sees it. Release is durable-first (journaled) and idempotent across repeated restarts.
Grounds: `seller_daemon.rs:469-489`, `:1382-1394`;
[`../meta/PIECE-11-CLAIM-LIFECYCLE.md`](../meta/PIECE-11-CLAIM-LIFECYCLE.md) "Restart-reconcile".

```bash
grep "seller reconcile" "$MOBEE_HOME/sell.log" | tail   # released N orphaned claim(s) on startup
```

---

## Verify (acceptance predicate for this skill)

```
→ for a no-claim: distinguishes below-rate (logged skip) from untargeted-not-subscribed (no log) and names the config fix
→ names the open-pool backfill window (offer_backfill_secs, default 1200, 0=live-only) + the always-on deadline/claimed/settled guards with logged skips
→ names CLAUDE_CODE_EXECUTABLE (NixOS), codex spend-cap+raw-`codex exec` discriminator, cursor login/model/quota
→ explains HANG-consumes-window vs fast-fail-retries and points at --job-timeout-secs
→ ties "can't receive" to nip42= banner; ties relay-git failures to helper/seed/BYO
→ explains payment-after-restart as in-memory binding, stuck-not-lost, money-safe, with the named follow-up
→ config edits require a RESTART (startup-cached); hot-reload flagged as not-built
→ states restart safety (orphans RELEASED, no double-run)
```

## Grounding (source file:line)

- Rate gate / targeting: `crates/mobee-core/src/seller.rs:81-108`; skip logs `crates/mobee-core/src/seller_daemon.rs:205-222`, `:315`
- Backfill window: filters `seller_daemon.rs:1381` (`offer_subscription_filters` — targeted unbounded, open-pool `since(now−N)` + cap); config `home.rs:95-101` (`offer_backfill_secs`, serde default 1200); deadline-expiry refusal `seller_daemon.rs:426` (`DeadlineExpired`, logged)
- ACP idle timeout + retry semantics: `seller_daemon.rs:50`, `:899-908`, `:919-940`, `:1009-1015`; `crates/mobee-core/src/driver/acp_driver.rs:169`; codex kind-answer regression `:824-832`
- NIP-42 auth + p-gate: `seller_daemon.rs:1140-1157`, `:1158-1193`, `:1424-1429`, no-challenge WARN `:1372-1380`
- Relay-git announce/seed/helper: `crates/mobee/src/sell.rs:131-157`, `:379-422`; `seller_git.rs:269`, `:331-351`, `:393-501`, `:427-431`
- Payment-after-restart / in-memory binding: `seller_daemon.rs:240-253`, `:433-446`, `:1083-1099`; `../meta/PIECE-11-CLAIM-LIFECYCLE.md`
- Config startup-cached: `seller_daemon.rs:256-278`, `:605`, `:1330-1539`; `crates/mobee/src/sell.rs:78-192`
- Restart-reconcile (orphan release): `seller_daemon.rs:469-489`, `:1382-1394`; `crates/mobee-core/src/seller.rs:179-223`
