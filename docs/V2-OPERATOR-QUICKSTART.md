# V2 Operator Quickstart — standing up a mobee v2 seller

Fresh-operator guide for this tree: zero → a live, claiming `mobee sell` daemon speaking the
**v2 protocol** (kinds `3400`–`3405` + `30340`). Every statement below is grounded in this
repo's source; grounding refs are given as `file:line`.

> **Testnut only by default.** All money is testnut ecash unless you deliberately flip the
> real-money switch (`allow_real_mints`, below). The seller key is auto-generated, stored
> `0600`, and must never be printed, logged, committed, or passed on argv — there is no
> `--key` flag, and one is actively refused (`crates/mobee/src/sell.rs:474-479`).

## 1. Build

```bash
cargo build --release -p mobee --features acp,wallet
```

- **`acp` is REQUIRED for agent runs and is NOT in the default feature set**
  (`crates/mobee/Cargo.toml:9-12` — `default = ["wallet"]`, `acp` is separate). Without it the
  binary builds and the daemon boots, but every job execution fails closed with
  `DaemonError::AcpRequired` (`crates/mobee-core/src/seller_daemon.rs:2161-2170`). A seller
  built without `acp` claims nothing useful — always build with it.
- `wallet` is a default feature, so naming it is redundant but harmless; `mobee sell` and
  `mobee doctor` both refuse to run without it (`crates/mobee/src/sell.rs:53-61`,
  `crates/mobee/src/doctor.rs:292-300`).

The binary lands at `target/release/mobee`. Export it for the snippets below:

```bash
export MOBEE_BIN="$PWD/target/release/mobee"
```

## 2. Fresh key + home

The seller home is `~/.mobee`, or `$MOBEE_HOME` when set
(`crates/mobee-core/src/home.rs:549-560`). First run of any `mobee` command bootstraps it
(`home.rs:566-603`):

- `config.toml` — written with working defaults (relay `wss://mobee-relay.orveth.dev`, mint
  `https://testnut.cashudevkit.org`, `home.rs:14-17`).
- `key` — a fresh 32-byte hex secret, created `0600`, never echoed
  (`home.rs:752-777`). An existing key with too-open permissions is re-chmod'd to `0600` or
  the boot is refused (`home.rs:706-734`).
- `wallet/` — empty wallet dir.

Bootstrap is idempotent: existing config and key are preserved on relaunch.

## 3. Minimal `config.toml`

`mobee sell` writes the `[seller]` section for you on first run (§5); this is what the fields
mean if you author or audit it by hand. All fields ground in `SellerConfig`
(`home.rs:70-104`) and `MobeeConfig` (`home.rs:406-461`).

```toml
relay_url = "wss://mobee-relay.orveth.dev"
accepted_mints = ["https://testnut.cashudevkit.org"]
per_job_budget_sats = 21      # buyer-side caps; unused by the seller path
total_budget_sats = 100
# allow_real_mints = false    # default — see below

[seller]
agent = "claude"              # optional preset label: claude | cursor | codex
agent_command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
rate_sats = 2                 # claim floor: offers below this are ignored
git_remote = "https://mobee-relay.orveth.dev/git/<seller-pubkey-hex>/m<first-16-hex>.git"
```

- **`agent` / `agent_command`** — `agent` is a display/rediscovery label; `agent_command` is
  what actually launches the ACP agent, and it **must be an argv array** — a TOML string is
  refused at parse ("no-shell by construction", `home.rs:66-73,356-395`). Presets resolve to
  argv for you: `claude` → `claude-agent-acp` on PATH, else
  `npx -y @agentclientprotocol/claude-agent-acp`; similarly `cursor` (`cursor-agent acp`) and
  `codex` (`codex-acp`) (`crates/mobee/src/agent_presets.rs:96-139`). Custom presets go in an
  `[agents.<name>] argv = [...]` table and override same-named built-ins
  (`agent_presets.rs:18-42`).
- **`rate_sats`** — the seller's rate floor in sats; also advertised in the heartbeat's
  `rate` tag (§6).
- **`git_remote`** — where deliveries are pushed. When absent/empty it **defaults to the
  seller's self-owned relay-git namespace**
  `https://mobee-relay.orveth.dev/git/<pubkey>/m<first16(pubkey)>.git`
  (`crates/mobee/src/sell.rs:242-254`, `home.rs:325-336`), logged as
  `git_remote defaulting to relay-git …`. For a relay-git remote the daemon does a NIP-34
  announce **before** any push and then probes that the repo was actually seeded — a global
  name collision would otherwise 404 the first push (`sell.rs:134-160,394-425`).
- **`accepted_mints`** — the seller's payment accept policy; must be non-empty at boot
  (`seller_daemon.rs:355-359`). The first entry is the mint advertised first and the
  buyer-side default mint (`home.rs:405-417`).
- **`allow_real_mints` (default `false`)** — the real-money switch (issue #49). At the
  default `false`, boot fail-closes unless every `accepted_mints` entry is on the
  testnut/dev allow-list (exactly `https://testnut.cashudevkit.org` today); at `true` any
  well-formed `https://` mint is admitted and **real sats can move**. It flips only the
  allow-list check — every other money gate (creq membership, redeem guard, dust guard,
  budget caps, co-signatures) is unchanged (`home.rs:428-433,485-501`,
  `seller_daemon.rs:360-371`).

Useful optional knobs (all default sensibly when absent): `[seller] claim_open_pool`
(default `false` — targeted offers only), `offer_backfill_secs` (default `1200`; `0` =
live-only, targeted offers always backfill), `job_timeout_secs`, and the
`[seller_heartbeat]` section (`enabled = true`, `interval_secs = 300` by default,
`home.rs:205-246`).

**`config.toml` is read once at startup** — restart the daemon after editing it.

## 4. Environment variables

- **`MOBEE_HOME`** — home override; defaults to `~/.mobee` (`home.rs:549-560`).
- **`NOSTR_PRIVATE_KEY` — NOT needed by the seller in this tree.** The daemon's identity is
  the `$MOBEE_HOME/key` file, read in-process (`seller_daemon.rs:372-375`); nothing in
  `crates/` reads `NOSTR_PRIVATE_KEY`. Its historical role was feeding the external
  `git-credential-nostr` helper, which is now optional (next bullet). Only export it if you
  drive raw `git` against relay-git by hand with that helper — and then only on the child
  process env, never argv.
- **git-credential-nostr and system `git` are NO LONGER required** (issue #55). Every
  seller/buyer git leg — delivery push, base fetch, ls-remote probe — runs **in-process via
  libgit2** with NIP-98 auth signed in-process from the seller key; there is no system-git
  fallback and no config knob to select one (`home.rs:290-292`,
  `crates/mobee-core/src/git_transport.rs`, `sell.rs:399-404`). `mobee doctor` accordingly
  reports the git version and credential-helper checks as informational PASSes even when
  both are absent (`crates/mobee/src/doctor.rs:108-147,170-185`).
- **`CLAUDE_CODE_EXECUTABLE`** — for the `claude` preset on NixOS/non-glibc boxes: export
  the absolute path of a working `claude` binary before `mobee sell`, or the ACP adapter's
  bundled runtime is DOA (expects `/lib64`) and jobs fail silently. This is an ACP-adapter
  concern, not a mobee flag (`docs/skills/harness-presets/claude.md:31-44`).

  ```bash
  export CLAUDE_CODE_EXECUTABLE="$(command -v claude)"
  ```
- Test-only overrides: `MOBEE_HEARTBEAT_INTERVAL_SECS` / `MOBEE_HEARTBEAT_ENABLED`
  (`crates/mobee-core/src/heartbeat.rs:25-31`) and
  `MOBEE_SELLER_BOOT_PUSH_PREFLIGHT=0` to skip the boot push probe (`home.rs:262-267`).

## 5. Launch

First run (the only required choices are the agent and the rate):

```bash
export MOBEE_HOME="$HOME/.mobee"
"$MOBEE_BIN" sell --non-interactive --agent claude --rate-sats 2 2>&1 | tee "$MOBEE_HOME/sell.log"
```

This resolves the preset, defaults `git_remote` to relay-git, persists `[seller]` to
`config.toml`, and starts the daemon (`sell.rs:1-8,199-357`). Subsequent launches are
zero-prompt: plain `mobee sell` relaunches from config. Open-pool claiming is opt-in via
`--claim-open-pool`.

## 6. The v2 protocol surface

All v2 kinds live in one mobee-owned block; the v1 DVM kinds (`5109`/`6109`/`7000`) are gone
(`crates/mobee-core/src/kinds.rs`):

| Kind | Object | Author |
|---|---|---|
| `3400` | Receipt (co-signed settlement) | buyer + seller |
| `3401` | Offer | buyer |
| `3402` | Claim — carries the invoice | seller |
| `3403` | Result (typed delivery) | seller |
| `3404` | Feedback (progress / error / refusal) | seller |
| `3405` | Award (claim selection) | buyer |
| `30340` | Seller heartbeat, addressable, `d="mobee-seller"` | seller |

- Every mobee event carries the namespace tag `["t", "mobee"]`; parsers refuse events
  without it (`crates/mobee-core/src/gateway.rs:7,255-261,605-606`). Trade events also carry
  the protocol version tag `["v", "2"]` (`gateway.rs:11,610`).
- **The claim is the invoice**: the kind-3402 claim carries the **seller-authored NUT-18
  payment request** as a `["creq", "creqA…"]` tag, built with `creq::build_seller_creq` and
  read back by buyers with `creq::parse_creq` (`gateway.rs:365-385,644`). The receipt binds
  it via a `creq-hash` tag — SHA-256 over the full `creqA…` string (`gateway.rs:519-527`).
- The **heartbeat** (kind `30340`) is parameterized-replaceable — hence a `30xxx` kind, not
  `34xx` — republished every ~300 s with tags `d=mobee-seller`, `t=mobee`, `accepting` (y/n,
  flips while a job is in flight), `queue_depth`, `rate`, and `protocol_versions`
  (`heartbeat.rs:17-99`). Consumers must resolve it by `(pubkey, d)`, never by event id.

## 7. Smoke checklist

Run through these in order; all greps are against the launch log from §5.

1. **NIP-42 authed line** — the daemon is live and can receive p-gated payments only after
   relay auth (`seller_daemon.rs:2647,2795`):

   ```bash
   grep "seller daemon online" "$MOBEE_HOME/sell.log" | grep "nip42=authenticated"
   ```

   `nip42=no-challenge` is a WARN state: the relay issued no challenge at connect; if it
   p-gates kind-1059, payment receive may be dead (`seller_daemon.rs:2707-2715`).

2. **Heartbeat lines** — enabled-by-default cadence plus at least one publish
   (`seller_daemon.rs:2893-2911,2098`):

   ```bash
   grep "seller heartbeat enabled: kind-30340 d=mobee-seller" "$MOBEE_HOME/sell.log"
   grep "seller heartbeat published id=" "$MOBEE_HOME/sell.log"   # appears within interval_secs
   ```

3. **Relay-git delivery ready** (relay-git remotes only, logged at startup,
   `sell.rs:140-160`):

   ```bash
   grep "relay-git NIP-34 announce ok" "$MOBEE_HOME/sell.log"
   grep "relay-git seed probe ok" "$MOBEE_HOME/sell.log"
   ```

4. **`mobee doctor`** — environment self-check; exits `0` when no check FAILs
   (`crates/mobee/src/doctor.rs:1-11,308-359`). It probes, in one pass: git version
   (informational), `git-credential-nostr` presence (informational — not required), relay
   reachability with the same NIP-42 connect+auth sequence the daemon uses, every configured
   mint's `/v1/info`, and that the configured agent preset's argv0 is resolvable:

   ```bash
   "$MOBEE_BIN" doctor && echo DOCTOR-OK
   ```

   Expect `PASS relay reachability — …: connected + NIP-42 authenticated` and
   `PASS agent preset — agent 'claude' resolvable (argv0=…)`.

The startup log also prints the never-echo status line
(`seller starting pubkey=… agent=… rate_sats=… claim_open_pool=… git_remote=…`,
`sell.rs:183-191`) — confirm the pubkey and rate are what you expect. The secret key never
appears in any log line.
