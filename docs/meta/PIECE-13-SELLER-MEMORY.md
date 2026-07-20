# Piece-13 — persistent seller memory

**Give the seller a memory.** Today's `mobee sell` daemon is amnesiac: every job starts from a blank
agent, and the moment a job's per-run transcript is written it is never read again. The daemon knows
what it *earned* (the journal) but keeps nothing about what it *learned* — which task shapes went well,
which buyers were painful, what a class of job actually takes to deliver. This piece adds two layers on
top of the existing per-job execution: a **lossless append-only capture** of every job the seller
touches (the asset), and a **distilled markdown memory** the seller's own agent writes after a job and
reads at the start of the next one (what the agent actually consumes). The distilled layer is always
rebuildable from the capture layer, so the memory system can be re-distilled with better prompts and
models later without ever losing history — "backwards-improving" by construction.

> **Status: design proposal, DOC-ONLY.** This document proposes **no** protocol changes and specifies no
> wire events. It adds two **local, on-disk** capture/derivation layers under `MOBEE_HOME`, one extra
> agent turn, an **operator-authorable** memory dir (§ Human input), and **config-pointed plugin seams**
> over sensible in-repo defaults (§ Defaults + plugin ecosystem); nothing new goes on the relay. Where a
> field the design wants is not something the daemon knows at capture time today, it is named
> **needs-new-plumbing** inline (§ Layer 0) rather than assumed.
>
> **Anchored to:** `dev@868bb09`. All `file:line` references are to that tree.
>
> **Class: not money-adjacent.** Memory is **self-reported by the agent** and must never touch the
> money-safety path (the journal, the pay gate, the receipt bind). It is diagnostic/economic context
> only. See § Threat & integrity. Sibling economic-brain design:
> [`business-manager-design.md`](../../../../projects/mobee-architecture/business-manager-design.md) —
> its `seller-decisions.jsonl` is a **separate** capture stream (§ Relationship to sibling streams).

---

## The two layers

Memory is split so that the thing the agent reads is cheap and lossy, and the thing it is derived from
is complete and permanent.

- **Layer 0 — capture.** One append-only, schema-versioned **episode** per job the seller touches
  (including refusals). Lossless: it records everything the daemon knew about the job and a pointer to
  the raw agent transcript. It is never rewritten or garbage-collected. This is the asset — the moat is
  the history.
- **Layer 1 — distilled memory.** `MOBEE_HOME/memory/` — a `MEMORY.md` index plus topic files, plain
  markdown with `[[wikilinks]]` (the llm-wiki shape). Written **by the seller's own agent** in a
  post-job retro step, read by the agent at the next job's start. Lossy, small, human-readable, and —
  critically — **always rebuildable from layer 0** (§ Evolvability).

The invariant that makes the whole thing safe to evolve: **layer 1 is a cache; layer 0 is the source of
truth.** Delete `memory/` and the next re-distillation reconstructs it from the episode log. Layer 0
records are additive-only and never deleted, so a better distiller run later strictly improves the memory
without any data loss.

## Layer 0 — episode capture

### Where it lives

A new append-only file `MOBEE_HOME/episodes.jsonl`, a sibling of the existing
`seller-journal.jsonl` (`seller.rs:19,232` — both at `home.root`) and of the
business-manager's proposed `seller-decisions.jsonl`. One JSON object per line, one line per job
the seller **classified** (claimed *or* refused). Same durable-append discipline as the journal
(`OpenOptions::append` + `sync_all`, `seller.rs:359-372`).

### Why a separate file, not an extension of the journal

**Decision: episodes are a separate stream that *references* the journal by `job_id`/`result_id`; they do
NOT extend `seller-journal.jsonl` and do NOT re-own any money-safety fact.** Grounded in the code:

1. **The journal parser is strict and fail-closed.** `SellerJournal::entries()` does
   `serde_json::from_str::<JournalEntry>` on **every** line and returns a `"corrupt journal line"` error
   on any shape it does not recognise (`seller.rs:255-257`). Writing rich episode records into the same
   file would make every reconcile/dedup read treat them as corruption — a **daemon-fatal** regression on
   the money-safety path.
2. **The journal is a hot path read in full on every trade.** `has_claim` / `has_receipt` /
   `has_release` each re-read and re-parse the entire file on every claim and every payment
   (`seller.rs:262-282`, O(n) per operation). Bloating it with lossy episode detail degrades the
   money-safety path that dedup and pay-once depend on.
3. **Single-owner-per-fact avoids the drift the charter warns against.** The journal stays the sole
   source of truth for money facts (claim / receipt / release, amounts, mint, buyer — pay-once and
   `plan_orphaned_claims` restart-reconcile, `seller.rs:184`). Episodes carry the *additional* context and
   **reference** the journal's `job_id` (and `result_id` on delivery) rather than duplicating its
   authoritative fields. Because no fact has two writers, the two files cannot drift — they **join** on
   `job_id`. A verifier reconciles by joining, never by comparing two copies of the same number.

### Episode schema (v1)

Schema-versioned so layer 0 can evolve additively forever. Every episode carries `schema_version` and
`episode_kind` (`claimed` | `refused`). Fields below are grouped by the daemon lifecycle point at which
they become known; each is annotated with the code site that already has the value in scope, or marked
**⚠ needs-new-plumbing** where the daemon would have to be taught to capture it.

**Envelope (always present).**
- `schema_version` — integer, starts at `1`.
- `episode_kind` — `claimed` | `refused`.
- `captured_at` — unix seconds at episode write.
- `seller_pubkey` — `SellerDaemon::seller_pubkey` (`seller_daemon.rs:314`).

**Offer facts (known at classify, both kinds).** All from `ParsedOffer` (`gateway.rs:114`) available in
`classify_offer` (`seller_daemon.rs:428`):
- `job_id` — offer event id (`event.id.to_hex()`, `seller_daemon.rs:490`).
- `offer_task` — full task text (`offer.task`). Lossless: the whole prompt, not a summary.
- `output_type` (`offer.output`), `amount` (`offer.amount`), `unit`, `mint` (`offer.mint_url`),
  `deadline_unix` (`offer.deadline_unix`), `offer_target` (`offer.seller_pubkey` — targeted vs open-pool).
- `buyer_pubkey` — `event.pubkey.to_hex()` (`seller_daemon.rs:509`).
- `job_class` — `from_scratch` | `contribution`, from `intent.contribution.is_some()`
  (`seller_daemon.rs:469-478`); when contribution, `contribution_target` / `contribution_base_oid` from
  `ContributionOffer` (`contribution.rs`, threaded at `seller_daemon.rs:955-965`).
- `configured_rate_sats` — `SellerConfig::rate_sats` (`home.rs:73`) — the rate context the decision was
  made against.

**Refusal facts (`episode_kind = refused`).**
- `refusal_reason_code` — the `OfferSkip` variant name; `refusal_reason` — its `OfferSkip::reason()`
  string (`seller_daemon.rs:230-256`), already an enumerated, machine-mappable set.
  **⚠ needs-new-plumbing:** refusals are currently `eprintln!`-only on the skip path
  (`seller_daemon.rs:349`) with **no durable record**. Capturing a refused episode means calling the
  episode writer on the `OfferDisposition::Skip` branch. The *reason enum already exists*; only the write
  is new.

**Claim facts (`episode_kind = claimed`).**
- `claim_id` — kind-7000 event id (`ActiveJob::claim_id`, set `seller_daemon.rs:385-416`).
- `claim_ts` — journal claim timestamp (`append_claim` writes `now_unix()`, `seller.rs:298-304`).
- `deadline_unix` — resolved job deadline (`ActiveJob::deadline_unix`, `job_deadline_unix`
  `seller.rs:111`).

**Delivery facts (claimed jobs that reached kind-6109 publish, `execute_active_job`
`seller_daemon.rs:977-987`).**
- `result_id` — kind-6109 result event id.
- `commit_oid` (`commit`), `fork_ref` (`git_remote` + `branch`), `delivery_kind` (`"fork"`,
  `seller_daemon.rs:915`).
- `usage` — `UsageMetadata` (`driver/mod.rs:62`): `model`, `input/output/reasoning/cache_read/cache_write`
  tokens, `cost`, `transport`. **Opportunistic — often partial or `None`** (absent-stays-absent;
  never zero-fill, mirroring `seller_exec_metadata` `seller_daemon.rs:1051`).
- `wall_time_ms` (`seller_daemon.rs:847`), `harness` (`harness_and_transport` `seller_daemon.rs:1123`).
- `transcript_ref` — relative path to the raw transcript, `seller-jobs/<job_id>/seller-run.jsonl` (§
  Transcript retention).
- `deliver_ts` — **⚠ needs-new-plumbing (minor):** not recorded today; only the kind-6109
  `created_at` carries it. Capture `now_unix()` at successful publish.

**Outcome (terminal state of the job).**
- `outcome` — `delivered_paid` | `delivered_unpaid` | `refused` | `errored`.
- `amount_received`, `expected_amount`, `swap_ok`, `collect_ts` — from the journal `Receipt`
  (`try_apply_payment` `seller_daemon.rs:748-767`; journal entry `seller.rs:140-148`) or the
  `ReceiptOutcome` (`seller_daemon.rs:1000`).
- `error_reason` — machine-readable failure reason for `errored`. **⚠ needs-new-plumbing:**
  `fail_active` currently **discards** its `_reason` argument (`seller_daemon.rs:990`) and publishes a
  generic kind-7000. Threading that string into the episode is a small change.
- `receipt_event_id` (kind-3400) — **⚠ needs-new-plumbing:** the kind-3400 receipt is **buyer**-published
  (`authorize_pay.rs:459`); the seller daemon never holds or observes it. Recording it would require the
  seller to fetch its own kind-3400s from the relay — out of scope for v1. The seller's own delivery id
  (`result_id`) and the redemption record stand in as the trade anchor.

An episode is written **incrementally is NOT required**: v1 writes one terminal episode per job at the
job's terminal transition (paid, delivered-unpaid at backpressure/timeout, refused, or errored), by which
point all in-scope fields above are known in one place. This keeps the writer to a single append per job
and avoids partial-episode reconciliation.

### Transcript retention

The raw agent transcript already exists and is already retained: `run_agent_job` writes an `EventLog` to
`seller-jobs/<job_id>/seller-run.jsonl` (`seller_daemon.rs:1284`), and the per-job workdir is **never
cleaned up on the production path** — the only `remove_dir_all` calls in `seller_daemon.rs` are inside the
test module (grep: all matches ≥ line 1916). So the transcript is durable by default; the episode only
needs to store a **pointer** (`transcript_ref`), not a copy.

**Retention policy: keep everything.** Disk is cheap and history is the moat — the transcript is the
richest possible layer-0 record and the raw material for any future re-distillation. v1 defines **no
rotation and no deletion**. A retention/rotation knob (age- or size-bounded, e.g. compress transcripts
older than N days) is named as a **future, optional** addition; it is a pure layer-0 policy and never
affects episodes or memory.

## Layer 1 — distilled memory

### Convention

`MOBEE_HOME/memory/` (`home.root.join("memory")`), created on first retro:
- `MEMORY.md` — the index: one line per topic file, loaded at job start.
- Topic files — markdown, freeform, cross-linked with `[[wikilinks]]`. The agent owns their names and
  contents; the daemon owns only the directory and the index-load convention.

This is deliberately the same llm-wiki shape a forge hand's own memory uses, so the primitive is familiar
and a future migration to a richer store (§ Evolvability) is a projection, not a rewrite.

### Read-on-start

The daemon already composes the agent's task prompt itself — `compose_agent_prompt(task, git_remote)`
(`seller_daemon.rs:1216`) builds public, secret-free text that is handed to the ACP agent as a single
text `ContentBlock` (`seller_daemon.rs:1293-1298`). This is the least-invasive seam: **inline the
`MEMORY.md` index into that prompt** and name the absolute memory directory so the agent can read topic
files on demand.

Why inline rather than rely on the agent reading the file itself: the ACP agent's cwd is the per-job
workdir `seller-jobs/<job_id>` (`SessionConfig.cwd`, `seller_daemon.rs:1289`), **not** `MOBEE_HOME`, so
`memory/` is not reachable by a relative path and a from-scratch job's workdir contains nothing. Inlining
the bounded index guarantees the memory reaches context regardless of the agent's file-access behavior;
the absolute path lets a capable agent pull specific topic files. Config-gated by `memory_enabled`
(default **on**); when off, `compose_agent_prompt` is byte-identical to today.

### Retro (write-back)

After a job reaches a terminal delivered state (and, when cheap enough, after refusals too), the daemon
runs **one extra agent turn** with a retro prompt — "update your durable memory with what this job
taught you; write to `MOBEE_HOME/memory/`, keep `MEMORY.md` a current index, link topics with
`[[wikilinks]]`." Mechanically this reuses the existing `run_agent_job` machinery
(`seller_daemon.rs:1262`) with the memory dir as an allowed write target, invoked after
`mark_delivered` / `reconcile_payments` on the success path. Config-gated by `retro_enabled`, **separately
from** `memory_enabled` (the read path is cheap; the retro turn costs a model call, so it can be turned
off independently for cost control). The retro turn is best-effort: a failed or timed-out retro logs and
is skipped — it must never affect the trade's money path or block the daemon.

## Human input — the operator writes memory too

Layer 1 is plain markdown in a plain directory **by design** precisely so the operator is a first-class
author, not just a reader. The seller's human can open `MOBEE_HOME/memory/`, read what the agent has
learned, correct it, and add their own durable guidance (house rules, buyers to avoid, task shapes to
prefer) — the same files, the same `[[wikilink]]` shape. The model is **defaults + human override**: the
agent supplies a working memory out of the box; the operator refines it.

For this to be safe, two things need a convention: telling agent-written from human-written content
apart, and making sure a re-distillation (§ Evolvability — which regenerates the agent's memory) never
clobbers what the human wrote.

**Provenance convention (v1: file-level ownership).** Every topic file carries a minimal YAML frontmatter
`author: agent | operator` (and `updated_at`). The distiller stamps `author: agent` on everything it
writes; the operator's files are stamped `author: operator`. A distinguished `operator-notes.md` topic
file is always operator-owned by convention and seeded (empty, `author: operator`) when the memory dir is
created, so there is an obvious place for human guidance from day one. `MEMORY.md` (the index) lists both
kinds and marks each entry's author.

**Merge-not-clobber rule.** Re-distillation regenerates **only** files stamped `author: agent`.
Files stamped `author: operator` (including `operator-notes.md`) are **never** regenerated or overwritten
— they are read as input to the distiller (so the human's guidance informs the rebuild) and passed
through untouched. If the operator wants to lock an agent-written topic against future regeneration, they
flip its frontmatter to `author: operator` — that single edit takes ownership. Whole-file ownership keeps
v1 simple and unambiguous; **block-level** provenance (preserving human-marked spans inside an
agent-owned file) is named as a **future** refinement, not built in v1. The read-on-start path
(§ Layer 1) inlines the index and exposes the dir regardless of authorship, so human-written topics reach
the agent's context exactly like agent-written ones.

## Defaults + plugin ecosystem

The design ships **sensible defaults in-repo** and makes each behavior-defining seam **overridable via
config** — the same "batteries included, everything swappable" shape as Claude Code's own
**skills/hooks** pattern (ship defaults; let the operator drop in files that override or extend them).
The precedent is deliberate: memory behavior should be operator-tunable without a fork of mobee.

Three seams are override points in v1, each a **file path in `[seller.memory]` config** that falls back to
an in-repo default when unset:

1. **The retro/distiller prompt** — `retro_prompt_path`. The template the retro turn (§ Layer 1) runs.
   Default shipped in-repo; an operator points this at their own template to change what the agent
   distills and how. This is the highest-value seam — it is where memory *policy* actually lives.
2. **The read-on-start injection** — `read_on_start_template_path`. The template that frames how
   `MEMORY.md` is inlined into `compose_agent_prompt` (`seller_daemon.rs:1216`). Default in-repo; override
   to change how memory is presented to the agent at job start.
3. **The claim-policy hook** — the **sibling seam, already specced** in the business-manager design
   (`business-manager-design.md` § V1 — the claim-policy hook): a swappable pre-claim decision function
   that reads the same local state and writes `seller-decisions.jsonl`. Named here as the economic-brain
   counterpart to these memory seams — the two ecosystems share the "defaults + override" frame and the
   same local capture substrate.

**v1 is deliberately light: config points at files, nothing more.** There is **no plugin registry, no
marketplace, no discovery/versioning machinery, and no dynamic code loading** — a "plugin" in v1 is just
an operator-supplied prompt template or (for the claim hook) a swapped function behind config. A richer
ecosystem — a drop-in `MOBEE_HOME/plugins/` convention with auto-discovery, packaged/versioned plugins,
or a shared registry — is named as **future** and explicitly out of scope for v1 (§ Non-goals). The v1
seams are chosen so that future machinery is additive: a registry would just be another way to populate
the same config paths.

## Evolvability guarantees

This is the point of the whole design.

- **Layer 1 is always rebuildable from layer 0.** `memory/` is a cache. A re-distillation pass reads the
  full `episodes.jsonl` (plus the transcripts it points at) and regenerates the memory with a better
  prompt or a stronger model. Because layer 0 is complete, the rebuilt memory can be **strictly better**
  than any earlier version — "backwards-improving." Deleting `memory/` is safe.
- **Additive-only schema evolution.** Episodes carry `schema_version`; new fields are **added**, never
  removed or repurposed, and readers tolerate missing fields (the journal already models this with
  `#[serde(default)]` back-compat, `seller.rs:131-138`). A v2 reader parses a v1 episode; a v1 reader
  ignores unknown v2 fields. **Layer-0 records are never deleted or rewritten.**
- **Derived views are read-only projections over layer 0.** A future sqlite index, a pgvector semantic
  store, or reputation aggregates are all **read-only projections** built by scanning the episode log —
  never a second writable source of truth. They are **non-goals for v1** (§ Non-goals) but the record is
  designed so they are possible later: err toward capturing *more* in the episode now (full task text,
  full usage, transcript pointer) so the "necessary information" is already on disk when we want to
  project it.

## Relationship to sibling capture streams

Three append-only streams live side by side under `MOBEE_HOME`, each single-owner:
- `seller-journal.jsonl` — **money-safety truth** (claim / receipt / release; pay-once; restart-reconcile).
  Unchanged by this piece.
- `seller-decisions.jsonl` — **claim-policy decisions** (proposed by the business-manager design): what
  the economic brain was offered, what it decided, why, and the decision latency. A *sibling*, referenced
  here, **not absorbed** — it captures the pre-claim judgment; episodes capture the post-claim reality.
- `episodes.jsonl` — **rich per-job capture** (this piece). Joins to the journal by `job_id`/`result_id`
  and to the decisions log by `job_id`.

They join on `job_id`. None duplicates another's authoritative fields, so none can drift.

## Threat & integrity

Memory is **self-reported by the agent** and derived from the seller's own view of its own jobs. It is a
convenience/economic asset, not evidence. Two hard rules:

1. **Memory must never feed the PAY gate.** Buyer-side payment authorization stays strictly
   artifact-based — delivery verification, the receipt bind, `amount == offer.amount`
   (`authorize_pay.rs`, unchanged). Nothing in `memory/` or `episodes.jsonl` is ever an input to a
   pay/verify decision on either side.
2. **Self-reported ≠ verifiable.** Episode `usage`/`cost` are the same seller-claimed, unverifiable
   figures piece-9 already labels `metadata_trust=seller-claimed` (`seller_daemon.rs:1067`). Treat them as
   the seller's private bookkeeping. Reputation (a future chapter) must derive from **buyer-verifiable
   receipts**, never from a seller's self-authored memory — explicitly a **non-goal** here.

## Non-goals (v1)

- No sqlite, no vector search, no embeddings — plain JSONL + markdown only.
- No cross-seller / shared memory — one seller, its own `MOBEE_HOME`.
- No reputation scoring — reputation derives from receipts, not self-reported memory; separate chapter.
- No buzz integration and no new relay events — nothing leaves the box.
- No transcript rotation/GC — keep everything (rotation named as future-optional).
- No kind-3400 receipt observation by the seller — out of scope (needs-new-plumbing, named above).
- No plugin registry / marketplace / auto-discovery / dynamic code loading — v1 seams are config
  file-paths only (§ Defaults + plugin ecosystem); richer machinery is future.
- No block-level memory provenance — v1 is whole-file ownership (§ Human input); span-level is future.

## V1 build plan

Three pieces, each with artifact-predicate acceptance. All additive; none touches the money-safety path.

### Piece A — episode capture in the daemon
Add `episodes.jsonl` + the `Episode` type (schema_version=1) + a single terminal write per job on the
claimed **and** refused paths. Includes the small plumbing: durable refusal capture (skip branch),
`deliver_ts`, and threading `fail_active`'s dropped reason into `error_reason`.
- **Acceptance:**
  - A claimed→delivered→paid job appends exactly one episode line with `outcome=delivered_paid`,
    populated `result_id`/`commit_oid`/`amount_received` and a `transcript_ref` pointing at an
    on-disk `seller-run.jsonl`.
  - A refused offer appends exactly one episode with `episode_kind=refused` and a non-empty
    `refusal_reason_code` matching the `OfferSkip` variant.
  - `seller-journal.jsonl` byte-content for the same run is unchanged vs pre-piece (money path
    untouched); `SellerJournal::entries()` still parses green.
  - Red-on-revert: reverting the writer drops the episode line (asserted by hash/line-count delta).

### Piece B — memory dir + read-on-start
Create `MOBEE_HOME/memory/` on demand (seeding an empty `operator-notes.md`, `author: operator`); inline
`MEMORY.md` into `compose_agent_prompt` behind `memory_enabled`, framed by the `read_on_start_template_path`
seam (in-repo default when unset).
- **Acceptance:**
  - With `memory_enabled=true` and a non-empty `MEMORY.md`, the composed prompt string contains the
    index text and the absolute memory path (unit-assertable on `compose_agent_prompt`).
  - With `memory_enabled=false`, the composed prompt is byte-identical to the pre-piece output (golden
    test).
  - On first creation the dir contains `operator-notes.md` stamped `author: operator`.
  - When `read_on_start_template_path` points at an operator file, the composed prompt reflects that
    template, not the default (unit test); unset falls back to the in-repo default.

### Piece C — retro write-back (with provenance + prompt seam)
After a delivered-paid (and optionally refused) terminal transition, run one retro agent turn against the
memory dir behind `retro_enabled`, using the `retro_prompt_path` template seam (in-repo default when
unset); best-effort, non-blocking. The distiller stamps `author: agent` and honors merge-not-clobber.
- **Acceptance:**
  - With `retro_enabled=true`, a completed job triggers exactly one extra agent turn whose session cwd
    grants write to `MOBEE_HOME/memory/`, and a `MEMORY.md` exists afterward (integration test with the
    mock driver asserting the extra turn + the file).
  - A retro that errors or times out is logged and does **not** change the journal or the trade outcome
    (fault-injection test: money path green with retro forced to fail).
  - With `retro_enabled=false`, no extra turn is issued.
  - Files written by the retro are stamped `author: agent`; a pre-existing `author: operator` file
    (and `operator-notes.md`) is byte-unchanged across a retro run (merge-not-clobber test).
  - When `retro_prompt_path` points at an operator template, the retro turn uses it, not the default
    (assertable on the composed retro prompt).

## Open questions (for the human owner)

1. **Retro cost vs value — retro every job, or only some?** A retro turn is a full extra model call per
   job. *Recommendation:* v1 defaults `retro_enabled=on` but retro only on **delivered-paid** jobs (the
   ones with a real signal), with refusal-retro off by default — refusals are already losslessly captured
   in layer 0 and can be batch-distilled later without a per-refusal model call.
2. **Memory scope — per-seller-identity, or per-agent-preset?** A seller can run different harnesses
   (`claude`/`codex`/`cursor`) over time; memory written by one may mislead another. *Recommendation:* v1
   keeps a single `MOBEE_HOME/memory/` per seller identity (simplest, matches the single-home model), and
   we let re-distillation slice by harness later from layer 0 (`harness` is on every episode) if it
   proves necessary — don't pre-partition.
3. **Retro isolation — reuse the job's agent session, or a fresh turn?** Reusing the just-finished
   session gives the retro full context for free but couples it to the delivery turn's lifecycle; a fresh
   turn is clean but must be re-fed the transcript. *Recommendation:* a **fresh** turn seeded with the
   episode + `transcript_ref` — it keeps delivery and retro independently gateable and failable (the
   money path never waits on retro), at the cost of re-reading the transcript.
4. **Plugin seams — explicit config file-paths, or a drop-in `MOBEE_HOME/plugins/` convention?** v1 as
   specced uses explicit `[seller.memory]` paths (`retro_prompt_path`, `read_on_start_template_path`). A
   more Claude-Code-like alternative is auto-discovery: drop a template into a well-known dir and it takes
   effect without editing config. This is a genuine fork — the first is simpler and fully explicit, the
   second is friendlier to a future ecosystem but adds discovery/precedence rules. *Recommendation:*
   ship **explicit config paths in v1** (no discovery surface, trivial to reason about) and revisit a
   drop-in convention when a real second plugin exists to justify it — the config paths remain the
   underlying mechanism either way, so a drop-in layer is purely additive later.
