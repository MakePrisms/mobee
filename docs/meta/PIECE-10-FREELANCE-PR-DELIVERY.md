# Piece-10 — freelance-PR (contribution) delivery

**The forge hires its own sellers.** This chapter makes mobee do real work *for the forge*: the
**forge itself becomes a BUYER**, posting jobs against **forge repos it owns**, and mobee **sellers**
(turtle or external) do the work and deliver it as a **pull request** — fork the target, run the agent,
push a branch, and the **buyer/forge verifies, pays, then merges** ("accepts the PR") — one ordering,
stated in full under § Delivery model. This fits how the forge team already
collaborates: sellers publish to their own **relay-git namespaces** and **announce via NIP-34**
(kind-30617); the targets are **relay-git-hosted forge repos**. "Freelance-PR" is that primitive —
agent labor delivered as a mergeable contribution, settled on-chain-adjacent through the existing money
path.

Technically this is a protocol/spec extension atop landed **piece-7** (git delivery) and **piece-6**
(payment SM). It adds a **contribution** job class: the buyer's offer names a **target repo it owns** +
a base commit, and the seller delivers a change *against that repo*. Two seller paths —
**fork** (push to the seller's own relay-git namespace) or **NIP-34 patch** (kind-1617), typed as the
foundation's `Delivery::Commit` and `Delivery::Tree` variants
([PIECE-12](PIECE-12-TYPED-DELIVERY-ABSTRACTION.md)). **v1 ships fork-only** (the `Commit` variant); patch
(the additive `Tree` variant) is designed below and explicitly deferred.

> **v5 — reworked onto the landed typed-delivery FOUNDATION ([PIECE-12](PIECE-12-TYPED-DELIVERY-ABSTRACTION.md), @ dev `a77cdeb`).**
> The fork path now builds **on `Delivery::Commit`** — the foundation's only-live variant, a
> behavior-preserving re-type of today's commit path; the fork tip **is** that variant's `commit_oid`, and
> the contribution binds (`base_oid` / target / ancestry / custody / authorship) layer **on top of it**.
> The **patch** path is the foundation's **additive `Delivery::Tree` variant** (designed, not built) —
> **not a parallel money-path** (v4's "parallel typed money-path" cost is retired by the abstraction).
> **The money-safety design is UNCHANGED from v4** (coordinator CLEAR-FOR-BUILD stands, 23133); v5 only
> re-expresses delivery on the foundation and repoints scope/patch at PIECE-12. **Build decomposition:**
> **Step 0** = the PIECE-12 abstraction + `Commit` re-type (coordinator behavior-equivalence sub-pass —
> produced-byte equivalence + refuse-path parity + red-on-revert + suite green — gated *before* this) →
> **Step 1+** = this fork contribution (the 6 MUSTs) on `Commit`, full money-gate. **NO self-FF on
> pay-verify (either step).**
>
> **v4 — folds a codex-deep design review (compose → blinded adversarial (Opus) → codex, complete)**
> into the strong v3 draft. **v3 is preserved; v4 SHARPENS MECHANISMS — it does not re-open the design.**
> The refinements: the **authorship bind is re-centred on the seller's schnorr-signed kind-6109 result**
> (which commits to `{job_id, seller_pubkey, target_repo, base_oid, fork_ref, commit_oid}`) — a git
> commit-**trailer** is **downgraded to optional** in-commit provenance; delivery is stated as **ONE
> state machine — `verify → pay → merge`** (FF-preferred, buyer-custody); `base_oid` is resolved from the
> **pinned `target_repo`**, not the seller echo; seller result fields are **equality-checked against the
> buyer's signed offer, never treated as authority**; the content gate is **honestly scoped** (stops
> **empty / out-of-scope**, NOT worthless-in-scope — quality-judging stays deferred to the
> payment-and-reputation chapter); **custody-retention** is added (the buyer fetches + retains the paid
> object and merges by the local oid); and the new fields are enumerated as schema/state additions
> **adjacent to** authorization (frozen money-core byte-scope in v4 — **v5's Step-0 re-type supersedes**, see the v5 note above).
>
> **Two items were flagged PROPOSED-PENDING-COORDINATOR and are now CONFIRMED at coordinator
> shape-review** (folded below as RESOLVED, no longer pending): **(1)** the chapter-acceptance pay-bind
> wording — *pay binds the delivered FORK-TIP `commit_oid` (== the seller-signed 6109 `commit_oid`),
> merged/FF'd into target; a merge commit is NOT the paid object*; **(2)** the receipt stays **as-is**
> (attests the delivered object + `delivery_kind`, already landed in piece-9) with **no re-lock** —
> contribution-context (`target_repo`, `base_oid`) rides the **signed kind-6109** + the buyer's
> accept-bind/journal, so a receipt extension is not warranted (see § Receipt binding).
>
> **v3 lineage (preserved):** a refinement of the v2 adversarial draft (compose → blinded adversarial
> (Opus) → codex). v2's reshape is PRESERVED in full. v3 folds the now-**RESOLVED** operator rulings,
> reframes the goal (forge-as-buyer), separates delivery from settlement, adds a chapter-level
> acceptance, and **re-verifies every code ref against dev tip `0f05d9b`**. The money path is
> **commit-oid-typed end to end**; the descendant gate is still **greenfield** (no spike code in the
> source tree); **the receipt now DOES attest the delivered object** — piece-9 v3 landed the delivery
> binding (**option (a)**, @ `4190a15`). freelance-PR remains a **full money-adjacent build, not an
> additive doc change** (size acknowledged by gudnuf).

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

**Flow — ONE state machine (`verify → pay → merge`, stated identically everywhere):**
offer (pinned `target_repo` + `base_oid`) → seller forks + runs its agent + pushes `fork_ref` → seller
**result advertises `{target_repo, base_oid, fork_ref, commit_oid}` inside its schnorr-signed kind-6109**
→ **buyer verify** (all pre-pay, all against BUYER-CONTROLLED inputs):

1. **Custody fetch** — fetch the fork tip into a **buyer-controlled ref / object-store** (custody);
   record that local ref in the accept-bind. The buyer thereafter operates on the **local custody oid**,
   never the live fork branch name.
2. **base resolved from the PIN** — fetch `base_oid` from the **pinned `target_repo` (`naddr`)** into the
   same custody odb (NOT the seller-echoed value); fail-closed if `base_oid` is missing from the pinned
   target.
3. **Descendant** — peel both as commits; `git merge-base --is-ancestor <base_oid> <commit_oid>`.
4. **Tip-match** — fetched custody tip == `commit_oid`.
5. **Authorship** — verify the **seller's kind-6109 signature** over the tuple and that the fetched/paid
   commit == the signed `commit_oid` (the seller's own sig binds `seller_pubkey` → `commit_oid`).
6. **Content gate** — non-empty + in-scope (MUST #5; a floor, **not** a quality bar).

The seller-echoed `{target_repo, base_oid, fork_ref}` are **equality-checked against the buyer's signed
offer/accept-bind only** — a cross-check input, never authority; all fetch/merge policy comes from the
buyer's signed offer. → **`authorize_pay` binds payment to the delivered FORK-TIP `commit_oid`** (the
existing money path, unchanged) → **THEN merge** = the buyer/forge merges the **custodied `commit_oid`**
into `target_repo` ("accept the PR"), **FF-preferred** so the merged oid == the paid fork-tip → receipt
closes. Merge is a **buyer-custody** action and is **not** what payment binds — **payment binds the
verified FORK-TIP `commit_oid`, never a merge commit** (a non-FF merge commit is a different oid and is
not the paid object).

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

## Scope decision — v1 = `Commit` variant + FORK; PATCH = additive `Tree` variant, deferred (RULED)

**RULED GO — v1 = fork-only, built on the foundation's `Delivery::Commit` variant** (hearth Q3 + gudnuf).
The typed-delivery foundation ([PIECE-12](PIECE-12-TYPED-DELIVERY-ABSTRACTION.md)) **retires v4's
"parallel typed money-path" cost**: v4 held that supporting patch meant duplicating the commit-typed path
(a `Patch` variant across `AuthorizePayRequest`, `PaymentKey`, `DeliveryVerifier`, and the `== commit_oid`
gates); with the foundation, patch is instead an **additive `Delivery::Tree` variant** — a tree-oid variant
+ its verify arm added to the *one* abstraction, **not a parallel path**. **v1 ships the `Commit` variant +
the fork contribution; the `Tree` / patch variant is designed (§ Path B, PIECE-12) and explicitly NOT
built.**

---

## Path A — fork (v1)

The seller branches off the offer's `base_oid`, commits its agent's work, and pushes to its **own**
relay-git namespace (`default_relay_git_remote`, `home.rs:95` — owner-scoped push forces the fork; no
write access to the buyer's `target_repo`). It announces the fork (kind-30617, `profile.rs:421`) and its
kind-6109 **result advertises `{target_repo, base_oid, fork_ref, commit_oid}` inside the seller's schnorr
signature** (`fork_ref` = fork repo + branch; `commit_oid` = the fork tip). **Binding:
`delivery_integrity_hash = commit_oid`** (the fork tip) — the foundation's `Delivery::Commit` variant
(`delivery.rs:86` `from_fetched_tip` tip-match; PIECE-12), so v1 reuses the money path unchanged. Ordering is the ONE state
machine (**`verify → pay → merge`**): after the buyer binds payment to the fork-tip `commit_oid`, it
**merges the custodied `commit_oid` into `target_repo`** ("accept the PR", **FF-preferred**) — a
buyer-custody action, **not** what payment binds.

**MUSTs added for contribution (all NEW build — none exist today):**

1. **Thread the contribution binds** — `base_oid`, the pinned `target_repo`, `fork_ref`, and the
   **buyer-custody local ref** — from the offer → accept-bind → pay request → verifier (enumerated as
   schema/state additions in § Money-gate). Without `base_oid` the descendant gate cannot run and the
   offer's target is decorative. (Today: **0 hits** for `base_oid` in source.)
2. **Descendant gate (greenfield), base resolved from the PIN.** `base_oid` MUST be resolved/fetched
   from the **pinned `target_repo` (`naddr`)** — **NOT the seller-echoed value** — into the **same
   custody odb** as the fork tip; peel both as commits and refuse unless
   `git merge-base --is-ancestor <base_oid> <commit_oid>`; **fail-closed if `base_oid` is missing from
   the pinned target**. Closes unrelated-history / swapped-base advertisement. Ancestry proves lineage,
   **not meaningfulness** — any meaningful-contribution limits (diff-scope / object-size) live in the
   MUST #5 policy hook, not here. (New build — no `verify_git_descendant`/`merge-base` in source.)
3. **Authorship bind (closes replay/lift) — the seller's SIGNED kind-6109 result IS the bind.** A git
   trailer is **NOT sufficient**: trailer text is copyable and proves text-inclusion, not seller
   authorship, so a seller could be **paid for a third party's commit**. Instead, the seller's kind-6109
   **result event (already schnorr-signed by the seller)** MUST commit, in its signed payload, to the
   tuple `{job_id, seller_pubkey, target_repo, base_oid, fork_ref, commit_oid}`; the buyer verifies
   **(a)** the seller signature and **(b)** that the fetched/paid commit == the signed `commit_oid`. The
   seller's own signature thus cryptographically ties `seller_pubkey` → this `job_id` → this exact
   `commit_oid` — it **cannot be paid for a third party's commit**. The git commit-**trailer** is
   **downgraded to OPTIONAL** in-commit provenance (not the security bind). Optionally **also** require a
   git commit **signature** by the seller key where practical (belt-and-suspenders). (`job_hash` /
   `sig/seller` cover only the job-hash, not the commit — hence the signed tuple.)
4. **Pin `target_repo` as an `naddr` (owner pubkey + relay/clone URL), not a bare `d`-tag** (`.names` is
   global across owners → spoofable, `home.rs:86-87`). **Authority is the buyer's SIGNED offer, never the
   seller echo:** the seller result's `{target_repo, base_oid, fork_ref}` are **equality-checked against
   the buyer's signed offer / accept-bind ONLY** (a cross-check input), and **all fetch/merge policy —
   which repo, which base, where to fetch/merge — comes from the buyer's signed offer**. This closes the
   confused-deputy: the buyer never fetches, bases, or merges against a seller-provided value.
5. **Content / non-empty gate (autonomous MUST) — honestly scoped.** v1 contribution is autonomous (no
   human reads the diff before pay), so refuse a delivery whose diff against `base_oid` is **empty or
   out-of-scope** (does not touch the offered paths). **This is the floor, and only the floor: it stops
   EMPTY / OUT-OF-SCOPE deliveries — it is NOT a quality gate.** An in-scope-but-worthless diff can still
   pass and be paid; **judging quality is the hard problem, deferred to the payment-and-reputation
   chapter** (§ Settlement). Do **not** overclaim this gate as paid-worthless-grief prevention — it is
   not. To let a buyer tighten the floor, v1 exposes a **configurable buyer POLICY HOOK** (path allowlist
   + forbidden paths + max-diff-size + an optional CI/tests predicate) that **MAY** gate pre-pay; it is
   the home for any meaningful-contribution limits. (The post-pay **merge** is a separate forge review
   step, but pay must not depend on it — the content gate + policy hook are the autonomous floor.)
6. **Ref stability + custody retention.** Use a **per-job unique ref** (full `job_id`, not the
   `mobee/<job_id[:8]>` prefix that collides) and enforce **no-force-push as a contribution MUST** (a
   later push must not move the advertised `commit_oid` out from under a paid receipt). **Custody
   retention (NEW):** the verifier MUST fetch `commit_oid` into a **buyer-controlled ref / object-store**,
   **record that local ref in the accept-bind**, and **merge by the LOCAL `commit_oid`** — never the live
   fork branch name — so a seller who **deletes or moves the fork after pay cannot strand the buyer**.
   (No-force-push covers tip-move; custody-retention covers deletion too.)

## Path B — NIP-34 patch (kind-1617) — the additive `Delivery::Tree` variant (DESIGNED, NOT v1)

Seller publishes a kind-1617 patch against `base_oid`; **binding = the resulting TREE oid**
(content-deterministic — a commit oid can't work, patch application yields a per-applier commit). This is
the foundation's **additive `Delivery::Tree` variant** ([PIECE-12](PIECE-12-TYPED-DELIVERY-ABSTRACTION.md))
— **not a parallel money-path** (the v4 framing is superseded). Adding it = the `Tree` variant + its verify
arm on the one abstraction, plus:

- **Determinism pinned (money bind):** apply with filters disabled (no autocrlf/ident/clean-smudge),
  a fixed file-mode policy, and a pinned object-format (sha1/sha256) shared seller↔buyer — else honest
  parties compute different trees (false refuse) or an attacker games normalization.
- **Strict apply against `base_oid`:** no 3-way, no fuzz; clean-apply-failure = fail-closed refuse; the
  patch event pins `base_oid` so the tree is unambiguously against the named base.
- Same authorship bind (the seller's signed kind-6109 tuple — with the patch's **tree oid** as the bound
  delivered object) + `target_repo` pin + equality-check-not-authority as Path A.
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
>
> **Contribution-context — RESOLVED (keep the piece-9 receipt AS-IS; NO re-lock).** The kind-3400 receipt
> object + `delivery_kind` stay exactly as they landed in piece-9. Rationale: the seller's **signed
> kind-6109** already carries and signs `{target_repo, base_oid, commit_oid}` (and MUST #3 makes that
> signature the authorship anchor), so the contribution context is **already cryptographically recorded**
> there; the receipt itself already binds the paid `commit_oid` via `delivery_integrity_hash` + both
> co-sigs. Duplicating `target_repo` / `base_oid` into the kind-3400 would be **redundant if unsigned**
> (tamperable) or a **piece-9 re-lock if signed** (money-code churn on a LOCKED, teeth'd artifact — not
> warranted). Contribution context therefore lives authoritative in the **buyer accept-bind / journal +
> the signed kind-6109**. A self-describing receipt is an **optional FUTURE observatory follow-up, not
> this arc.**

## Offer shape (contribution mode; additive)

| tag | shape | meaning |
|-----|-------|---------|
| `job-class` | `["job-class","contribution"]` | absent ⇒ from-scratch (back-compat) |
| `target-repo` | `["target-repo","<owner_pubkey_hex>","<clone_url>"]` | the buyer's `target_repo`, pinned by **owner pubkey + explicit clone URL** (positional — nostr 0.44's `Nip19Coordinate` cannot carry an https clone URL, so v1 carries the naddr's money-relevant payload positionally; canonical bech32 `naddr` rendering = named observatory-interop follow-up). **`owner_pubkey` is BOUND, not fetch-enforced, by design (v1):** it rides the buyer's signed offer, the seller-signed authorship tuple, and the accept-time echo equality-check; fetch scoping rests on the buyer-signed `clone_url` (buyer authority — no confused-deputy). A fetch-time owner↔namespace cross-check folds into the bech32 follow-up. |
| `base` | `["base","<base_branch>","<base_oid>"]` | base branch + the exact `base_oid` the contribution must descend from |
| `accepts` | `["accepts","fork"]` (v1) | positional multi-value (`["accepts","fork","patch"]` when patch ships) — not comma-joined |

The kind-6109 **result** echoes `target_repo` + `base_oid` and adds `fork_ref` (repo + branch) +
`commit_oid` (the advertised tip) — **all inside the seller's schnorr signature**. The echoed
`target_repo` / `base_oid` are a **cross-check input only** (equality-checked against the buyer's signed
offer); **authority is the buyer's signed offer** — the buyer resolves `base_oid` from the pinned
`target_repo`, never from the seller echo (MUST #2, MUST #4).

## Back-compat (buyer refusal is the security boundary)

- **From-scratch unchanged** (no `job-class` ⇒ existing path).
- **Seller kind-7000 refusal = INTEROP courtesy, not a security control.** A seller without contribution
  support *should* emit a kind-7000 `status=error` on a `job-class=contribution` offer rather than
  silently run it as from-scratch and push to its own repo — but this is a **courtesy a legacy or
  malicious seller can ignore**, so **no money decision may rest on it**.
- **The NORMATIVE SECURITY BOUNDARY is BUYER-side.** The buyer MUST **refuse** any result whose delivery
  does not satisfy the contribution binds — custody fetch + descendant (base-from-pin) + authorship
  (seller-signed kind-6109 tuple) + `target_repo` / `base_oid` equality-check + content gate. **Only the
  buyer's refusal protects the money;** a well-behaved seller is a convenience, never the guarantee.

## Money-gate (coordinator's money bar)

The contribution verify-path (base-ancestry + fork-fetch + pay-bind) touches **`PayPathDeliveryVerifier`**
and the `authorize_pay` gates → it is subject to the **coordinator's money bar** before **any** FF that
touches the pay-verify path: **independent full-suite re-run on the frozen candidate + live fixtures +
dual-review (both frames).** **Frozen-core (post-foundation):** the typed-delivery re-type threads `Delivery` through
`authorize_pay` / `payment` / `delivery` in **Step 0** ([PIECE-12](PIECE-12-TYPED-DELIVERY-ABSTRACTION.md))
— gudnuf-authorized, **behavior-preserving only**, gated by the Step-0 behavior-equivalence sub-pass
(produced-byte equivalence + refuse-path parity + red-on-revert + suite green) **before** this chapter
builds on it. `payment_wallet.rs` stays **byte-frozen** (no wallet/spend logic change); `receipt.rs` stays
**as landed**. **Step 1+ (this chapter)** adds **threading + a verifier gate** on the `Commit` variant —
not wallet/authorize/payment logic rewrites.

**Fields threaded (schema/state additions ADJACENT to authorization — NOT wallet/payment rewrites):** the
accept-bind + `authorize_request` gain `base_oid`, the pinned `target_repo` (`naddr`), `fork_ref`, and the
**buyer-custody local ref**. These sit next to the existing `commit_oid` bind; **`payment_wallet.rs` stays byte-frozen**, and
the `authorize_pay` / `payment` changes are the Step-0 behavior-preserving re-type (PIECE-12) plus this
chapter's threading — no wallet/spend logic rewrite. The **receipt is not
extended** by this chapter (RESOLVED — see § Receipt binding): contribution-context rides the signed
kind-6109 + the buyer accept-bind/journal, so no kind-3400 re-lock is warranted.

## Findings — RESOLVED

1. **v1 scope = fork-only — GO.** Patch deferred as the additive `Delivery::Tree` variant (PIECE-12; not a
   parallel money-path). Ruled by hearth (Q3) + gudnuf.
2. **Receipt binding — RESOLVED via option (a).** piece-9 re-locked to attest `delivery_integrity_hash`
   + `delivery_kind` in the kind-3400 schema **and** its co-signed preimage; **landed in piece-9 v3 @
   `4190a15`** (code present at dev tip — see § Receipt binding). The receipt **does** attest the
   delivered object.
3. **Scope = full money-adjacent build — acknowledged.** freelance-PR is a full money-adjacent build
   (`base_oid` threading + greenfield descendant gate + authorship bind + `target_repo` pin + content
   gate + runtime refuse) — **not** the additive doc change the original charter framed. gudnuf
   **size-acked** this. The fork path fits the existing commit-typed money bind; the rest is new.
4. **codex-deep design refinements — FOLDED (v4).** The compose→adversarial→codex pass sharpened
   mechanisms without re-opening the design: authorship re-centred on the **seller-signed kind-6109
   tuple** (git trailer → optional, MUST #3); ONE `verify → pay → merge` state machine, FF-preferred,
   buyer custody (§ Delivery model, MUST #6); `base_oid` resolved from the pin (MUST #2); seller fields
   **equality-checked, not authority** (MUST #4); content gate **honestly scoped** to empty/out-of-scope
   + policy hook (MUST #5); fields enumerated adjacent to authorization (§ Money-gate); buyer-refusal as
   the security boundary (§ Back-compat). The two items flagged PROPOSED-PENDING were **CONFIRMED at
   coordinator shape-review** (RESOLVED): the chapter-acceptance pay-bind wording, and no receipt
   extension (contribution-context rides the signed kind-6109 + accept-bind — § Receipt binding).

## Acceptance — SPEC-DOC bar

*(the bar for THIS doc; the chapter bar is below and distinct)*

- Offer fields (contribution) specified + differ from from-scratch.
- **Fork path (v1)** fully specified incl. the six MUSTs, built on the foundation's
  `Delivery::Commit` variant (PIECE-12); patch path designed + deferred as the additive `Delivery::Tree`
  variant (no parallel money-path).
- **Delivery is ONE state machine — `verify → pay → merge`** (FF-preferred, buyer-custody) — stated
  identically in the intro, § Delivery model, Path A, and both acceptance bars.
- Pay binding per path stated (fork = the delivered **FORK-TIP `commit_oid`** tip-match reused, **never a
  merge commit**; patch = tree-oid, deferred) with the commit-vs-tree type-confusion + determinism
  hazards named.
- Descendant (base-from-pin) + authorship (**seller-signed kind-6109 tuple**, git trailer optional) +
  `target_repo`-identity (equality-check, not authority) + custody-retention + honestly-scoped content
  gate specified as NEW MUSTs (greenfield).
- Receipt binding resolved (option (a), landed @ `4190a15`) — recorded, not left open; v4 **confirms the
  receipt is NOT extended** (contribution-context rides the signed kind-6109 + accept-bind).
- Delivery ⊥ settlement; settlement is status-quo verify-then-pay AS-IS; grace + escrow + reputation
  deferred to a future payment-and-reputation chapter (quality-judging is the hard problem, not
  atomicity), with freelance-PR as its testing vehicle.
- Back-compat: **buyer-refusal is the normative security boundary**; the seller kind-7000 refusal is an
  interop courtesy, not honor-system money protection.
- Code refs re-verified against dev tip `0f05d9b`; moved refs updated.

## Acceptance — CHAPTER (freelance-PR is REAL)

*(distinct from the spec-doc bar above; this is the bar for the built chapter — the forge actually
hiring a mobee to do forge work)*

```
acceptance (chapter):
  # ordering is ONE state machine: verify -> pay -> merge (see § Delivery model)
  return_predicate: >
    A REAL forge job targeting a REAL relay-git forge repo is posted; a mobee seller
    (turtle or external) forks the target, runs its agent, and delivers a kind-6109 result
    SCHNORR-SIGNED BY THE SELLER over {job_id, seller_pubkey, target_repo, base_oid, fork_ref,
    commit_oid}, where commit_oid DESCENDS from base_oid; the BUYER verify-path fetches the fork
    commit INTO BUYER CUSTODY, resolves base_oid from the PINNED target, and asserts
    base-ancestry + tip-match + seller-authorship (the 6109 signature binds seller_pubkey to
    commit_oid) + content gate; pay binds the delivered FORK-TIP commit_oid (== the seller-signed
    6109 commit_oid), merged/FF'd into target; a merge commit is NOT the paid object; a
    kind-3400 receipt closes with BOTH co-sigs verifying (independent teeth); full suite green on
    the frozen FF candidate; NON-mock — the PR is agent-authored real work merged into a real
    forge repo.
  non_counting:
    - a from-scratch artifact job (not a contribution against a target repo)
    - a contribution "delivery" that is never actually merged into the target
    - suite-green without a live real-forge-job -> PR -> merge -> pay leg
    - base-ancestry left unchecked
    - pay bound to a commit_oid != the seller-signed kind-6109 commit_oid
    - authorship established by a git trailer rather than the signed kind-6109
    - verify that binds/pays BEFORE fetching commit_oid into buyer custody
    - base_oid taken from the seller echo rather than the buyer's pinned target
```

## Fence / reality class

**FORK PATH BUILT (Step-1 landed); chapter close pending the natural trade.** From-scratch delivery is
**PROVEN** and the collect leg **REAL-AND-LIVE**. The fork contribution path is **BUILT on
`Delivery::Commit`** ([PIECE-12](PIECE-12-TYPED-DELIVERY-ABSTRACTION.md)) with all six MUSTs live —
base-from-pin, descendant, authorship via the seller-signed kind-6109 tuple (extending the pre-pay
co-sig seam), target_repo equality-check, content gate + policy hook, buyer-custody retention — and the
assembled verify→pay glue is **live-proven** (a full contribution trade against a real relay-git target:
fork → signed tuple → custody fetch → descendant → pay → co-signed receipt, both signatures verified).
A contribution offer cannot be paid without its gates (the explicit pay form refuses fail-closed absent
an accept-bind). Remaining: the buyer **post-path wiring** for contribution offers (thin follow-up — the
canonical tag constructor exists; the MCP post tool does not yet emit it), after which the chapter closes
on a natural forge-hire. The patch path stays the additive `Delivery::Tree` variant (designed, not built).

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
- Built on the typed-delivery foundation
  [PIECE-12-TYPED-DELIVERY-ABSTRACTION.md](PIECE-12-TYPED-DELIVERY-ABSTRACTION.md) (fork = `Delivery::Commit`;
  patch = the additive `Delivery::Tree`). Composes onto [PIECE-7-GIT-DELIVERY.md](PIECE-7-GIT-DELIVERY.md);
  receipt binding per [PIECE-9-RECEIPT-AND-EXEC-METADATA.md](PIECE-9-RECEIPT-AND-EXEC-METADATA.md) (D4, @
  `4190a15`).
