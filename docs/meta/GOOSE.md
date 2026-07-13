# Goose — research note for mobee agentic surface

Last updated: 2026-07-13 (mobee-meta independent check + forge memory).
Buzz ask: `88cb2a87…` — team replies pending; fold when they land.

## Upstream snapshot (PROVEN via public sources today)

| Fact | Value |
|------|--------|
| Org / repo | AAIF / [aaif-goose/goose](https://github.com/aaif-goose/goose) (moved from `block/goose`) |
| License | Apache-2.0 |
| Latest release | **v1.42.0** (2026-07-13) — forge athanor pin was **v1.41.0** (2026-07-06) |
| Surfaces | Desktop, CLI, HTTP API, **ACP** (`goose acp`), MCP extensions |
| Rust core | `crates/goose` is an **rlib** with public `Agent` API (`Agent::new`, `reply` stream, extensions) — see `examples/agent.rs` |
| crates.io | Not a useful published embed path for the engine (`goose-sdk-types` alpha ≠ engine) — **git tag dep** is the real path |
| Default features | `default = []` — portable embed = `--no-default-features --features rustls-tls` |

## Rust API / embed — what we know from forge (athanor-app)

Gate 1 spike **GREEN** (2026-07-06): in-process embed + iOS rlib cross-compile + tool round-trip.

- Pin: `goose` git tag **v1.41.0**, `default-features = false`, `features = ["rustls-tls"]`
- Build **`--lib` only** (bins are not for embed targets)
- **No stable blessed embed SDK** — you depend on internal types; carry **`rmcp = "=1.7.0"`** (or goose lockfile) or fresh resolve breaks on rmcp 1.8
- Prefer **Frontend tools** / in-process duplex MCP over spawning `goose`/`goosed` when sandboxed
- Live Anthropic turn through embedded goose **PROVEN** on athanor (~2s sim)
- Field report: `athanor-app/docs/research/goose-embedding-feedback.md` — asks upstream for blessed embed API + lean mobile profile

## Implications for mobee (recommendation — provisional until buzz replies)

Mobee’s seller spine today is **ACP client** (`AcpDriver` + `mobee run --agent-command …`). That stays the right **marketplace boundary**: buyer/seller speak ACP to *whatever* agent binary the seller runs.

| Approach | When for mobee | Notes |
|----------|----------------|-------|
| **A. ACP subprocess** (`goose acp`, `claude-acp`, `codex-acp`) | **Default for v0.x installables / nix harness list** | Matches current spine; multi-harness without embedding goose; nix packages story |
| **B. Embed `goose` rlib** | Only if we want an **in-process seller runtime** (no child agent) or shared agent loop inside mobee | Heavier; tokio; lockfile/rmcp discipline; couples mobee to goose churn; conflicts with “honest sync” until we accept a runtime for that path |
| **C. REST `goosed`** | Web/simple clients | Weaker permissions/streaming vs ACP; not the IDE/agent-job fit |

**Provisional stance:** keep mobee agentic I/O on **ACP**. Treat Goose as (1) a **first-class ACP harness binary** in the nix/harness matrix, and (2) optional later **embed** experiment only if product needs in-process — do not block marketplace merge train on embed. Reuse athanor pin/recipe if/when embed is claimed.

## What NOT to pull casually

- `local-inference` / mlx / cuda / vulkan / `code-mode` / `system-keyring` / full `portable-default` unless needed
- Unpinned git `main` of goose (tag + lockfile discipline)
- Assuming crates.io `goose-sdk-types` is the embed API
- Replacing ACP with goose-only types in marketplace events

## Open questions for the team (buzz)

1. Any mobee-specific Goose decision already made (harness-only vs embed)?
2. Prefer pin **v1.41.0** (forge-proven) vs bump **v1.42.0** for next agentic work?
3. Is `goose acp` already in the planned harness list next to claude-acp / codex-acp?
