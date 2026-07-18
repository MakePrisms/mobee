# Piece-12 — typed `Delivery` abstraction (foundation)

Make **delivery a first-class typed value** — one `Delivery` enum over *the delivered git object* — so
new delivery forms (patch/tree) are added as **variants of one abstraction**, never as a parallel
money-path. This is the **foundation** freelance-PR (piece-10) builds its fork path on, and the seam a
future patch-delivery piece slots into.

> **Design-only, foundation-first.** This doc lands and is shape-confirmed **on its own**, *before*
> piece-10's freelance-PR plan is reworked on top of it (gudnuf's sequencing: settle the foundation, then
> the feature). It specifies **one thing**: the typed `Delivery`, its **`Commit`** variant as a
> **behavior-preserving re-type** of today's commit path, a **`Tree`** variant that is **designed but NOT
> built**, how the type threads through the money-core, and the **money-gate behavior-equivalence
> requirement** for the `Commit` re-type. **No feature logic here** — freelance-PR's contribution binds
> (base_oid / ancestry / custody / authorship) live in piece-10, layered on the `Commit` variant.
>
> **Class: MONEY-adjacent.** The re-type **threads through the frozen money-core**
> (`authorize_pay` / `payment` / `delivery`) — the "flag if the arc needs it" case; **gudnuf authorized
> it**, scoped to a **behavior-preserving re-type only**. Verified against `origin/dev` `0f05d9b`.

---

## Why — delivery is implicit today

Delivery is currently a **loose set of fields**, not a type. A seller's result advertises `repo` /
`branch` / `commit_oid` (`delivery.rs:31` `GitDelivery`); the buyer fetches the branch tip, peels
`^{commit}`, and tip-matches (`delivery.rs:86` `VerifiedDelivery::from_fetched_tip`, via
`DeliveryVerifier::verify(&GitDelivery)` `:108`); `authorize_pay` binds
`delivery_integrity_hash == commit_oid` (`authorize_pay.rs:162`) and stamps the receipt with a
`DeliveryKind` discriminator that is **hardcoded to `Fork`** (`authorize_pay.rs:305`; enum
`receipt.rs:65`).

The delivered **object type is commit-oid, baked in everywhere** — the fetch peels `^{commit}`, the bound
hash is a commit oid, `DeliveryKind::Fork` is the only live kind. Adding a **patch (tree-oid)** delivery
under this shape means duplicating the whole typed path (`AuthorizePayRequest`, the verifier, the
`== commit_oid` gate) — piece-10 v4 named this the **"parallel typed money-path"** cost. The typed
abstraction removes that: **one `Delivery` type, patch added as a variant.**

## The typed `Delivery`

A `Delivery` enum over **the delivered git object** — what `delivery_integrity_hash` binds, and how the
verifier proves it into buyer custody:

- **`Delivery::Commit { repo, branch, commit_oid }` — the ONLY LIVE variant.** A **behavior-preserving
  re-type** of today's commit path. Bound object = `commit_oid`; verify = fetch the branch tip + peel
  `^{commit}` + tip-match — the **exact existing path** (`delivery_git.rs:88-124`, `delivery.rs:86`),
  yielding today's `VerifiedDelivery`. **Nothing about the money path's behavior changes — only its
  type.** (This is the variant piece-10's fork contribution layers on: the fork tip *is* the `Commit`'s
  `commit_oid`.)
- **`Delivery::Tree { base_oid, tree_oid, … }` — DESIGNED, NOT BUILT (additive).** Bound object = the
  resulting **tree oid** (content-deterministic; a commit oid can't work because patch application yields
  a per-applier commit). Verify = strict apply of a NIP-34 patch against `base_oid` + tree compare, under
  a pinned determinism policy (filters off, fixed mode/object-format). **Adding it later is adding this
  variant + its verify arm — NOT a parallel money-path.** It is enumerated here so the abstraction is
  shaped to receive it; **no `Tree` verify arm is built in v1.**

`DeliveryVerifier::verify` dispatches on the variant (`Commit` → the existing fetch/peel/tip-match;
`Tree` → deferred). `VerifiedDelivery` stays the custody proof; the oid the money path binds is the
variant's bound object (`Commit` → `commit_oid`, unchanged).

### Correspondence to the landed wire tag (unchanged)

The variants are named by **git object** (`Commit` / `Tree`) — the axis the money bind actually turns on.
The **landed** kind-3400 discriminator `DeliveryKind` (`receipt.rs:65`) names the **seller-path**
(`Fork` / `Patch`). In v1 these are **1:1**:

| typed `Delivery` (in-code) | seller-path | landed `DeliveryKind` wire tag | bound oid |
|---|---|---|---|
| `Commit` | fork | `Fork` → `"fork"` | `commit_oid` |
| `Tree` *(designed)* | patch | `Patch` → `"patch"` | `tree_oid` |

The typed enum is the **in-code / verify-path type**; the receipt's `DeliveryKind` is the **wire tag** and
stays **exactly as piece-9 landed it (no re-lock).** The re-type only changes *where* `delivery_kind`
comes from — **derived from the `Delivery` variant** instead of the `authorize_pay.rs:305` hardcode — and
for the sole live variant `Commit → DeliveryKind::Fork → "fork"`, i.e. **byte-identical to the current
hardcode.**

> **Open naming call for shape-confirm:** variants by **object** (`Commit`/`Tree`, my pick — self-states
> what the hash binds, and matches how the abstraction was scoped) vs. by **seller-path** (`Fork`/`Patch`,
> mirroring the existing `DeliveryKind`). Either way `DeliveryKind` on the wire is untouched.

## How it types through the money-core

The `Delivery` type threads through three files; the change is **mechanical typing, not logic**:

- **`delivery.rs`** — home of the `Delivery` enum + the per-variant dispatch in `DeliveryVerifier::verify`.
  The `Commit` arm calls the existing `from_fetched_tip` path unchanged. `GitDelivery` / `VerifiedDelivery`
  / `CommitOid` are retained (they *are* the `Commit` variant's payload + proof).
- **`authorize_pay.rs`** — builds the pay request + the co-signed receipt preimage from the verified
  delivery. The `delivery_integrity_hash == commit_oid` gate (`:162`) becomes "== the variant's bound oid"
  (`Commit → commit_oid`, identical). `delivery_kind` in the preimage (`:296-305`) is **derived from the
  variant** rather than hardcoded — `Commit` yields `Fork`/`"fork"`, the current value.
- **`payment.rs`** — the payment state carries the typed delivery where it carries the commit fields
  today; behavior-identical for `Commit`.
- **`payment_wallet.rs` — UNTOUCHED.** Wallet/spend logic never sees the delivery type. **Byte-frozen.**
- **`receipt.rs` — as landed.** `DeliveryKind` + the preimage fields (`delivery_integrity_hash`,
  `delivery_kind`) are unchanged; the re-type only *populates* `delivery_kind` from the variant.

### Frozen-core scope (gudnuf-authorized)

The re-type **does change `authorize_pay` / `payment` / `delivery`** — this is the deliberate, authorized
exception to byte-freezing the money-core. It is scoped to a **behavior-preserving re-type ONLY**: **no
logic change to pay/wallet beyond the delivery typing.** `payment_wallet.rs` stays **byte-frozen**;
`receipt.rs` stays **as landed**.

## Money-gate — behavior-equivalence requirement (the `Commit` re-type)

Because the re-type touches money-core, the `Commit` variant **MUST be proven behavior-identical to
today's commit path** before anything builds on it. Coordinator money-gate **sub-pass**, gated **before**
Step 1+ (piece-10 fork), **no self-FF**:

```
acceptance (Step-0 behavior-equivalence — the Commit re-type):
  return_predicate: >
    The typed Delivery lands with a Commit variant that is a behavior-preserving re-type of the
    existing commit path. For a commit delivery, the PRODUCED bytes are byte-identical pre- and
    post-re-type: the receipt preimage (canonical_json), the delivery_integrity_hash pay-bind,
    and the kind-3400 delivery_kind tag ("fork") — equivalence is over the PRODUCED bytes / wire
    (the source necessarily changes, so it is not a source diff). Red-on-revert proves the typing
    is load-bearing (breaking the Commit dispatch or the bound-oid selection turns a passing
    commit trade red). Full suite green on the frozen candidate. payment_wallet.rs byte-frozen;
    receipt.rs as-landed.
  non_counting:
    - a source-diff "equivalence" instead of produced-byte / wire equivalence
    - suite green without the byte-equivalence check on a real commit delivery
    - any pay/wallet logic change smuggled in under "typing"
    - a built Tree verify arm (v1 builds none)
    - self-FF (money-core touch → coordinator independent gate)
```

## Build scope

- **This doc's build = Step 0.** Introduce `Delivery`; re-type the commit path onto `Delivery::Commit`;
  thread through `authorize_pay` / `payment` / `delivery`; no behavior change. `Delivery::Tree` is added
  to the enum **without a verify arm** (or omitted until the patch piece) — **not built** in v1. →
  coordinator behavior-equivalence gate (above).
- **Step 1+ = piece-10 freelance-PR fork, on the landed `Commit` variant** — separate arc, separate doc,
  its own full money-gate. Not in scope here.

## Non-goals

Not building `Tree`/patch delivery · not changing settlement · not changing the receipt object/kind (no
piece-9 re-lock) · not changing wallet/spend logic · no freelance-PR contribution logic (that is piece-10).

## Acceptance — DESIGN-DOC bar

- One typed `Delivery` specified: variants, per-variant verify, and the oid each binds.
- `Commit` = the only live variant, a **behavior-preserving re-type** of today's commit path; its
  behavior-equivalence requirement stated as the money-gate sub-pass (produced-byte equivalence +
  red-on-revert + suite green).
- `Tree` = **additive, designed-not-built** — resolving v4's "patch = parallel money-path" (now a variant,
  not a parallel path).
- Types-through enumerated: `authorize_pay` / `payment` / `delivery` re-typed behavior-preserving
  (gudnuf-authorized); `payment_wallet` byte-frozen; `receipt` as-landed (wire `DeliveryKind` unchanged).
- Correspondence to the landed `DeliveryKind` stated; naming call flagged.
- Refs verified against dev tip `0f05d9b`.

## Reference (verified @ `0f05d9b`)

- **Current delivery surface:** `delivery.rs:5` `CommitOid`, `:31` `GitDelivery{repo,branch,commit_oid}`,
  `:80` `VerifiedDelivery`, `:86` `from_fetched_tip` (tip-match), `:108` `DeliveryVerifier::verify`.
  Fetch + peel: `delivery_git.rs:88-124` (`^{commit}` peel `:108`). Transport allowlist / pay-path
  factory: `PayPathDeliveryVerifier` `delivery_git.rs:159-160`.
- **Money bind:** `authorize_pay.rs:162` (`!= commit_oid` refuse), `:296-305` preimage build
  (`DeliveryKind::Fork` hardcode `:305`).
- **Landed wire tag:** `receipt.rs:65` `enum DeliveryKind{Fork,Patch}` (labels `"fork"`/`"patch"` `:76-77`),
  preimage `delivery_kind` `:112` in `canonical_json` `:130`.
- Composes onto / generalizes [PIECE-7-GIT-DELIVERY.md](PIECE-7-GIT-DELIVERY.md); receipt binding per
  [PIECE-9-RECEIPT-AND-EXEC-METADATA.md](PIECE-9-RECEIPT-AND-EXEC-METADATA.md) (unchanged). Foundation for
  [PIECE-10-FREELANCE-PR-DELIVERY.md](PIECE-10-FREELANCE-PR-DELIVERY.md) (fork = `Commit` variant + binds).

## Fence / reality class

**BUILT (Step 0 landed).** The typed `Delivery` is live money-core: the commit path is re-typed onto
`Delivery::Commit` (behavior-preserving — produced-byte equivalence proven pre- vs post-re-type via the
in-tree equiv harness, coordinator-gated), and the fork contribution path (piece-10 Step-1) builds on it.
`Tree` remains designed-only (lands with the future patch piece). Commit delivery is **REAL-AND-LIVE**.
