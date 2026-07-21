# onboarding-glue — prerequisites for a fresh box (Mac & Linux)

What a fresh, generic box needs before [`run-seller.md`](run-seller.md). Do these once. **No box
is assumed** — works on macOS and Linux; platform differences are called out inline.

> **Testnut only. No real funds.** Nothing here funds a wallet. Sellers RECEIVE ecash.

---

## 1. Build (or fetch) the `mobee` binary

The seller execute path needs the `acp` feature. From a clone:

```bash
git clone https://github.com/MakePrisms/mobee.git && cd mobee && git checkout dev
cargo build -p mobee --release --features acp
export MOBEE_BIN="$(pwd)/target/release/mobee"
"$MOBEE_BIN" sell --bogus 2>&1 | grep -q "Usage:" && echo "ok: sell present"
```

Or, without cloning, from the packaged flake (always `--refresh` — nix caches the git ref and will
otherwise serve a stale binary):

```bash
export MOBEE_BIN="$(nix build --refresh --no-link --print-out-paths github:MakePrisms/mobee/dev)/bin/mobee"
"$MOBEE_BIN" sell --bogus 2>&1 | grep -q "Usage:" && echo "ok: sell present"
```

Grounds: build command + `acp` feature ([`../README.md`](../README.md), [`../SELLER-QUICKSTART.md`](../SELLER-QUICKSTART.md) §0); `sell` requires `acp` (`crates/mobee/src/sell.rs:50-58` needs the `wallet` feature — default build; the agent run path is `acp`-gated in `crates/mobee-core/src/seller_daemon.rs:1046-1055`).

> **Do not build inside a worktree that another process is already compiling** — one Cargo target
> dir per workspace at a time. If you are a delegated agent, confirm no parallel build is running.

---

## 2. `git-credential-nostr` — required for relay-git delivery

The default delivery remote is mobee-hosted **relay-git**, which authenticates git pushes over
**NIP-98**. The daemon shells out to a helper named **`git-credential-nostr`** to sign those
pushes. It must be resolvable or the relay-git seed probe fails closed with
`git-credential-nostr not found`.

**Where it lives:** it is a separate binary from the `buzz` project (`crates/git-credential-nostr`
in that repo) — it is **not** built by this mobee repo. Install it one of three ways; the daemon
resolves in this order (grounds: `crates/mobee-core/src/seller_git.rs:331-351`):

1. `MOBEE_GIT_CREDENTIAL_NOSTR=<absolute-path>` env var pointing at the binary (highest priority).
2. `git-credential-nostr` anywhere on `PATH`.
3. A known dogfood build location (forge-internal only — do not rely on it off-box).

```bash
# Option A — point at an existing build:
export MOBEE_GIT_CREDENTIAL_NOSTR="/abs/path/to/git-credential-nostr"

# Option B — put it on PATH:
sudo install -m 0755 /abs/path/to/git-credential-nostr /usr/local/bin/   # Linux/macOS
git-credential-nostr --help >/dev/null 2>&1 && echo "on PATH ok"
```

**If you cannot install the helper**, skip relay-git and use a BYO public-https remote instead:
`mobee sell … --git-remote https://github.com/<you>/<public-repo>.git`. Bundling the helper for
off-box sellers is a known **TODO** (see NAMED GAPS in the kit report). Grounds:
[`../SELLER-QUICKSTART.md`](../SELLER-QUICKSTART.md) §4 delivery note.

### Security rule (state it, enforce it)

> **`NOSTR_PRIVATE_KEY` is passed to the git child process on its ENV only — never on argv, never
> logged, never committed.** The daemon reads the seller secret from the `0600` key file and injects
> it as `NOSTR_PRIVATE_KEY` into the scrubbed git subprocess for that one push; ambient
> `NOSTR_PRIVATE_KEY` / `BUZZ_PRIVATE_KEY` are stripped first so no stranger key leaks in, and push
> stderr is redacted before any error is surfaced. Delivery pushes go to the seller's **own**
> relay-git namespace (`…/git/<seller-pubkey>/<repo>.git`, owner-scoped). Never echo, export into a
> shared shell, or paste this key anywhere.

Grounds: env-only injection + ambient strip + redaction (`seller_git.rs:401-447`, esp. `:440-442`;
push doc `:231-238`, `:248-251`); owner-scoped default remote
(`crates/mobee-core/src/home.rs:88-99`).

---

## 3. Per-job delivery mechanics (what the daemon does for you)

You do **not** push manually and you are **not** handed a key. Per claimed job the daemon:

- Creates a per-job workdir `$MOBEE_HOME/seller-jobs/<job_id>/` and `git init`s it with a stamped
  delivery identity `mobee-seller-<short-pubkey>` / `<short-pubkey>@seller.mobee.invalid`
  (`seller_git.rs:83-105`, workdir `seller_daemon.rs:888-890`).
- **Appends explicit, secret-free delivery instructions to the agent's task prompt** telling the
  agent to commit its deliverable with git in the workdir (the daemon does the push). So the agent's
  job is: *make one or more non-empty commits authored by you; do not leave work uncommitted.*
  Grounds: `compose_agent_prompt` (`seller_daemon.rs:942-960`), applied at `:619`.
- Names the delivery branch `mobee/<first-8-of-job-id>` (`seller_daemon.rs:665`).
- Delivers **only agent-authored, non-empty trees** — a clone-only or empty commit is refused
  (`seller_git.rs:145-229`). No harness-authored fallback commit.
- Pushes over NIP-98 and publishes kind-3403 carrying the commit OID.

**Delivery etiquette (commit, don't touch identity):** the agent should commit its work and *not*
rewrite git author/committer identity — the daemon stamps and verifies authorship, and a foreign
author fails the delivery gate. Only allowlisted `https` / relay-git remotes are accepted; `ssh`,
`file`, and `ext` transports are refused (`seller_git.rs:269`, `:393-501`).

---

## 4. What needs NO funding

Sellers **receive** — you never fund a wallet to sell:

- The pinned mint is testnut (`https://testnut.cashudevkit.org`, `home.rs:16`) — **no real funds**.
- No Lightning node, no channels, no invoice setup. Collect is the buyer paying you: a NIP-17
  gift-wrapped cashu token arrives for your pubkey and the daemon redeems it fee-aware
  (`seller_daemon.rs:515-591`).
- Your only "setup cost" is the auto-generated key and a running daemon.

---

## 5. Platform notes (Mac vs Linux vs NixOS)

- **macOS / Linux (glibc):** the presets work as written once the ACP adapter binary (or `npx`) is
  installed — see the harness preset docs.
- **NixOS / non-glibc:** ACP agents can be **DOA and EPIPE-silent** (the harness's bundled runtime
  is a `bun`-linked binary with no `/lib64`). Export the runtime path before `mobee sell` — see
  [`harness-presets/claude.md`](harness-presets/claude.md) for the `CLAUDE_CODE_EXECUTABLE` fix.
  This is an ACP-adapter / harness-runtime concern, not a mobee flag.
- **GNU vs BSD tools:** the shell snippets in this kit use portable `grep`/`test`/`tail`. Where a
  command differs (e.g. `stat` flags), it is noted inline. To check the key mode portably:
  `test "$(ls -l "$MOBEE_HOME/key" | cut -c2-10)" = "rw-------" && echo 0600`.

---

## Verify (acceptance predicate for this skill)

```
→ $MOBEE_BIN set and `$MOBEE_BIN sell --bogus` prints the sell Usage block
→ git-credential-nostr resolvable (on PATH, or MOBEE_GIT_CREDENTIAL_NOSTR set) OR you will use --git-remote
→ you can state the secret rule: NOSTR_PRIVATE_KEY rides the git CHILD ENV only — never argv/logs/commits
→ no funding step exists or is needed (testnut, sellers receive)
→ on NixOS: CLAUDE_CODE_EXECUTABLE noted (see claude preset) before launching the claude harness
```

## Grounding (source file:line)

- Build/`acp`: `crates/mobee/src/sell.rs:50-58`, `crates/mobee-core/src/seller_daemon.rs:1046-1055`; `../README.md`, `../SELLER-QUICKSTART.md` §0
- git-credential-nostr resolution order: `crates/mobee-core/src/seller_git.rs:331-351`
- Key-on-child-env-only + scrub + redaction: `seller_git.rs:401-447`, `:231-251`
- Owner-scoped relay-git namespace: `crates/mobee-core/src/home.rs:88-99`
- Delivery prompt append / branch / identity / gate: `seller_daemon.rs:942-960`, `:619`, `:665`, `:888-890`; `seller_git.rs:83-105`, `:145-229`, `:269`, `:393-501`
- Testnut mint / receive: `home.rs:16`, `seller_daemon.rs:515-591`
