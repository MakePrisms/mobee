# Piece-14 — the mobee v2 microstandard

**Two moves in one version.** (1) **Let the party getting paid author the payment terms.** The offer stops
naming a mint; the seller's **claim** carries a **NUT-18 payment request** (`creq…`) naming the mint(s) it
accepts, the amount, the unit, and the transport. The claim *is* the invoice. The buyer satisfies it —
bridging over Lightning if its balance lives at a different mint — and payment/receipt binding moves onto
the request hash the seller authored. (2) **Leave the NIP-90 namespace entirely.** v2 events live in a
dedicated, contiguous kind block (`3400`–`3405`) plus one addressable heartbeat kind (`30340`), every one
of them stamped with a required `t=mobee` tag. The old `5109/6109/7000` kinds and the `+1000`
request/response convention are gone.

Buyer-sets-mint was backwards: in every real marketplace the invoice comes from the seller, because the
seller is the one who has to redeem the money and knows which mints it trusts. And borrowing the NIP-90 DVM
kinds bought nothing but explorer noise — NIP-90 is marked unrecommended upstream, and mobee never used its
job-request/result symmetry. v2 fixes both.

> **Status: design proposal, DOC-ONLY.** This document proposes a **breaking wire change**: the offer
> schema loses `mint_url`; every event kind is renumbered into the mobee `34xx` block; a `t=mobee`
> namespace guard becomes mandatory; and the flow adopts the NUT-18 `PaymentRequest`/`PaymentRequestPayload`
> types that the pinned cashu crate **already ships but the repo does not yet use** (§ NUT-18). It changes
> the money-safety path (claim construction, the pay gate, the receipt bind, the seller redeem guard);
> those files are gate-fenced and their build jobs are flagged for buyer-side review (§ Build
> decomposition). No escrow, no atomicity, no delivery-vs-payment coupling, and no payment-payload locking
> condition are introduced — failure semantics stay simple on purpose and are carried by trust +
> reputation (§ Failure semantics, § Metadata trust levels).
>
> **Anchored to:** `dev@16390ad`. All `file:line` references are to that tree.
>
> **Class: money-adjacent.** This touches `payment.rs`, `payment_send.rs`, `payment_wallet.rs`,
> `authorize_pay.rs`, `receipt.rs`, and the seller daemon money path. Every change point is cited below
> with a before→after. Sibling non-money design: [`PIECE-13-SELLER-MEMORY.md`](PIECE-13-SELLER-MEMORY.md).

---

## Motivation

**Buyer-sets-mint is backwards.** The offer authored by the buyer names the mint
(`OfferDraft.mint_url` at `gateway.rs:55`, emitted as the `mint` tag at `gateway.rs:101`, parsed back into
`ParsedOffer.mint_url` at `gateway.rs:120`/`:305`). The seller can only take it or leave it: it
fail-closes at boot unless its configured mint equals the one hardcoded default
(`seller_daemon.rs:330-333`) and skips any offer whose `mint_url` differs (`seller_daemon.rs:522-524`).
That is the wrong party holding the pen. The seller is the one who must *redeem* the ecash and bear the
counterparty risk of a bad mint; it — not the buyer — should say which mints it will accept.

**Receiver-authored terms solve fragmentation and fake-token verification at once.** Because the buyer
picks the mint, two buyers with balances at two mints can never pay the same seller without the seller
first agreeing to both — liquidity fragments across mints and offers silently go unclaimed. And the
current envelope carries a buyer-declared `mint_url` (`payment_send.rs:9`, `:295`) that the seller must
cross-check against buyer-declared terms — the same terms the buyer authored — a self-referential loop.
When the *seller* authors a NUT-18 request listing the mints it accepts (`m`), the amount (`a`), the unit
(`u`), and the transport (`t`), the buyer's job becomes "satisfy this request or bridge to a mint on the
list," the binding anchors to the seller-authored request hash rather than to buyer-declared numbers, and
fragmentation is handled by the buyer's wallet bridging over Lightning to an accepted mint.

**The NIP-90 namespace was a liability, not an asset.** mobee's kinds were borrowed from NIP-90's DVM
range (`5109` offer, `6109` result, `7000` feedback) on the `+1000` request/response convention. NIP-90 is
marked unrecommended upstream; mobee never used its job-request/result pairing semantics, and living in a
shared DVM range meant mobee events showed up as noise in every DVM explorer and vice versa. v2 leaves the
range entirely for a dedicated, contiguous mobee block and gates membership with a required `t=mobee` tag
(§ Kinds).

---

## Kinds — the v2 mobee block

Every v2 event lives in a dedicated block and carries a mandatory `["t", "mobee"]` **namespace guard**.
There is no `+1000` convention and no DVM range reuse.

| Kind | Object | Author | Notes |
|---|---|---|---|
| `3400` | **RECEIPT** | buyer + seller (co-signed) | number unchanged from v1 |
| `3401` | **OFFER** | buyer | work spec + price + award mode + eligibility |
| `3402` | **CLAIM** (bid + invoice) | seller | carries the NUT-18 `creq` quote |
| `3403` | **RESULT** | seller | typed delivery (§ Delivery seam) |
| `3404` | **FEEDBACK** (progress / error / refusal) | seller | closed reason-code enum (§ Failure semantics) |
| `3405` | **AWARD** | buyer | bid-mode winner selection (§ Award modes) |
| `30340` | **SELLER HEARTBEAT** | seller | addressable, `d="mobee-seller"` (§ Heartbeat) |

- **`t=mobee` is required on all seven kinds.** Every v2 parser rejects an event that lacks the
  `["t","mobee"]` tag before reading any other field — the namespace guard. An event in a mobee kind
  without the guard is not a mobee event.
- **Kinds `3400`–`3405` are a contiguous, mobee-owned block.** v1's `5109/6109/7000` are simply *different
  kinds*; a v2 parser never matches them (§ Compatibility).
- **The claim and feedback split.** In v1 both the claim (`status=processing`) and error/progress rode a
  single kind `7000`. In v2 the claim is its own kind `3402` (pure bid + invoice) and all
  progress/error/refusal moves to `3404` (§ The flow, § Failure semantics).

### Heartbeat / liveness — addressable kind `30340`

A seller advertises liveness with an **addressable** replaceable event, `d="mobee-seller"`, republished on
a ~5-minute cadence. Payload fields:

- `accepting` — `y`/`n` (is the seller taking new work right now)
- `queue_depth` — current in-flight job count
- `rate` — the seller's advertised rate (sats)
- `protocol_versions` — the mobee protocol versions this seller speaks (feeds `min_protocol_version`
  eligibility, § Eligibility)
- plus `t=mobee` and `d="mobee-seller"`.

> **NIP-01 constraint:** addressable (parameterized-replaceable) kinds MUST be in the `30000`–`39999`
> range — hence `30340`, not a `34xx` value. Consumers key a heartbeat by **`(pubkey, d)`**, never by
> event id: an addressable event is superseded in place, so an old id goes empty and a by-id lookup would
> read as "seller gone." Always resolve the latest by author + `d`.

---

## The flow

Five events on the trade path (heartbeat is out-of-band). The substantive changes from v1: **new kinds**,
**`mint_url` leaves the offer and the seller authors a `creq` in the claim**, and **an award step for
pooled bid offers**.

1. **OFFER — kind 3401.** Work spec + price. Carries `task`, `output`, `amount`, `unit`, `deadline_unix`,
   an **award mode** (`mode ∈ direct | first-claim | bid`, § Award modes), optional `seller_pubkey`
   (targeted), optional **eligibility `require` tags** (non-directed modes only, § Eligibility). **No
   `mint` tag / `mint_url` field.**

2. **CLAIM — kind 3402.** The seller accepts (or, in `bid` mode, bids) and attaches a **NUT-18 payment
   request** (`creq…`) it authored: accepted mint(s) in `m`, `a = offer.amount`, `u = offer.unit`, `t` = a
   nostr transport addressed to the seller's own key (NIP-17). **The claim is the invoice, and its `creq`
   is the seller's quote.** The `creq` string is attached as a dedicated `["creq", "creqA…"]` tag on the
   `claim_draft`, not the content field.
   *Justification (one sentence):* every other mobee protocol field is a nostr tag (amount, deadline, e/p,
   `t`, `v`) and the claim content is `""`, so a tag keeps the parser uniform, keeps the event filterable,
   and the `creq` is self-describing so it needs no sibling tags.

3. **AWARD — kind 3405 (bid mode only).** In `bid` mode the buyer selects among the collected `3402` bids
   and publishes a buyer-signed award e-tagging the offer and the winning claim; only the awarded seller
   works (§ Award modes). `direct` and `first-claim` modes skip this step.

4. **RESULT — kind 3403.** Delivery of the work. Delivery is a **typed field** (`type=fork` in v2,
   § Delivery seam) carrying the fork ref, commit oid, `delivery_integrity_hash`, and `job-hash`. Result
   metadata carries a `metadata_trust` level (§ Metadata trust levels).

5. **PAY + RECEIPT — kind 3400.** The buyer parses the seller's `creq`, checks `a`/`u` against the offer,
   and satisfies the request: it produces a cashu token **from one of the mints in `m`** and returns a
   NUT-18 `PaymentRequestPayload` over the stated transport (NIP-17 DM). **If the buyer's balance is at a
   mint not in `m`, its wallet bridges via Lightning** (§ Lightning bridge). Then buyer and seller co-sign
   a `3400` receipt whose preimage binds the **`creq_hash`** and the *realized* mint (§ Binding).

Progress, errors, and refusals at any step are `3404` FEEDBACK events with a machine-readable reason code
(§ Failure semantics) — never silent drops.

---

## Award modes

The offer's `mode` tag selects how a claim becomes an award. `mode ∈ direct | first-claim | bid`.

- **`direct`** — the offer targets a `seller_pubkey`. A `3402` claim from that seller **is** the award;
  work starts immediately. This is the default when the offer is targeted. Eligibility `require` tags are
  invalid here (there is nothing to filter — the buyer already picked the seller).

- **`first-claim`** — open pool; the **first eligible** `3402` claim wins instantly. Opt-in, intended for
  cheap micro-jobs where losing a race costs little. Sellers may start optimistically on claim; the buyer
  pays only the earliest eligible claim (tiebreak by `created_at`), and losers' claims expire unpaid. No
  `3405` award event is published — the winning claim is self-awarding.

- **`bid`** — open pool, the default for untargeted offers. Each `3402` claim is a **bid**; because every
  claim carries its own `creq` quote, the bid set *is* rate discovery. Sellers do **not** start work on
  claim. Within a bid window the buyer publishes a `3405` **AWARD** event and only the awarded seller(s)
  work.
  - **Award event carrier:** a dedicated kind **`3405`**, buyer-signed, e-tagging the offer and the
    winning claim. It is *not* a tag-marked `3404`: `3404` is the seller-authored feedback kind, and a
    buyer-authored award should not ride a seller's kind.
  - **`award_count`** (offer tag, default `1`): the buyer awards `N` winners, publishes one `3405` per
    winner, and expects/pays `N` deliveries — the tournament / panel pattern (e.g. hire three sellers, pay
    all three, keep the best).

---

## Eligibility requirements

Non-directed offers (`first-claim`, `bid`) may carry `require` tags — a **closed vocabulary of
log-verifiable predicates only**. Nothing here depends on unverifiable seller claims.

| `require` predicate | Meaning | Verified against |
|---|---|---|
| `min_receipts` | seller has ≥ N co-signed `3400` receipts | relay history |
| `min_completion_rate` | seller's deliveries ÷ awards ≥ r | relay history |
| `harness` | seller runs a named harness (claude/cursor/codex) | seller heartbeat / claim declaration |
| `min_protocol_version` | seller speaks mobee protocol ≥ v | seller heartbeat `protocol_versions` |
| `mint_overlap` | seller's accepted mints intersect the buyer's payable set | the claim's `creq` `m` list |

- **Sellers SHOULD self-gate:** a seller that reads a `require` it does not meet declines with a `3404`
  reason `ineligible` rather than claiming.
- **Buyers MUST enforce at award:** the buyer re-checks every requirement against relay history before
  publishing a `3405` (or before paying a `first-claim` winner). Self-gating is a courtesy; award-time
  enforcement is the guarantee.
- **Invalid in `direct` mode** — a targeted offer has already chosen its seller.

> **Sybil caveat (explicit):** receipt counts and completion rates are **sock-puppetable** until
> reputation weighting exists — a seller can trade with its own buyer sock puppets to inflate
> `min_receipts`. Eligibility `require` tags are **filters**, not trust. Reputation (a separate chapter,
> § Out of scope) is trust. They are different layers; do not read a satisfied `require` as a trusted
> counterparty.

---

## Metadata trust levels

Result metadata (execution facts — harness, model, token counts, transcript pointers) carries a
`metadata_trust` level declaring **how much the buyer should believe it**:

| `metadata_trust` | Meaning | v2 |
|---|---|---|
| `seller-claimed` | the seller asserts it; nothing binds it | **shipped** |
| `replay-auditable` | the result binds a transcript hash; the seller must produce the transcript on audit; the buyer replays sampled runs | **shipped** |
| `provider-signed` | a third-party provider (e.g. the model API) co-signs the metadata | defined, **unshipped** |
| `tee-attested` | a trusted-execution-environment attestation binds the run | defined, **unshipped** |

- **v2 ships `seller-claimed` + `replay-auditable`.** For `replay-auditable`, a result MAY bind a
  transcript hash; the seller must be able to produce the full transcript on demand, and the buyer audits
  by replaying a sample and comparing. The other two levels are named now so the field's value space is
  stable, but are not implemented.
- **No lock/condition field in the payment payload.** The NUT-18 `PaymentRequest` supports a `nut10`
  locking condition; v2 does **not** set it. Coupling payment to a delivery/attestation condition is
  deferred (YAGNI ruling) — failure semantics stay simple (§ Failure semantics).

---

## Delivery seam (typed)

Delivery is a **typed field** on the `3403` result: `type=fork` in v2. This is today's relay-git flow —
the seller forks the buyer's repo and pushes a branch, and the result carries the fork ref, the commit
oid, the `delivery_integrity_hash`, and the `job-hash`. **The target repo must be pre-announced** (the
buyer names the repo/branch on the offer; a seller cannot deliver against a repo the buyer never declared)
— this rule is unchanged and is stated here explicitly because it is load-bearing for the fork type.

Future delivery types (`bundle`, `patch`, …) slot in **without a version bump**: a v2 consumer that meets
an unknown delivery `type` **refuses it with a `3404` reason** rather than misparsing it as a fork. The
type field is the seam; adding a type is additive.

---

## Exact change inventory

Every change point below is grouped and given a before→after. `file:line` is `dev@16390ad`.

### (a) Schema / offer + kinds — `gateway.rs`

- **Kind renumbering — `gateway.rs:8-13`.** *Before:* `JOB_OFFER_KIND = 5109`, `JOB_RESULT_KIND = 6109`,
  `JOB_FEEDBACK_KIND = 7000`, `JOB_RECEIPT_KIND = 3400`. *After:* `JOB_OFFER_KIND = 3401`,
  `JOB_CLAIM_KIND = 3402` (**new** — split out of the old `7000`), `JOB_RESULT_KIND = 3403`,
  `JOB_FEEDBACK_KIND = 3404`, `JOB_RECEIPT_KIND = 3400` (unchanged), `JOB_AWARD_KIND = 3405` (**new**),
  `SELLER_HEARTBEAT_KIND = 30340` (**new**, addressable). Call sites reference these by name, so the value
  changes do not ripple beyond re-pointing the claim draft to `JOB_CLAIM_KIND` and adding award/heartbeat
  drafts (§ Job A′).
- **`t=mobee` namespace guard.** *Before:* no namespace tag. *After:* `to_event_draft`/all draft builders
  emit `["t","mobee"]`; every parser (`parse_offer` `gateway.rs:253`, result/claim/feedback/receipt
  parsers) rejects an event lacking it before reading other fields.
- **`OfferDraft.mint_url` field — `gateway.rs:55`.** *Before:* `pub mint_url: String,`. *After:* removed.
- **`OfferDraft::to_event_draft` mint tag — `gateway.rs:101`.** *Before:*
  `TagSpec::new(["mint", &self.mint_url])`. *After:* line removed (no `mint` tag emitted).
- **`ParsedOffer.mint_url` field — `gateway.rs:120`.** *Before:* `pub mint_url: String,`. *After:* removed.
- **`parse_offer` mint read — `gateway.rs:305-306`.** *Before:*
  `mint_url: first_tag_value(&event.tags, "mint").ok_or(OfferParseError::MissingTag("mint"))?.to_owned()`.
  *After:* removed; a `mint` tag on a v2 offer is ignored (not required, not read).
- **Offer award/eligibility tags — `gateway.rs` `OfferDraft`.** *After:* emit `["mode", …]`, optional
  `["award_count", …]`, and zero-or-more `["require", "<predicate>", "<value>"]` tags; `parse_offer` reads
  them back (§ Award modes, § Eligibility).
- **Test fixture — `seller.rs:486`** (`mint_url: crate::home::DEFAULT_MINT_URL.into()`): drop the field
  from the `ParsedOffer` fixture. *(seller.rs is gate-fenced; this fixture edit rides with Job B.)*

### (b) Seller daemon gates + config — `home.rs`, `seller_daemon.rs` (money-adjacent)

- **Config: single mint → `accepted_mints` — `home.rs`.** *Before:* `MobeeConfig.mint_url: String`
  (`home.rs:320`; the field lives on `MobeeConfig`, a **top-level** config key, not on `SellerConfig`),
  validated against `DEFAULT_MINT_URL` (`home.rs:17`). *After:* `accepted_mints: Vec<String>`, default
  `vec![DEFAULT_MINT_URL.to_string()]` (§ Config). The existing single `mint_url` maps to
  `accepted_mints = [<that value>]` on load (back-compat shim, § Config).
- **Boot gate — `seller_daemon.rs:330-333`.** *Before:*
  `if home.config.mint_url != DEFAULT_MINT_URL { return Err(DaemonError::Config(...)) }`. *After:*
  validate `accepted_mints` is non-empty and every entry parses as a mint URL; the fail-closed
  real-mint restriction stays (each entry must be an allow-listed testnut/dev mint — real-mint
  enablement is separately gated, § Out of scope).
- **Offer gate — `seller_daemon.rs:522-524`.** *Before:*
  `if offer.mint_url != DEFAULT_MINT_URL { Skip(OfferSkip::NonTestnutMint { mint_url }) }`. *After:*
  **removed** — the offer no longer names a mint, so there is nothing to gate here; the seller's accepted
  mints are asserted later against the *paid* token (§ (c) redeem guard). The `OfferSkip::NonTestnutMint`
  variant (`seller_daemon.rs:227`) is retired (or repurposed as a redeem-time refusal reason).
- **PaymentPolicy build from `offer.mint_url` — `seller_daemon.rs:812` and `:826-827`.** *Before:*
  `let mint = job.offer.mint_url.clone();` (:812) … `let policy = PaymentPolicy::new([mint_url(&mint)?]);`
  (:826) `let terms = policy.terms_for_offer(&offer, &self.seller_pubkey)?;` (:827). *After:* the policy is
  built from `self.config.accepted_mints` (the seller's own list), and `terms` are derived from the
  seller-authored `creq` + the *realized* mint reported in the buyer's `PaymentRequestPayload`, not from
  the offer. `terms_for_offer` no longer reads a mint off the offer.
- **Receipt binding source — `seller_daemon.rs:1043`.** *Before:*
  `ReceiptPreimage { … mint: active.offer.mint_url.clone(), … }`. *After:* `mint` = the realized mint;
  preimage also folds `creq_hash` (§ (d), § Job D).
- **Journal episode fill — `seller_daemon.rs:1483`.** *Before:* `episode.mint = offer.mint_url.clone();`.
  *After:* `episode.mint` = the realized mint from the payload (Piece-13 offer-facts substitution).

### (c) Buyer pay path + Lightning bridge — `authorize_pay.rs`, `payment_wallet.rs`, `payment_send.rs` (MONEY, gate-fenced)

- **Buyer envelope → NUT-18 payload — `payment_send.rs:5-15`, `:292-303`, `:305-323`.** *Before:*
  hand-rolled `PaymentPayload` / `PaymentEnvelope` carrying a buyer-declared `mint_url` (`:9`, `:295`,
  `:317`) and a `serialized_token`. *After:* the buyer emits a cashu `PaymentRequestPayload`
  (`id`, `memo`, `mint`, `unit`, `proofs`; § NUT-18) constructed to satisfy the seller's `creq`; `mint` is
  the *realized* mint the token came from, chosen from the `creq`'s `m` list. The custom envelope's
  canonical-JSON `mint_url` entry (`payment_send.rs:37`) is dropped in favor of the payload's `mint`.
- **Buyer chooses mint / Lightning bridge — `authorize_pay.rs` (pay gate) + `payment_wallet.rs`.**
  *Before:* buyer pays with a token at the offer's mint; no bridge. *After:* the buyer reads the `creq`
  `m` list; if it holds balance at a listed mint it sends directly; otherwise it **bridges via Lightning**
  — melt at its own mint, request a mint-quote at an accepted mint, pay the quote, and send a fresh token
  from the accepted mint (§ Lightning bridge). The bridge reuses the wallet's existing cdk melt/mint
  primitives (`cdk::wallet::Wallet`, already used by `payment_wallet.rs`).
- **Redeem guard — `payment_wallet.rs:920-977` (`receive_with`).** *Before:* asserts the incoming token's
  `mint`/`unit`/face-value match `terms` built from `offer.mint_url` (via `terms_for_offer`,
  `payment_wallet.rs:93-101`). *After:* asserts the token's mint is `∈ self.config.accepted_mints` **and**
  equals `payload.mint`, and that `amount`/`unit` match the seller-authored `creq`. P2PK / fee / post-fee
  checks (`require_received_amount_after_fee`, `:973-977`) are unchanged.

### (d) Binding / receipt — `payment.rs`, `receipt.rs`, `authorize_pay.rs` (MONEY, gate-fenced)

- **AttemptId reconciliation key — `payment.rs` (attempt domain `mobee/v1/payment-attempt` `:26`; hash
  folds at `:126-133`; `PaymentTerms` `:140`; `PaymentKey` `:169`).** *Before:* the attempt hash folds in a
  `mint` sourced from the offer among its fields. *After:* add a `creq_hash` field to the key and fold it
  into the attempt hash; `mint` remains but now denotes the *realized* mint from the payload (not the
  offer). `amount`/`unit` are read off the `creq`.
- **Receipt co-signed preimage — `receipt.rs:100-115` (`ReceiptPreimage`), canonical array
  `:119-135`, digest `:137-143`.** *Before:* binds `job_hash, offer_id, amount, unit, mint, buyer_pubkey,
  seller_pubkey, delivery_integrity_hash, delivery_kind, exec_metadata_commitment`, with `mint` sourced
  from offer terms. *After:* add `creq_hash` to the preimage (fixed position, additive to the canonical
  array — a binding-format change, hence the version bump); `mint` becomes the realized mint. Both
  co-signatures then commit to the seller-authored request.
- **Receipt build — `authorize_pay.rs:403` (`receipt_preimage_for`), `:434` (`build_and_publish_receipt`),
  event `gateway.rs:511-546` (`receipt_draft`).** *Before:* preimage built from `PaymentKey` fields with
  offer-sourced mint. *After:* thread `creq_hash` and the realized mint through `receipt_preimage_for` and
  add a `["creq-hash", …]` tag to `receipt_draft` alongside the existing `job-hash`/`mint` tags.
- **Dead code note — `receipt.rs:7-44` (`ReceiptHashInput`, domain `mobee/v1/receipt`).** This type has
  **no live caller** (a repo-wide grep of `ReceiptHashInput` outside `receipt.rs` is empty — re-verified
  at `16390ad`). It is not part of the live binding and should be deleted in Job D to avoid confusion.

### (e) Version bump — `gateway.rs`

- **`PROTOCOL_VERSION` — `gateway.rs:8`.** *Before:* `"1"`. *After:* `"2"`. Emitted unchanged on all kinds
  via `version_tag()` (`gateway.rs:575-576`).
- **Validation — `parse_offer` — `gateway.rs:260-262`.** It already rejects any `v != PROTOCOL_VERSION`
  with `OfferParseError::UnsupportedVersion`. After the kind exit this is a *minor-version* gate within the
  mobee block (major separation is by kind now, § Compatibility). The `v` tag is emitted on all kinds but
  validated only on the offer parse path; result/claim/feedback/receipt parsers do not check it (they flow
  inside an already-version-checked job). Adding `v` validation to the other parsers is a separate
  hardening (§ Out of scope).

### (f) Buyer-facing ripple — `job_lifecycle.rs`, `crates/mobee/src/mcp.rs`

- **`OfferView.mint_url` — `job_lifecycle.rs:134`**, populated from `parsed.mint_url` at `:1126`. `OfferView`
  derives `Serialize` (`job_lifecycle.rs:123`) and is returned by the MCP `get_job`/list tools
  (`crates/mobee/src/mcp.rs`), so deleting the field **changes the buyer-facing tool JSON** even though
  `mcp.rs` never names `mint_url` — the field flows out via whole-struct serialization. This ripple lands
  in Job A′ with the field deletion; `build_offer_draft` (`job_lifecycle.rs:424`, mint param `:426`) and
  its call sites (`:354`, tests) also drop the mint argument.

---

## Config: seller `accepted_mints`

The single `mint_url` config becomes a list. It is a **top-level** `MobeeConfig` key (the current
`mint_url` at `home.rs:320` is top-level in `config.toml`, not under a `[seller]` section):

```toml
# The mints this seller will accept payment at. First entry is the default the
# seller advertises first in the creq `m` list. Real-mint entries are gate-fenced
# (see Out of scope) — testnut/dev mints only in v2.
accepted_mints = ["https://testnut.cashudevkit.org"]
```

- **Default:** `vec![DEFAULT_MINT_URL.to_string()]` — i.e. exactly the current testnut default
  (`home.rs:17`), so an operator who sets nothing behaves identically to today.
- **Migration of the existing single field:** on load, if a legacy top-level `mint_url = "…"` is present
  and `accepted_mints` is absent, map it to `accepted_mints = ["<that value>"]` (a small back-compat shim
  in the config loader, next to the existing `migrate_dead_mint_url` shim at `home.rs:453-456`). This keeps
  every current seller config working unchanged.
- **Boot validation:** non-empty; every entry a well-formed mint URL; every entry within the allow-listed
  real-mint fence (testnut/dev only in v2). The old "must equal `DEFAULT_MINT_URL`" scalar check
  (`seller_daemon.rs:330-333`) becomes "every entry is allow-listed."
- **Where the list is used:** authoring the `creq` `m` array (claim), building the redeem
  `PaymentPolicy` (`seller_daemon.rs:826`), and the redeem guard membership check
  (`payment_wallet.rs`).

---

## NUT-18 — encoding and reuse

Verified against the NUT-18 spec (`cashubtc/nuts/main/18.md`) and the pinned cashu crate.

**Encoding.** A payment request serializes as `"creq" + "A" + base64url(CBOR(PaymentRequest))` — the
`creqA…` prefix. `PaymentRequest` CBOR keys (all optional): `i` payment id, `a` amount (int), `u` unit
(string, MUST be set if `a` is set), `s` single-use (bool), `m` mints (string array), `d` description,
`t` transports (array), `nut10` locking condition. A `Transport` object is `t` type, `a` target, `g` tags.
For nostr the transport type is `"nostr"`, the target is an `nprofile`, and tags carry `[["n","17"]]` to
signal NIP-17. The buyer's reply is a `PaymentRequestPayload`: `id` (echoes the request `i`), `memo`,
`mint` (the single mint the proofs came from), `unit`, `proofs`.

**Reuse — do not hand-roll.** The pinned `cashu = "=0.17.2"` (`crates/mobee-core/Cargo.toml:18`) already
ships a `nut18` module re-exporting `PaymentRequest`, `PaymentRequestBuilder`, `PaymentRequestPayload`,
`Transport`, `TransportBuilder`, `TransportType`. `PaymentRequest` has `FromStr`/`Display` implementing the
`creqA` prefix, and `TransportType::Nostr` exists. **The repo does not use any of these today** (grep of
`nut18`/`PaymentRequest` across `crates/` is empty) — the current flow rolls its own `PaymentEnvelope`. The
build MUST adopt the cashu types for `creq` encode/decode and for the payload rather than re-implement
CBOR/base64 by hand.

**What the seller authors.** `PaymentRequestBuilder` with `a = offer.amount`, `u = offer.unit`,
`m = config.accepted_mints`, one `Transport { t: Nostr, a: <seller nprofile>, g: [["n","17"]] }`,
`i = <job/attempt id>`, `s = true` (single-use — one claim, one payment), and **no `nut10` locking
condition** (§ Metadata trust levels). `.build()`, then `Display` → the `creq…` string that goes in the
claim's `["creq", …]` tag.

---

## The Lightning bridge (buyer wallet)

When the buyer holds no balance at any mint in the `creq` `m` list, its wallet bridges rather than
refusing:

1. **Mint-quote at an accepted mint** — pick a mint from `m`, request a `MintQuote` for `a` (bolt11
   invoice to pay into that mint).
2. **Melt at the buyer's own mint** — request a `MeltQuote` at the buyer's mint for that invoice and pay
   it (melt), which pays the accepted mint's invoice over Lightning.
3. **Mint the fresh token** — once the mint-quote is paid, mint proofs at the accepted mint and send that
   fresh token as the `PaymentRequestPayload` (its `mint` = the accepted mint).

This is entirely buyer-side and uses the cdk melt/mint primitives already available via
`cdk::wallet::Wallet` (the crate `payment_wallet.rs` already depends on). Fees (both the melt fee at the
buyer's mint and any mint fee) come out of the buyer's balance; the seller receives exactly `a` at an
accepted mint. The bridge is best-effort and synchronous within the pay attempt — on failure the pay
attempt refuses (`mint_unreachable_pay`, § Failure semantics) and the buyer retries; no partial state is
committed because the receipt only co-signs after the seller confirms redemption.

---

## Failure semantics

Simple **on purpose** — no escrow, no atomicity, no delivery-vs-payment coupling. Trust + reputation carry
the residual risk. All refusals and progress notices are `3404` FEEDBACK events; each carries a
**machine-readable reason code from a closed enum**, plus an optional human tail (**≤ 300 chars, no
absolute paths**).

### Closed reason-code enum (kind 3404)

```
unsupported_version   ineligible            rate_gate            deadline_gate
expired               wrong_mint            unknown_creq         mint_unreachable_pay
mint_unreachable_redeem  amount_mismatch    claim_expired        internal
agent_spawn_failed    agent_run_failed      agent_timeout
git_fork_failed       git_push_failed
```

- **Admission / pre-claim:** `unsupported_version`, `ineligible`, `rate_gate`, `deadline_gate`, `expired`,
  `claim_expired` — the seller declines or the buyer/seller times a claim out.
- **Payment:** `wrong_mint`, `unknown_creq`, `mint_unreachable_pay`, `mint_unreachable_redeem`,
  `amount_mismatch` — the money path (below).
- **Execution:** `agent_spawn_failed`, `agent_run_failed`, `agent_timeout`, `git_fork_failed`,
  `git_push_failed` — the seller's work or delivery failed after award; surfaced as a `3404` error in place
  of a `3403` result.
- **Catch-all:** `internal` — an unexpected seller-side fault; the human tail (no paths) narrows it.

### Money-path detection + retry

| Situation | Detected at | Reason code | Retry semantics |
|---|---|---|---|
| Buyer pays a token from a mint **not** in `creq.m` | seller redeem guard (`payment_wallet.rs` `receive_with`) | `wrong_mint` | Seller refuses (`3404` error); buyer re-pays from a listed mint. No funds move (token not swapped). |
| Buyer's `creq` reference is **unknown / unparseable** | buyer parse (`PaymentRequest::from_str`) or seller on payload | `unknown_creq` | Buyer aborts and re-fetches the claim; if the seller receives a payload for a `creq_hash` it never authored, it refuses `unknown_creq`. |
| **Accepted mint unreachable at PAY** (buyer can't mint/bridge) | buyer wallet (mint-quote / melt) | `mint_unreachable_pay` | Buyer retries within a bounded window (try the next mint in `m`); if all listed mints are down, buyer walks away — no payload, no binding. |
| **Seller mint unreachable at REDEEM** (token valid, mint down) | seller redeem (`payment_wallet.rs` receive) | `mint_unreachable_redeem` | Seller retries within a window; on exhaustion it walks away (token stays unredeemed-but-not-lost; redeem-sweep is a separate PIECE). No receipt co-signed. |
| **Amount mismatch** (payload value ≠ `creq.a` after fees) | seller redeem guard (`require_received_amount_after_fee`, `payment_wallet.rs:973-977`) | `amount_mismatch` | Seller refuses; buyer re-pays the exact amount. Existing post-fee check, unchanged in shape. |

All refusals are **explicit** `3404` `status=error` events whose content carries the reason code — never a
silent drop. Reputation (a separate chapter) reads these outcomes; the money path never blocks on them.

---

## Compatibility

The kind exit makes cross-version separation **clean by construction** — v1 and v2 do not share a wire
surface, so there is no fall-back to reason about.

- **v1 events are simply not v2 protocol.** A v1 offer is kind `5109`; a v2 seller subscribes to kind
  `3401` with a `t=mobee` filter. It never matches a `5109` event at all — no parse, no refusal, no
  inference. Likewise a v1 seller never matches a v2 `3401`. **Different kinds = clean separation**, which
  is strictly better than v1's tag-based version refusal (a v2 parser cannot be tricked into reading a v1
  event because it never receives one as a candidate).
- **Our fleet flag-days together.** Because mobee's buyers and sellers are operated as one fleet on one
  relay, v2 rolls out as a coordinated flag-day: all seats cut over at once, so there is **no dual-version
  period** to support and no need for a v2 seller to speak v1. The `protocol_versions` heartbeat field and
  `min_protocol_version` eligibility exist for third-party interop, not for our own transition.
- **Third parties get the spec + vectors.** External implementers are not on our flag-day; they build
  against the published spec and signed test vectors (§ Spec artifacts), targeting the v2 kind block
  directly.

---

## Build decomposition

The breaking money change is a spine of five marketplace jobs, each ≤ a few hundred lines. **Money files
are isolated:** Jobs B–E touch the gate-fenced set (`seller.rs`, `payment*.rs`, `authorize_pay.rs`,
`receipt.rs`, `payment_wallet.rs`, seller daemon money path) and are **flagged for buyer-side review**.

**Order: B → C → D → E → A′.** This is the STOP-report fix (`PIECE-14-JOB-A-STOP.md`): `ParsedOffer.mint_url`
has six live consumers in the fenced files, so the offer field **cannot be deleted until every downstream
reader has been re-pointed**. Job A′ is therefore the *final* cleanup — it deletes the now-dead field,
does the kind renumbering + `t=mobee` guard, and bumps the version, in a tree where B–E have already
re-sourced every mint read. Every job below leaves the workspace **compiling** per its own acceptance
block. The six-consumer inventory (STOP report) is placed as: consumer 1 (offer gate) → Job B; consumers
2, 5 (redeem PaymentPolicy / `terms_for_offer`) → Jobs B/E; consumer 3 (receipt mint) → Job D; consumer 4
(journal) → Job B; consumer 6 (fixture) → Job B; the buyer-facing `OfferView`/MCP ripple → Job A′.

### Job B — seller `accepted_mints` config + gate relaxation (money-adjacent)
Replace top-level `MobeeConfig.mint_url` with `accepted_mints: Vec<String>` (default `[DEFAULT_MINT_URL]`,
legacy single-field shim), relax the boot gate to allow-list membership, delete the offer mint gate, retire
`OfferSkip::NonTestnutMint`, re-source the journal `episode.mint`. `ParsedOffer.mint_url` stays present and
readable (Jobs D/E re-point their own reads; the redeem `PaymentPolicy` at `:812/:826` is temporarily built
from `accepted_mints` here). `home.rs`, `seller_daemon.rs`, `seller.rs` fixture.

```acceptance
- [ ] grep -n 'accepted_mints' crates/mobee-core/src/home.rs  → field on MobeeConfig, default = [DEFAULT_MINT_URL]
- [ ] grep -n 'NonTestnutMint' crates/mobee-core/src/seller_daemon.rs  → offer-gate skip removed (variant retired or redeem-only)
- [ ] cargo test -p mobee-core -- accepted_mints_default  → an empty config yields accepted_mints == [DEFAULT_MINT_URL]
- [ ] cargo test -p mobee-core -- legacy_mint_url_migrates  → a config with only mint_url loads as accepted_mints == [that value]
- [ ] cargo build --workspace  → compiles (boot gate now checks membership; ParsedOffer.mint_url still present)
```

### Job C — seller authors the `creq` in the 3402 claim (MONEY — buyer-side review)
Build a NUT-18 `PaymentRequest` (cashu `PaymentRequestBuilder`) from `offer.amount`/`offer.unit` +
`config.accepted_mints` + a nostr transport to the seller key, no `nut10` condition, attach as a
`["creq", …]` tag on `claim_draft`. `gateway.rs`, `seller_daemon.rs`.

```acceptance
- [ ] grep -n 'PaymentRequestBuilder\|creqA\|"creq"' crates/mobee-core/src  → creq authored + tagged on the claim
- [ ] cargo test -p mobee-core -- claim_carries_creq  → the claim has a `creq` tag whose value starts with "creqA"
- [ ] cargo test -p mobee-core -- creq_roundtrip  → PaymentRequest::from_str(tag) yields a=offer.amount, u=offer.unit, m=accepted_mints, one nostr transport to seller, no nut10
- [ ] cargo build --workspace  → compiles against cashu 0.17.2 nut18 types (no hand-rolled CBOR)
```

### Job D — binding moves onto the `creq` hash (MONEY — buyer-side review)
Add `creq_hash` to `PaymentKey`/attempt hash (`payment.rs`) and to `ReceiptPreimage` (`receipt.rs`); mint
in both becomes the realized mint; thread it through `authorize_pay.rs` and the seller receipt binding
(`seller_daemon.rs:1043`), add a `creq-hash` receipt tag; delete dead `ReceiptHashInput`.

```acceptance
- [ ] grep -n 'creq_hash' crates/mobee-core/src/payment.rs crates/mobee-core/src/receipt.rs  → present in PaymentKey and ReceiptPreimage
- [ ] grep -rn 'ReceiptHashInput' crates/mobee-core/src  → no matches (dead type removed)
- [ ] cargo test -p mobee-core -- receipt_preimage  → preimage digest changes when creq_hash changes
- [ ] cargo test -p mobee-core -- attempt_id  → AttemptId differs for two claims with different creq_hash, same offer
- [ ] cargo build --workspace  → compiles
```

### Job E — buyer pay path + Lightning bridge + redeem guard (MONEY — buyer-side review)
Buyer parses the `creq`, sends a NUT-18 `PaymentRequestPayload` (bridging over Lightning if its balance is
at a non-listed mint); `terms_for_offer` stops reading a mint off the offer; seller redeem guard checks
token mint ∈ `accepted_mints` and equals `payload.mint`. `authorize_pay.rs`, `payment_wallet.rs`,
`payment_send.rs`.

```acceptance
- [ ] grep -n 'PaymentRequestPayload' crates/mobee-core/src/payment_send.rs  → buyer emits the NUT-18 payload (old PaymentEnvelope replaced)
- [ ] cargo test -p mobee-core -- pay_matches_creq  → a payload whose mint ∉ creq.m is refused with reason `wrong_mint`
- [ ] cargo test -p mobee-core -- lightning_bridge  → with balance only at an unlisted mint, the buyer melts→mint-quotes→sends a token from a listed mint (mock wallet)
- [ ] cargo test -p mobee-core -- redeem_guard  → seller receive accepts a token from a listed mint == payload.mint, refuses otherwise
- [ ] cargo test -p mobee-core  → full crate suite green
```

### Job A′ — final cleanup: drop `mint_url`, renumber kinds, `t=mobee` guard, version bump (gateway-touching)
With B–E landed, `ParsedOffer.mint_url`/`OfferDraft.mint_url` are dead: delete them and the `mint` tag
emit/read. Renumber the kind constants (`5109→3401`, `6109→3403`, split `7000` into `3402` claim + `3404`
feedback, `3400` receipt unchanged), point `claim_draft` at `JOB_CLAIM_KIND`, add the `["t","mobee"]`
namespace guard to every draft builder and every parser (reject on absence), and bump `PROTOCOL_VERSION`
to `"2"`. Absorb the buyer-facing ripple: drop the `mint` arg from `build_offer_draft` and its call sites,
delete `OfferView.mint_url`, and re-point the seller-daemon/`job_lifecycle`/`mcp` subscription+filter kinds
to the new numbers. `gateway.rs`, `seller_daemon.rs` (filters), `job_lifecycle.rs`, `crates/mobee/src/mcp.rs`,
`seller.rs` fixture.

```acceptance
- [ ] grep -rn 'mint_url' crates/mobee-core/src/gateway.rs  → no matches (field and tag gone)
- [ ] grep -n 'PROTOCOL_VERSION: &str = "2"' crates/mobee-core/src/gateway.rs  → 1 match
- [ ] grep -n '3401\|3402\|3403\|3404\|3405' crates/mobee-core/src/gateway.rs  → kind constants renumbered; 3402 claim split from 3404 feedback
- [ ] cargo test -p mobee-core -- namespace_guard  → an event without t=mobee is rejected by parse_offer
- [ ] cargo test --workspace  → full suite green (buyer-facing MCP ripple resolved)
```

### Follow-on additive jobs (non-breaking; land after the flag-day or alongside it)
These extend the v2 surface without touching the money spine's wire break. Each is independent and
compiles standalone against the v2 kind block.

- **Job F — award modes + kind 3405** (money-adjacent: decides who is paid). `mode`/`award_count` offer
  tags; `3405` award draft/parse; bid-window + await-award in the seller daemon; buyer award publish.
  ```acceptance
  - [ ] grep -n 'JOB_AWARD_KIND\|"mode"\|"award_count"' crates/mobee-core/src/gateway.rs  → award kind + offer tags present
  - [ ] cargo test -p mobee-core -- award_bid_mode  → in bid mode a seller does not start until a 3405 e-tagging its claim exists
  - [ ] cargo build --workspace  → compiles
  ```
- **Job G — eligibility `require` tags**. Closed-vocab parse/emit; seller self-gate (`3404 ineligible`);
  buyer enforce-at-award against relay history; reject `require` on `direct` offers.
  ```acceptance
  - [ ] grep -n '"require"\|min_receipts\|mint_overlap' crates/mobee-core/src  → closed-vocab predicates parsed
  - [ ] cargo test -p mobee-core -- require_direct_invalid  → a require tag on a direct offer is rejected
  - [ ] cargo build --workspace  → compiles
  ```
- **Job H — result `metadata_trust` levels**. `metadata_trust` result tag (`seller-claimed`/
  `replay-auditable` shipped); optional transcript-hash bind; replay-audit sampling helper.
  ```acceptance
  - [ ] grep -n 'metadata_trust\|replay-auditable' crates/mobee-core/src  → level tag + shipped values present
  - [ ] cargo test -p mobee-core -- metadata_trust_levels  → unshipped levels parse but are flagged unshipped
  - [ ] cargo build --workspace  → compiles
  ```
- **Job I — seller heartbeat (addressable kind 30340)**. Republish `d="mobee-seller"` on a ~5-min cadence
  with `accepting`/`queue_depth`/`rate`/`protocol_versions`; consumers resolve by `(pubkey, d)`.
  ```acceptance
  - [ ] grep -n 'SELLER_HEARTBEAT_KIND\|"mobee-seller"' crates/mobee-core/src  → addressable kind 30340 + d-tag
  - [ ] cargo test -p mobee-core -- heartbeat_addressable  → kind ∈ 30000..=39999 and keyed by (pubkey, d)
  - [ ] cargo build --workspace  → compiles
  ```

---

## Out of scope

- **Escrow, atomicity, delivery-vs-payment coupling, payment-payload locking (`nut10`)** — deliberately not
  introduced; trust + reputation carry residual risk (§ Failure semantics, § Metadata trust levels).
- **Real-mint enablement** — v2 keeps the fail-closed testnut/dev allow-list; accepting real mints is a
  separate, separately-gated change (the boot allow-list is the seam).
- **Redeem-sweep** — recovering unredeemed-but-not-lost tokens after a `mint_unreachable_redeem` walk-away
  is tracked in a separate PIECE, not here.
- **Reputation scoring** — the failure-reason stream and receipt history feed a future reputation chapter;
  eligibility `require` tags are filters, not that trust layer (§ Eligibility).
- **`provider-signed` / `tee-attested` metadata trust** — defined but unshipped in v2 (§ Metadata trust).
- **Multi-mint splitting in one payment** — v2 pays from a single mint (`payload.mint` is one mint).
- **`v`-tag validation on result/claim/feedback/receipt kinds** — v2 relies on the offer-parse gate plus
  the `t=mobee` guard; adding version checks to the other parsers is a separate hardening.

---

## Spec artifacts

At cut time, this doc plus **signed test-vector fixtures** (a golden event per kind — `3400`–`3405` and
`30340` — with known keys, so third parties can validate their parsers and signature checks) are promoted
to `docs/protocol/PROTOCOL-v2.md` + a fixtures directory. **That extraction is a follow-on job**;
`PROTOCOL-v2.md` is not written here. This document remains the single PIECE-14 source of truth until then.

---

## Decisions log

- **Full DVM exit.** New contiguous kind block `3400` receipt · `3401` offer · `3402` claim · `3403`
  result · `3404` feedback · `3405` award. `5109/6109/7000` and the `+1000` convention die in v2 (NIP-90 is
  unrecommended upstream; the namespace bought only explorer noise).
- **`t=mobee` namespace guard** required on all mobee kinds; every parser rejects its absence.
- **Heartbeat** = addressable kind `30340`, `d="mobee-seller"`, ~5-min cadence, payload
  accepting/queue-depth/rate/protocol-versions; keyed by `(pubkey, d)` per NIP-01 (`30000`–`39999`).
- **Claim = `3402`** bid+invoice carrying the NUT-18 `creq`; **`3404`** takes over all progress/error/
  refusal duties with a closed reason-code enum (machine code + optional human tail ≤300 chars, no paths).
- **Award modes** on the offer: `direct` | `first-claim` | `bid`; bid-mode award carried on its own kind
  `3405` (buyer-signed, e-tags offer + winning claim); `award_count` (default 1) for N-winner tournaments.
- **Eligibility `require` tags** (non-directed modes only): closed vocab of log-verifiable predicates
  (`min_receipts`, `min_completion_rate`, `harness`, `min_protocol_version`, `mint_overlap`); sellers
  self-gate, buyers enforce at award; sybil caveat — requirements are filters, reputation is trust.
- **Metadata trust levels** on results: `seller-claimed` + `replay-auditable` shipped; `provider-signed` +
  `tee-attested` defined-unshipped. No payment-payload lock/condition (YAGNI).
- **Build order fix** (per `PIECE-14-JOB-A-STOP.md`): B → C → D → E → A′; A′ is the final cleanup that
  deletes the dead `mint_url`, renumbers kinds, adds the `t=mobee` guard, and bumps the version.
- **Typed delivery seam**: delivery is a typed field; v2 ships `type=fork` (relay-git, repo must be
  pre-announced); unknown types are refused with a reason, not misparsed — no version bump to add a type.
- **Compatibility**: v1 events are different kinds a v2 parser never matches (clean separation); our fleet
  flag-days together (no dual-version period); third parties build against the spec + signed vectors.
- **Spec artifacts**: this doc + signed golden-event fixtures become `docs/protocol/PROTOCOL-v2.md` at cut
  time — a follow-on job.
