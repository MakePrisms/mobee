# Mobee deployment & packaging

Self-host the mobee marketplace. One Rust workspace, Nix as the packaging
foundation, shipped for two runtimes (Docker, NixOS/systemd) across three
operator personas (relay-operator, seller, buyer).

## Principle

Nix + Rust is the foundation: the whole system is one cargo workspace, so every
persona is a package built from the same source and pinned by one `Cargo.lock` +
`flake.lock`. Docker images are built *from* the Nix packages (not a parallel
Dockerfile toolchain), so there is a single build path and no drift between the
two runtimes.

## Components (the backend bundle)

A mobee marketplace backend is three services behind one reverse proxy:

1. **Relay** — a nostr relay in *open mode* (open ingest + open read) accepting
   the mobee event kinds: 0 (profiles), 5109 (offer), 7000 (claim/status),
   6109 (result), 3400 (receipt), 31990 (NIP-89 announce), 1059 (NIP-17
   gift-wrap payment). This is the coordination surface. Reference impl =
   buzz-relay in open mode; the contract is "any nostr relay that accepts these
   kinds without membership."
2. **relay-git** — a git-over-HTTP endpoint serving `/git/<owner>/<repo>`. This
   is the **primary git-management transport** and stays that way: delivery is
   git-objects, verified by the buyer tip-matching the exact commit OID before
   paying. Keeping git as git is what makes delivery cryptographically
   verifiable. Removes the GitHub dependency from the loop.
3. **blossom** — a Blossom blob server (BUD spec, kind-24242 auth) for **blob
   uploads**. Additive, not a replacement: for artifacts that aren't naturally
   git (large binaries, datasets, build outputs). A result/receipt can reference
   a blossom blob hash alongside — or instead of — a git commit, so sellers
   choose the right transport per job. Git stays primary for code; blossom
   covers everything else.

Reverse proxy (Caddy) terminates TLS and routes: relay WS, `/git/…`, blossom
`/upload`+`/<sha256>`, and the observatory static site at `/network`.

## Delivery model (decided)

- **git-objects via relay-git = primary.** Verifiable by tip-match; unchanged.
- **blossom blobs = additive.** For non-git artifacts. The `delivery_integrity_hash`
  binds either a git commit OID or a blossom blob sha256; the verifier accepts
  both, the transport allowlist governs both. (Blossom's content-address IS the
  integrity hash — verification is a sha256 check, cleaner than the git tip-match.)

## Packaging targets

**Docker** — `docker-compose.yml` bundling relay + relay-git + blossom + Caddy +
Postgres (relay DB) + an object store (S3 or local for blossom/media). Images
built from the Nix packages. `docker compose up` = a running marketplace backend.

**Nix** — the flake exposes:
- `packages.{relay,relay-git,blossom,mobee}` — each component + the client binary.
- `nixosModules.mobee-relay` — `services.mobee-relay.enable = true` wires the
  relay + relay-git + blossom + Caddy as systemd units with declarative config
  (open-mode flags, kinds allowlist, TLS host, storage backend).
- `nixosModules.mobee-seller` — `services.mobee-seller.enable` runs the
  `mobee sell` daemon as a systemd service (harness, rate, mint, git-remote from
  module options; key file 0600, never in the nix store).
- `apps.{mcp,sell}` — the `nix run --refresh github:MakePrisms/mobee/<ref> -- mcp|sell`
  client path (buyer + ad-hoc seller), no clone. Always `--refresh` (or pin+bump the rev) —
  nix caches the git ref and will otherwise serve a stale binary.

## Personas

- **relay-operator** — deploys the backend bundle. `docker compose up`, or a
  NixOS host importing `nixosModules.mobee-relay`. This is what makes the
  marketplace theirs, not ours.
- **seller** — `mobee sell`, run three ways by taste: `nix run --refresh … -- sell`
  (quick), the `mobee-seller` NixOS module (persistent, declarative), or the
  Docker image. Same binary, same config contract.
- **buyer** — `nix run --refresh … -- mcp` wired into their agent (Claude etc.), or a
  module for a standing buyer. Zero clone.

Secrets (relay key, seller key, mint auth) are always file-references with 0600
perms, never baked into images or the nix store.

## Sequencing

1. **Flake foundation first** (in progress): fix the client flake so
   `nix run --refresh … -- mcp|sell` works hermetically — the packaging base everything
   else builds on.
2. **Relay bundle**: relay open-mode + relay-git endpoint + Caddy, as
   docker-compose + `nixosModules.mobee-relay`. Gets GitHub out of the loop.
3. **Blossom**: add the blob server to the bundle + the `delivery_integrity_hash`
   accepting a blob sha256 (a mobee-core delivery change — money-bar reviewed).
4. **Persona modules**: `mobee-seller` / buyer NixOS modules.

Build the loop working (seller slice) and the client runnable (flake) before the
self-host bundle — strangers should run a working marketplace before deploying
the backend.
