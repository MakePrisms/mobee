# Piece-10 — freelance-PR (contribution) delivery

**The forge hires its own sellers.** This chapter makes mobee do real work *for the forge*: the
**forge itself becomes a BUYER**, posting jobs against **forge repos it owns**, and mobee **sellers**
(turtle or external) do the work and deliver it as a **pull request** — fork the target, run the agent,
push a branch, and the **buyer/forge reviews, merges, and pays**. This fits how the forge team already
collaborates: sellers publish to their own **relay-git namespaces** and **announce via NIP-34**
(kind-30617); the targets are **relay-git-hosted forge repos**. "Freelance-PR" is that primitive —
agent labor delivered as a mergeable contribution, settled on-chain-adjacent through the existing money
path.

Technically this is a protocol/spec extension atop landed **piece-7** (git delivery) and **piece-6**
(payment SM). It adds a **contribution** job class: the buyer's offer names a **target repo it owns** +
a base commit, and the seller delivers a change *against that repo*. Two seller paths —
**fork** (push to the seller's own relay-git namespace) or **NIP-34 patch** (kind-1617). **v1 ships
fork-only**; patch is designed below and explicitly deferred.

> **v3 — a refinement of the v2 adversarial draft (compose → blinded adversarial (Opus) → codex). v2's
> reshape is PRESERVED in full.** v3 folds the now-**RESOLVED** operator rulings, reframes the goal
> (forge-as-buyer), separates delivery from settlement, adds a chapter-level acceptance, and
> **re-verifies every code ref against dev tip `0f05d9b`**. The money path is **commit-oid-typed end to
> end**; the descendant gate is still **greenfield** (no spike code in the source tree); **the receipt
> now DOES attest the delivered object** — piece-9 v3 landed the delivery binding (**option (a)**, @
> `4190a15`); **replay/lift (authorship bind) is still open**. freelance-PR remains a **full
> money-adjacent build, not an additive doc change** (size acknowledged by gudnuf). This piece goes to a
> fresh **codex-deep design pass + coordinator shape review** before any build.

Class: **MONEY-adjacent** → composition + adversarial + codex + operator gate. **Re-verified against
`origin/dev` `0f05d9b`** (dev tip): `authorize_pay.rs`, `receipt.rs`, `payment.rs`, `delivery.rs`,
`delivery_git.rs`, `job_lifecycle.rs`, `gateway.rs`, `profile.rs`, `home.rs`. Since the v2 verify pass
(`168c8bc`) the **mac cross-platform git-timeout fix rewrote `delivery_git.rs::git_output_timed`** to an
**in-process `wait_timeout`** (no more `timeout(1)`), so `delivery_git.rs` **line numbers shifted** — the
**fetch + `^{commit}` peel logic is unchanged**; only the timeout mechanism moved. All refs below are the
current dev-tip locations.

---

## Delivery model (fork, v1) — names + flow

The coordinator's names, used consistently throughout this doc:

- **`target_repo`** — the buyer/forge repo the contribution is *for* (pinned by `naddr`: owner pubkey +
  clone URL, not a bare `d`-tag).
- **`base_oid`** — the exact commit the contribution must **descend from** (carried on the offer's
  `base` tag, alongside the base branch).
- **`fork_ref`** — the seller's fork **repo + branch** in the seller's own relay-git namespace (what the
  buyer fetches and later merges).
- **`commit_oid`** — the **fork tip** commit the seller advertises and the buyer pays against.

**Flow:** offer (`target_repo` + `base_oid`) → seller forks + runs its agent + pushes `fork_ref` →
seller **result advertises `{target_repo, base_oid, fork_ref, commit_oid}`** → **buyer verify**: fetch
the fork commit → assert it **descends from `base_oid`** (ancestry) → **tip-match** (fetched tip ==
`commit_oid`) → **bind payment to `commit_oid`** → **merge** = the buyer/forge merges `fork_ref` into
`target_repo` ("accept the PR") → receipt closes. Merge is a **buyer-custody** action (accepting the
PR); it is **not** what payment binds — payment binds the verified `commit_oid`.

---

## Current state (VERIFIED in code @ `0f05d9b`)

- **The money path is commit-oid-typed throughout.** `authorize_pay.rs:162` refuses unless
  `delivery_integrity_hash == commit_oid`; `job_lifecycle.rs:569` (`authorize_request_from_bind`)
  refuses unless the buyer's `delivery_integrity_hash` equals the **accepted** commit (`bind.commit_oid`)
  and builds the pay request straight from the accept-bind (`:575-586`, `commit_oid = bind.commit_oid`);
  `delivery_git.rs:88-124` fetches the advertised **branch** tip (`+refs/heads/<branch>:…`, `:90`) and
  peels `<ref>^{commit}` (`:108`) — a **tree oid would fail** the peel. `AuthorizePayRequest` /
  `GitDelivery` / `DeliveryVerifier` all require non-empty `repo`+`branch`+`commit_oid`.
- **The buyer pays only what it accepted.** The pay request is built from the accept-bind
  (`job_lifecycle.rs:559-586`), whose `commit_oid` was captured from the accepted result at accept time
  (`:458-486`). But **nothing binds the delivered object to the seller's authorship**, and the offer's
  `target_repo` / `base_oid` are **not threaded** into the request or verifier.
- **The receipt now attests the delivered object (piece-9 v3 landed).** The kind-3400 `receipt_draft`
  carries a `delivery_integrity_hash` + `delivery_kind` (`fork`|`patch`) binding (`gateway.rs:501,
  525-531`; `ReceiptDelivery` `:490-495`), and **both fields are inside the co-signed preimage** —
  `ReceiptPreimage` fields (`receipt.rs:110,112`) are serialized into the signed `canonical_json`
  (`:129-130`). `authorize_pay.rs:296-305` builds that preimage and today hardcodes
  `DeliveryKind::Fork` (`:305`) — i.e. **v1 = fork-only is already baked into the money bind**. (See
  Receipt binding — RESOLVED.)
- **NIP-34: kind-30617 present** (`profile.rs:421`); **kind-1617 (patch) absent** (0 hits in source) —
  patch is greenfield.
- **Descendant/identity gates are greenfield, not "re-activatable":** `verify_git_descendant` /
  `merge-base --is-ancestor` / `ensure_repo_job_protection` are **0 hits anywhere in `crates/*/src`** —
  they live **only in docs** (PIECE-7, REBUILD-SEAM), **not in source**. No `base_oid` is threaded
  anywhere in the pay path (**0 hits for `base_oid` in `mobee-core/src`**), and custody fetches only the
  advertised tip (`delivery_git.rs`), so `base_oid`'s object isn't even present for an ancestor check.
- **`.names` registry is GLOBAL across owners** (`home.rs:86-87`; remote helpers `:88` repo-id, `:95`
  remote, `:102` `relay_git_repo_id`) → a bare repo-name / `d`-tag `target_repo` is spoofable.
- (`delivery_git.rs` seals a transport allowlist — bare git verify can fetch `ext::` = RCE; the pay path
  must use `PayPathDeliveryVerifier` (`:159-160`, wraps `AllowlistedDeliveryVerifier`), the only public
  fetch-capable factory (`:20-21`). The contribution paths inherit that allowlist.)

---

## Scope decision — v1 = FORK only; PATCH deferred (RULED)

**RULED GO — v1 = fork-only** (hearth Q3 + gudnuf). **Path B (patch → tree-oid) cannot reuse the money
path.** The path is commit-typed at three gates, fetches a branch tip, and peels `^{commit}`; a tree oid
cannot flow through it. Supporting patch means a **parallel typed money-path** — a `Patch` variant across
`AuthorizePayRequest`, `PaymentKey`, `DeliveryVerifier`, and the `== commit_oid` gates — a substantial
build, not a branch of the existing one. **v1 ships FORK only; patch is a follow-up piece.** The patch
design is specified below (§ Path B) but marked **NOT-v1**.

---

## Path A — fork (v1)

The seller branches off the offer's `base_oid`, commits its agent's work, and pushes to its **own**
relay-git namespace (`default_relay_git_remote`, `home.rs:95` — owner-scoped push forces the fork; no
write access to the buyer's `target_repo`). It announces the fork (kind-30617, `profile.rs:421`) and its
kind-6109 **result advertises `{target_repo, base_oid, fork_ref, commit_oid}`** (`fork_ref` = fork repo
+ branch; `commit_oid` = the fork tip). **Binding: `delivery_integrity_hash = commit_oid`** (the fork
tip) — the existing commit-oid tip-match (`delivery.rs:86` `from_fetched_tip`), so v1 reuses the money
path unchanged. Post-pay, the buyer **merges `fork_ref` into `target_repo`** ("accept the PR") — a
buyer-custody action, not what payment binds.

**MUSTs added for contribution (all NEW build — none exist today):**

1. **Thread `base_oid`** from the offer → accept-bind → pay request → verifier. Without it the
   descendant gate cannot run and the offer's target is decorative. (Today: **0 hits** for `base_oid` in
   source.)
2. **Descendant gate (greenfield).** Fetch `base_oid` into the same custody odb; refuse unless
   `git merge-base --is-ancestor <base_oid> <commit_oid>`. Closes unrelated-history / swapped-base
   advertisement. (New build — no `verify_git_descendant`/`merge-base` in source.)
3. **Authorship bind (closes replay/lift).** The delivered commit MUST carry a trailer binding
   `job_id` + `seller_pubkey`; verify it pre-pay. Without it a seller can advertise a **third party's**
   public fork tip that descends from `base_oid` and be paid for work it did not do (`job_hash` and
   `sig/seller` cover only the job-hash, not the commit).
4. **Pin `target_repo` as an `naddr`** (owner pubkey + relay/clone URL), **not** a bare `d`-tag
   (`.names` is global across owners → spoofable, `home.rs:86-87`). The pay path MUST bind the
   **offer's** `target_repo` + `base_oid`, not accept the seller result's repo unchecked.
5. **Content / non-empty gate (autonomous MUST).** v1 contribution is autonomous (no human reads the
   diff before pay), so refuse a delivery whose diff against `base_oid` is empty or does not touch the
   offered paths — else an empty-but-descendant commit passes descendant + tip-match and is paid
   (paid-worthless grief). Resolves the blind-vs-human ambiguity: v1 is blind → a content gate is
   required. (Note: the post-pay **merge** is a separate human/forge review step — but pay must not
   depend on it; the content gate is the autonomous floor.)
6. **Per-job unique ref** (full `job_id`, not the `mobee/<job_id[:8]>` prefix that collides) and
   **no-force-push as a contribution MUST** (a later push must not move the advertised `commit_oid` out
   from under a paid receipt — do not leave it deferred here).

## Path B — NIP-34 patch (kind-1617) — DEFERRED (design only, NOT v1)

Seller publishes a kind-1617 patch against `base_oid`; **binding = the resulting TREE oid**
(content-deterministic — a commit oid can't work, patch application yields a per-applier commit).
Requires the parallel typed money-path (above) plus:

- **Determinism pinned (money bind):** apply with filters disabled (no autocrlf/ident/clean-smudge),
  a fixed file-mode policy, and a pinned object-format (sha1/sha256) shared seller↔buyer — else honest
  parties compute different trees (false refuse) or an attacker games normalization.
- **Strict apply against `base_oid`:** no 3-way, no fuzz; clean-apply-failure = fail-closed refuse; the
  patch event pins `base_oid` so the tree is unambiguously against the named base.
- Same authorship bind + `target_repo` pin as Path A.
- **`delivery_kind = patch`** in the receipt binding (the tag already exists — `gateway.rs:530`,
  `receipt.rs:112`) discriminates commit-vs-tree in the co-signed preimage.

## Settlement (SEPARABLE from delivery) — `delivery ⊥ settlement`

Delivery (verify a contribution) and settlement (how funds move) are **orthogonal concerns**.
**freelance-PR is a DELIVERY chapter; it ships on status-quo settlement and touches nothing about how
money moves.** (Ruled by gudnuf.)

- **Settlement = TODAY's verify-then-pay, AS-IS (unchanged).** The buyer verifies the delivery (fetch +
  descendant + tip-match + authorship + content), then `authorize_pay` binds payment to `commit_oid`.
  This is the landed piece-6/piece-7 money path; freelance-PR reuses it verbatim. No settlement code
  changes in this chapter.
- **Deadline-lapse = repost / forfeit (option i) — STANDS. No grace-window is built.** Grace is
  **resolved-as-DEFERRED**, not "pending a policy" — there is no i/ii/iii decision to make in v1; the
  status-quo repost/forfeit behavior holds.
- **Escrow / atomic swap = DEFERRED — and the reason is load-bearing, not schedule.** The hard problem in
  paying for agent labor is **NOT the atomicity mechanism — it's JUDGING QUALITY / the result.** Escrow
  only *moves money around* a verify decision that is still hard; it adds no help to the actual question
  ("is this contribution good enough to pay for?"). So **payment sophistication is premature** until the
  quality-judgment problem is understood.
- **Grace + escrow + REPUTATION are ALL deferred to one dedicated FUTURE payment-and-reputation
  chapter,** gated on **real testing**. **Reputation is the natural lever for the quality-judging
  problem** (accumulated seller signal is what makes "good enough to pay" tractable), and reputation is
  also what **makes escrow tractable later** — so it precedes the payment-sophistication work rather than
  following it.
- **freelance-PR delivery is the TESTING VEHICLE that informs that chapter.** Shipping real forge-hires
  on status-quo settlement is exactly how we learn what quality-judgment and reputation need to be —
  which is why delivery must NOT wait on payment sophistication, and payment sophistication must NOT be
  designed before this delivery chapter has produced real data.

## Receipt binding — RESOLVED (option (a); landed @ `4190a15`)

**Resolved via option (a).** piece-9 was re-locked to attest `delivery_integrity_hash` + a
`delivery_kind` (`fork`|`patch`) discriminator **in the kind-3400 schema AND its co-signed preimage** —
so the settlement record attests the delivered object and the kind (commit-vs-tree) is **not forgeable**
(an unsigned discriminator could be flipped `fork`↔`patch` to reinterpret the same 40-hex). This
**landed in piece-9 v3** (`docs/meta/PIECE-9-RECEIPT-AND-EXEC-METADATA.md` @ `4190a15`, ruling D4) and is
present in code at dev tip:

- Receipt tags: `receipt_draft(..)` appends `delivery_integrity_hash` + `delivery_kind`
  (`gateway.rs:501, 525-531`; `ReceiptDelivery{integrity_hash, kind}` `:490-495`).
- Signed preimage: `ReceiptPreimage.delivery_integrity_hash` + `.delivery_kind` (`receipt.rs:110,112`)
  are serialized into `canonical_json` — the exact bytes both parties schnorr-sign (`:119-133`, delivery
  fields at `:129-130`). Doc-comment `receipt.rs:84-85`: "Binds the trade **and** the delivered git
  object (D4)."
- `authorize_pay.rs:296-305` builds the preimage with those fields (fork-only today: `DeliveryKind::Fork`
  hardcoded at `:305`).

> Note for the contribution build: `delivery_kind` is the wire/preimage name of the "path" discriminator
> the coordinator's charter called a *path tag*; it carries `fork`|`patch`. Option (b) (local-journal
> only) is **not** taken.

## Offer shape (contribution mode; additive)

| tag | shape | meaning |
|-----|-------|---------|
| `job-class` | `["job-class","contribution"]` | absent ⇒ from-scratch (back-compat) |
| `target-repo` | `["target-repo","<naddr>"]` | the buyer's `target_repo`, pinned by **owner pubkey + URL** (not a bare `d`-tag) |
| `base` | `["base","<base_branch>","<base_oid>"]` | base branch + the exact `base_oid` the contribution must descend from |
| `accepts` | `["accepts","fork"]` (v1) | positional multi-value (`["accepts","fork","patch"]` when patch ships) — not comma-joined |

The kind-6109 **result** echoes `target_repo` + `base_oid` (so the buyer can bind them to the offer) and
adds `fork_ref` (repo + branch) + `commit_oid` (the advertised tip).

## Back-compat (runtime teeth, not honor-system)

- **From-scratch unchanged** (no `job-class` ⇒ existing path).
- A seller **without** contribution support MUST emit a kind-7000 `status=error` on a
  `job-class=contribution` offer — it MUST NOT silently run it as from-scratch and push to its own repo
  (which would let the buyer pay against a non-descendant commit with no error).
- A buyer MUST **refuse** a result whose delivery does not satisfy the contribution binds
  (descendant + authorship + `target_repo`/`base_oid` match + content gate).

## Money-gate (coordinator's money bar)

The contribution verify-path (base-ancestry + fork-fetch + pay-bind) touches **`PayPathDeliveryVerifier`**
and the `authorize_pay` gates → it is subject to the **coordinator's money bar** before **any** FF that
touches the pay-verify path: **independent full-suite re-run on the frozen candidate + live fixtures +
dual-review (both frames).** The **frozen money-core** (`payment_wallet.rs` / `authorize_pay.rs` /
`payment.rs`) stays **byte-scope** — unchanged unless the build genuinely requires it, and any such need
is **flagged explicitly** for the coordinator (the new binds add threading + a verifier gate; they should
not need to rewrite the frozen wallet/authorize/payment core).

## Findings — RESOLVED

1. **v1 scope = fork-only — GO.** Patch deferred (needs a parallel typed money-path). Ruled by hearth
   (Q3) + gudnuf.
2. **Receipt binding — RESOLVED via option (a).** piece-9 re-locked to attest `delivery_integrity_hash`
   + `delivery_kind` in the kind-3400 schema **and** its co-signed preimage; **landed in piece-9 v3 @
   `4190a15`** (code present at dev tip — see § Receipt binding). The receipt **does** attest the
   delivered object.
3. **Scope = full money-adjacent build — acknowledged.** freelance-PR is a full money-adjacent build
   (`base_oid` threading + greenfield descendant gate + authorship bind + `target_repo` pin + content
   gate + runtime refuse) — **not** the additive doc change the original charter framed. gudnuf
   **size-acked** this. The fork path fits the existing commit-typed money bind; the rest is new.

## Acceptance — SPEC-DOC bar

*(the bar for THIS doc; the chapter bar is below and distinct)*

- Offer fields (contribution) specified + differ from from-scratch.
- **Fork path (v1)** fully specified incl. the six MUSTs; patch path designed + explicitly deferred with
  the reason (money-path retyping).
- Pay binding per path stated (fork = commit-oid tip-match reused; patch = tree-oid, deferred) with the
  commit-vs-tree type-confusion + determinism hazards named.
- Descendant + authorship + `target_repo`-identity gates specified as NEW MUSTs (greenfield).
- Receipt binding resolved (option (a), landed @ `4190a15`) — recorded, not left open.
- Delivery ⊥ settlement; settlement is status-quo verify-then-pay AS-IS; grace + escrow + reputation
  deferred to a future payment-and-reputation chapter (quality-judging is the hard problem, not
  atomicity), with freelance-PR as its testing vehicle.
- Back-compat with runtime teeth (kind-7000 refuse; buyer refuse), not honor-system.
- Code refs re-verified against dev tip `0f05d9b`; moved refs updated.

## Acceptance — CHAPTER (freelance-PR is REAL)

*(distinct from the spec-doc bar above; this is the bar for the built chapter — the forge actually
hiring a mobee to do forge work)*

```
acceptance (chapter):
  return_predicate: >
    A REAL forge job targeting a REAL relay-git forge repo is posted; a mobee seller
    (turtle or external) forks the target, runs its agent, and delivers a result advertising
    {target_repo, base_oid, fork_ref, commit_oid} where the commit DESCENDS from base_oid;
    the BUYER verify-path fetches the fork commit, asserts base-ancestry + tip-match, and
    binds payment to commit_oid; the forge reviews + MERGES fork_ref into the target and PAYS;
    a kind-3400 receipt closes with BOTH co-sigs verifying (independent teeth); full suite
    green on the frozen FF candidate; NON-mock — the PR is agent-authored real work merged
    into a real forge repo.
  non_counting:
    - a from-scratch artifact job (not a contribution against a target repo)
    - a contribution "delivery" that is never actually merged into the target
    - suite-green without a live real-forge-job -> PR -> merge -> pay leg
    - base-ancestry left unchecked
    - payment bound to anything other than the merged commit_oid
```

## Fence / reality class

**SPEC-DRAFT (design).** No code lands here. Reality: from-scratch delivery **PROVEN** (c/c2), and the
collect leg is **REAL-AND-LIVE**; contribution is **NOT BUILT** — fork-path v1 is buildable on the
existing commit-typed money path (receipt binding already landed) **plus** the new gates (base_oid
threading, descendant, authorship, target_repo pin, content); patch path needs a parallel typed
money-path (deferred).

## Reference

- **Commit-typed pay gates:** `authorize_pay.rs:162` (`!= commit_oid` refuse; empty-refuse `:157`),
  `job_lifecycle.rs:559-586` (`authorize_request_from_bind` — request from accept-bind; mismatch refuse
  `:569`; `commit_oid = bind.commit_oid` `:584`), accept captures result `commit_oid` `:458-486`,
  `delivery_git.rs:88-124` (branch fetch `:90` + `^{commit}` peel `:108`; in-process timeout
  `git_output_timed` `:211` via `wait_timeout` `:237`), `delivery.rs:86` (`from_fetched_tip` tip-match).
- **Transport allowlist:** `PayPathDeliveryVerifier` `delivery_git.rs:159-160` (wraps
  `AllowlistedDeliveryVerifier`); `ext::`/RCE note `:20-21`.
- **Receipt binding (landed):** `gateway.rs:501,525-531` + `ReceiptDelivery` `:490-495`; preimage
  `receipt.rs:100,110,112` + `canonical_json` `:119-133`; `DeliveryKind` `receipt.rs:73`; built in
  `authorize_pay.rs:296-305` (`DeliveryKind::Fork` `:305`).
- **NIP-34:** `profile.rs:421` (kind-30617 announce); kind-1617 **absent** (0 hits).
- **Namespace / `naddr`:** `home.rs:86-87` (`.names` GLOBAL across owners), `:88` `default_relay_git_repo_id`,
  `:95` `default_relay_git_remote`, `:102` `relay_git_repo_id`.
- **Greenfield gates:** `verify_git_descendant` / `merge-base --is-ancestor` / `ensure_repo_job_protection`
  / `base_oid` — **0 hits in `crates/*/src`** (docs-only: PIECE-7, REBUILD-SEAM).
- Composes onto [PIECE-7-GIT-DELIVERY.md](PIECE-7-GIT-DELIVERY.md); receipt binding per
  [PIECE-9-RECEIPT-AND-EXEC-METADATA.md](PIECE-9-RECEIPT-AND-EXEC-METADATA.md) (D4, @ `4190a15`).
