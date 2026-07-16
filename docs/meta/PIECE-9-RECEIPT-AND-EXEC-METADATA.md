# Piece-9 — receipt-as-own-event + execution-metadata tags

Protocol/spec extension atop landed **piece-6** (payment SM, [PIECE-6-PAYMENT-SM.md](PIECE-6-PAYMENT-SM.md))
and **piece-7** (git delivery, [PIECE-7-GIT-DELIVERY.md](PIECE-7-GIT-DELIVERY.md)). Two cohesive
changes to what the settlement artifacts **are** and **carry**:

1. The co-signed **receipt (kind-3400) becomes a first-class published event** with its own id, a real
   signature check, and a binding to the **delivered git object** — completing piece-6's already-live
   `ReceiptPublished` state, whose effect is currently a stub.
2. **Execution-metadata** (harness · model · usage · wall-time) rides the seller's kind-6109 result and
   is **echoed** (filtered) into the kind-3400 receipt, as **seller-claimed, unverified** data.

Class: **MONEY-adjacent** → composition + adversarial + codex + operator gate. **Back-compat is a hard
requirement.** Design piece — no product code lands here.

> **v3 — operator rulings folded (7/16, hearth; gudnuf-overridable):** D1 receipt publisher = **BUYER**
> (spec conformance — locked piece-6 already says the payer attests; the charter's "seller" was an
> acknowledged error). D2 cost = **reversible default: tokens-only PUBLIC, cost in the PRIVATE journal,
> `total_cost_usd` speced-but-unpopulated**, public-cost left as a pending gudnuf ruling (published
> events can't be unpublished, so the default walks forward — populating cost later is additive, never a
> retraction). D4 (from piece-10) = **YES, the receipt attests the delivered object** — folded here so
> the spec re-locks once. Pipeline: composition (Scribe) + blinded adversarial (Opus, which corrected a
> false current-state premise in the first draft — see below). **codex deep is PARKED** (CLI 0.89.0 hit
> a server version gate + the workspace spend cap is exhausted; unlock = gudnuf cap-raise + CLI bump;
> re-run on mobee-orch's ping).

> **v4 — Q2 ruling folded (7/16, gudnuf; supersedes D2's cost-private default):** the public
> exec-metadata block is **harness-generic and PUBLIC**, an **open/opportunistic** schema —
> each harness fills what it exposes; **absent fields stay ABSENT (never zero-filled)**. **Cost
> is PUBLIC where the harness reports it** (claude-agent-acp exposes `usage` + `total_cost_usd`
> → now a public tag), carrying harness id, model, `usage_transport`, `total_tokens` (+
> input/output/reasoning components), cache siblings, `wall_time` (ms), and cost — plus whatever
> else the harness exposes. `metadata_trust = seller-claimed` stays. This walks forward on the D2
> reversible default (publishing cost is additive, and D2 explicitly left it a pending gudnuf
> ruling — now ruled public). The margin-exposure trade-off (§ Privacy) is **accepted** as the
> cost of a transparent usage market. Item 2 below is written to this ruling.

Verified against `origin/dev` `168c8bc`: `payment.rs`, `authorize_pay.rs`, `gateway.rs`.
Build (piece-9 code, `scribe/piece9-code-receipt-usage` off `e2196db`): the public block is
**opportunistic** — the seller emits `harness` + `usage_transport` + `metadata_trust` +
`wall_time`; `tokens`/`model`/`cost` stay **absent** until ACP-usage capture is plumbed through
the driver (named deferred — the seller run surface exposes no token counts today).

---

## Item 1 — the receipt is its own event

### Current state (VERIFIED in code — this corrects the first draft)

The payment SM **already reaches** `Sent → ReceiptPublished → Closed` **live**
(`payment.rs::advance` :667-679, via `authorize_pay` → `PaymentService::run`, MCP-wired). It is **not**
missing and **not** on another branch. What is stubbed:

- The concrete `publish_receipt` effect (`authorize_pay.rs:215-221`) returns
  `ReceiptEvidence { receipt_id = payment.payment_id, author = buyer_cashu, valid_signers =
  vec![buyer_cashu, seller_cashu] }`. So today:
  - **`receipt_id` aliases the kind-1059 payment-envelope id** — no standalone receipt event exists.
  - **no kind-3400 is published** — `gateway.rs::receipt_draft` (the co-signed 3400 builder) has
    **zero live callers**.
  - `valid_signers` is **hardcoded** → `ReceiptAuthority::verify` (`payment.rs:464`, a membership check
    over the caller-supplied list) **always passes without checking any signature**. The
    "co-signed receipt" authority is inert theater.
- `ReceiptEvidence` carries **no relay-success field** (`payment.rs:451`), and the `Sent →
  ReceiptPublished` transition advances on any `Ok` — so the money-path "empty relay = fail" rule is
  **not enforced for the receipt** (it is enforced for the send, `payment.rs:656`).

Consequence: a buyer's receipts list and any third-party consumer (the observatory syncs from dev) have
no kind-3400 to read, and nothing verifies the receipt's authenticity.

### Spec (the build wires the real effect into the existing state)

- `publish_receipt` MUST build a **buyer-authored kind-3400** via `receipt_draft`, publish it, and set
  `receipt_id` to **that event's id** (not the 1059 id).
- **Deterministic receipt id.** Pin `created_at` + a fixed tag order so a piece-6 recovery **republish
  reproduces the same event id** → genuinely idempotent republish. (Without this, republish yields a
  new id each time — multiple valid receipts per trade, and dedup-by-id cannot collapse them.)
- **Real authority.** `ReceiptAuthority::verify` MUST verify the actual signatures (see § Signed
  preimage) against **externally-anchored** identities — **buyer == the offer's author** (from the
  offer event) and **seller == the accepted-claim/offer seller** — **never** taken from the receipt's
  own `p`-tags (a self-anchored check is circular: an attacker could author a 3400 with their own key,
  set `p=buyer` to themselves, lift the seller's public `sig/seller` off the kind-6109 result, and
  self-sign a "co-signed" receipt for a trade they were not party to).
- **Publisher = buyer-authored (RULED — spec conformance, not an open call).** Locked piece-6
  `ReceiptPublished` already says the payer attests; the earlier charter's "seller publishes" was an
  acknowledged error (hearth concurs). The buyer authors + publishes the kind-3400.
- **Money-path publish rule.** `publish_receipt` MUST **fail closed (`Err`) on empty `relay_success`
  before returning evidence** (mirroring `send_payment` at `payment.rs:656`). Recovery retries **only**
  the (now deterministic-id, idempotent) receipt publish — never re-pay, never re-send the DM
  (piece-6).

### Signed preimage (NEW — the co-signature must bind the trade + the delivered object)

Today `sig/seller` and `sig/buyer` are computed over the **job-hash only**
(`job_hash = SHA256(job_id | task | amount)`, `job_lifecycle.rs:589`; `sign_receipt_hash`,
`seller.rs:266`). That does **not** bind mint, offer id, result id, buyer/seller identity, the echoed
exec-metadata, or the delivered git object — so a buyer could wrap a genuine seller job-signature into a
receipt with a different amount/mint/metadata and it would still "verify."

**Spec:** define the receipt **preimage** the co-signatures commit to:
`{ job_hash, offer_id, result_id, amount, unit, mint, buyer_pubkey, seller_pubkey,
delivery_integrity_hash, delivery_kind, exec_metadata_commitment }`, where
`exec_metadata_commitment` = a hash of the canonical echoed exec-metadata tag set (or the empty marker
if none). The seller signs this preimage at delivery; the buyer counter-signs the same preimage before
publishing. `ReceiptAuthority::verify` checks both signatures over it. **D4 (piece-10):**
`delivery_integrity_hash` + `delivery_kind` are inside the preimage so the receipt **attests which git
object was paid for** and the kind (fork/patch) is **not forgeable** (an unsigned path could be flipped
to reinterpret the same 40-hex as a commit vs a tree oid).

### Two signature systems — keep them distinct

- **nostr event signatures** — the kind-3400 tags `sig/seller` + `sig/buyer` (schnorr over the
  preimage above) plus the buyer's **event-level** nostr signature (authorship). This is what proves
  the *settlement record*.
- **cashu P2PK** — `buyer_cashu` / `seller_cashu` proof locks (`payment.rs` `ReceiptAuthority` today
  tracks these). This is *payment authenticity* and is a separate system. The spec must not conflate
  the two; the receipt records settlement, the P2PK locks bind the proofs.

### Event schema — kind-3400 receipt

Existing `receipt_draft` tags (verified to match the code) + the D4 delivery-binding tags:

| tag | shape | meaning |
|-----|-------|---------|
| `job-hash` | `["job-hash", "<hex>"]` | streamed job-hash (pieces 1/5) |
| `amount` | `["amount", "<n>", "sat"]` | face amount (unit-tagged) |
| `e` (root) | `["e", "<offer_id>", "", "root"]` | binds the offer (kind-5109) |
| `e` (reply) | `["e", "<result_id>", "", "reply"]` | binds the result (kind-6109) |
| `p` | `["p", "<buyer>"]`, `["p", "<seller>"]` | parties (advisory — authority anchors externally, above) |
| `mint` | `["mint", "<url>"]` | settlement mint |
| `sig` | `["sig", "seller", "<sig>"]`, `["sig", "buyer", "<sig>"]` | co-signatures **over the preimage** |
| `delivery_integrity_hash` | `["delivery_integrity_hash", "<40-hex>"]` | **D4:** the paid delivered git object (piece-10) — fork tip `commit_oid`, or (deferred) patch result `tree_oid` |
| `delivery_kind` | `["delivery_kind", "fork"\|"patch"]` | **D4:** which object the hash is (commit vs tree) — **in the preimage**, so not forgeable |
| `t` / `v` | `["t","mobee"]`, `["v","<proto>"]` | protocol markers |

**Change (D4):** Item 1 now **adds the two delivery-binding tags** (`delivery_integrity_hash` +
`delivery_kind`, both in the signed preimage) so the settled receipt attests the delivered object; the
rest of Item 1 is **behavior** (publish it, record its own id, verify real sigs). Item 2 adds the
optional exec-metadata tags below.

### Buyer behavior

- On success: build + publish the deterministic-id kind-3400 (author = buyer), **fail closed on empty
  relay**, and **journal the 3400 event id** (not the 1059).
- **Dedup by `(author, job-hash)`, first-valid-seen** — not by raw event id (deterministic id makes
  republish collapse; the compound key still guards a non-deterministic edge).
- The receipt id is for **listing/lookup only**. Pay-once safety is the journal's `attempt_id` +
  `Sent`-existence (piece-6), **independent** of the receipt id — recording it is not a double-spend
  control.

### Back-compat (discriminator fixed)

- **Legacy trades reach `Closed` with `receipt_id` = the 1059 envelope id** — a present, non-empty,
  32-byte id structurally identical to a real 3400 id. So **"missing id" is the wrong discriminator.**
- Discriminate legacy vs new by **journal state + event kind** (resolve the recorded id; `kind==3400`
  ⇒ new, else legacy) — or, preferred, a **protocol-version stamp on the `ReceiptRecord`** so it is a
  **local** check, no relay fetch.
- `Sent`-with-no-receipt is **new-and-incomplete** (republish per piece-6), **not** legacy — a
  "missing id ⇒ legacy, skip" rule would wrongly mark it done.
- Consumer-visible states MUST be distinguishable: **not-paid** · **paid, receipt-pending** (`Sent`) ·
  **paid + receipted** (`Closed` w/ 3400) · **legacy closed** (`Closed` w/ 1059-aliased id). New code
  MUST NOT reject or re-pay a legacy trade. Legacy receipts have no `delivery_integrity_hash` — a
  consumer MUST treat its absence as legacy, never as a verification failure.

---

## Item 2 — execution-metadata tags

### Origin + trust (read first)

- The **seller** produces exec-metadata and attaches it to its **kind-6109 result** (seller-authored,
  seller-signed). It is **SELLER-CLAIMED and UNVERIFIED.**
- **Authority = the seller's result event.** The receipt echo is a **convenience copy**; on any
  divergence, consumers MUST resolve to the result. (`sig/seller` does not cover exec-metadata; only
  the buyer's event-level signature covers the echoed copy — so a buyer *could* alter the numbers.
  Result-is-authoritative closes that.)
- **In-artifact provenance is required:** every event carrying exec-metadata MUST include
  `["metadata_trust", "seller-claimed"]`. The buyer's co-signature attests to **settlement inclusion,
  not** the factual accuracy of the numbers — the trust boundary lives **in the artifact**, not only
  in this prose, so a third-party tool cannot mistake echoed numbers for co-attested data.
- **Buyer echo = canonical filtered copy**, not blind verbatim: copy only known, well-formed,
  type-valid, non-duplicate exec-metadata tags; drop the rest; **cap the echoed tag count**. (The buyer
  signs its receipt — it must not propagate seller-controlled tag-flood or garbage into a money-adjacent
  artifact.)

### Schema — aligned to the LOCKED checkpoint-a usage schema (do NOT reinvent)

From [USAGE-MATRIX-CPB.md](USAGE-MATRIX-CPB.md) checkpoint-a (locked): `total_tokens = input + output +
reasoning`; cache/tool are **sibling evidence, not summed**; `remaining` is **seller-local → excluded**
(privacy); **`usage_transport` (`acp-native | side-channel`) is required** — usage transport is
harness-dependent.

### Exact tags on kind-6109 result — echoed (filtered) into kind-3400 receipt

All **OPTIONAL** (absent ≠ invalid). Flat positional tags (codebase idiom `["amount","5","sat"]`):

| tag | shape | notes |
|-----|-------|-------|
| harness | `["harness", "<id>"]` | e.g. `claude-agent-acp`, `cursor-agent-acp`, `codex-acp-ng` |
| model | `["model", "<model-id>"]` | e.g. `claude-opus-4-8`, `grok-4.5`, `gpt-5.6-sol` |
| usage_transport | `["usage_transport", "acp-native"\|"side-channel"]` | the harness-dependence axis |
| metadata_trust | `["metadata_trust", "seller-claimed"]` | **required if any exec-metadata present** |
| tokens (total) | `["tokens", "<n>", "total"]` | `= input+output+reasoning` (see invariant) |
| tokens (components) | `["tokens", "<n>", "input"\|"output"\|"reasoning"]` | `input` = **non-cached**; `reasoning` MAY be absent |
| tokens (cache) | `["tokens", "<n>", "cache_read"\|"cache_write"]` | **siblings — never folded into `total`** |
| cost | `["cost", "<n>", "usd", "<basis>"]` | **PUBLIC where the harness reports it (Q2)** — opportunistic, absent-stays-absent — see § cost |
| wall_time | `["wall_time", "<n>", "ms"]` | unit **locked to milliseconds** |

**Anchor rule (parse-gate, not validity-gate).** If any **known** exec-metadata tag is present,
`harness` + `usage_transport` + `metadata_trust` are required. If they are absent, a consumer MUST
**ignore the whole exec-metadata block** — **never** reject the event. Presence of only **unknown**
family labels does **not** trigger the anchor requirement.

**Duplicates.** At most **one** tag per component/field. Duplicate labels ⇒ **ignore the exec-metadata
block** (do not sum, do not guess first/last).

**Total/components invariant.** When `total` **and** all summands are present, `total` MUST equal
`input + output + reasoning`; on mismatch, prefer the components / flag — never trust `total` blindly.
When `reasoning` is **absent** it is **unknown, not zero** — skip the equality check (do not infer 0).

**Never zero-fake an absent field** (a real `0` ≠ unavailable: cursor `reasoning` = absent, codex
`reasoning` = `0`). **`input` = non-cached input** — do not fold `cache_*` into `input`/`total`.

### cost — PUBLIC where the harness reports it (Q2 ruling — supersedes D2)

- **Ruling (gudnuf, 7/16): cost is PUBLIC**, emitted **where the harness reports it** and left
  **absent otherwise** (opportunistic; never zero-filled). claude-agent-acp exposes
  `total_cost_usd` → a public `cost` tag; a harness that bills server-side (cursor) omits it. This
  supersedes D2's tokens-only-public default and walks forward on D2's reversible design (publishing
  cost is additive; D2 explicitly left public-cost a pending gudnuf ruling).
- **Shape:** `unit` is locked to lowercase **`usd`**; `basis ∈ { harness-reported-usd,
  harness-reported-notional }`. `total_cost_usd` is **auth-dependent** — API-key billing = *incurred*
  cost (`harness-reported-usd`); a Max/subscription seat = *notional list-price*
  (`harness-reported-notional`). Never a token count under `cost` (a UI would render `3172` as
  "$3172").
- **Trade-off accepted (§ Privacy):** publishing cost is the deliberate cost of a transparent usage
  market. A seller that treats cost as sensitive MAY still omit it (all exec-metadata is optional).

### Privacy (margin-exposure trade-off — accepted per Q2)

Publishing `cost` + `amount` on the same public **seller-authored** result lets any observer compute
the seller's **per-job margin**, and across jobs reconstruct its **pricing/cost structure and
volume**; and it pairs a **dark** gift-wrap payment with a public cost record — partially undoing
gift-wrap darkness. **Q2 accepts this trade-off** for a transparent usage market: cost is public
where reported. `remaining` stays excluded (seller-local). `model`/`harness`/`wall_time`/`cost`
remain **optional** — a seller that treats its tooling or cost as sensitive MAY omit any of them
(absent-stays-absent); note public tags can tie a seller's nostr identity to its harness/persona.

### Harness reality (USAGE-MATRIX checkpoint-b + this piece's probe)

- **codex (`codex-acp-ng`, gpt-5.6-sol): ACP-native.** Usage on `session/prompt result.usage` +
  `usage_update`. Exposes model, noncached-input, cache-read, output, reasoning. → `acp-native`.
- **cursor (`cursor-agent` `2026.07.09`, grok-4.5): ACP-dark / side-channel.** Usage only on `--print
  --output-format stream-json`. Field names identical to claude; **`cost` ABSENT** (billed
  server-side); **`model` only in `stream-json` (`system/init`)**, not `json`; `reasoning` absent.
  (Static-bundle evidence — a live run was declined to avoid spending gudnuf's Cursor login.) →
  `side-channel`.
- **claude (`claude` headless / `claude-agent-acp`): print surface RICH — all six.** `-p
  --output-format json`: `model` = **key** of `modelUsage` (no top-level model string in `json`;
  present in `stream-json` `system/init` / `assistant.message.model`); `usage.input_tokens`
  (**non-cached**), `usage.output_tokens`, `cache_read`/`cache_creation_input_tokens`, `total_cost_usd`
  (real USD, auth-dependent — **published as a public `cost` tag per Q2** when reported), `duration_ms`.
  **`usage_transport` for claude = `side-channel`** (provisional): usage captured off the print sibling;
  the claude-agent-acp ACP-wire native-vs-dark label awaits the probe USAGE-MATRIX named — but the
  fleet's primary harness now has a **defined** value to emit.
- **Seller rule:** measure on the native surface, normalize to the locked total rule, set
  `usage_transport` to the surface used, omit fields the harness cannot report, keep cost private.

### Back-compat + human/machine rendering

- All exec-metadata tags OPTIONAL; consumers MUST accept none/some/all. **Unknown family labels MUST be
  ignored for computation, never cause rejection.**
- **Split-view rule:** machine consumers ignore unknown labels for computation **and** display; UIs
  render **only known labels** (an unknown label is surfaced as "unrecognized," **never** as data) — so
  a producer cannot slip `["tokens","999999","premium_reasoning"]` past a human on the public receipt
  while machines ignore it.
- **No `v` bump:** receipt-as-own-event + exec-metadata + the delivery-binding tags are detected by
  **tag/kind presence**, not a protocol-version change; legacy consumers ignore the new tags.

---

## Findings — review dispositions

**Fixed (from adversarial + codex):** false current-state premise → corrected in code; receipt
authority made real (external anchors + preimage) — was inert; deterministic id + dedup by
`(author, job-hash)` → closes the double-publish window; empty-relay fail-closed for the receipt;
legacy discriminator by state+kind/version, not "missing id"; `metadata_trust` provenance + result-is-
authoritative echo; filtered/capped echo; anchor as parse-gate; duplicate/absent-reasoning handling;
human/machine render rule; removed the unverified "inert dedup guard" and incorrect "double-count
impossible" claims.

**Operator rulings folded (7/16, hearth; gudnuf-overridable):**
1. **Publisher = BUYER** (D1) — closed as spec conformance (locked piece-6); no re-lock needed.
2. **Cost = PUBLIC where the harness reports it** (Q2, 7/16 — supersedes D2's reversible
   private-by-default). Harness-generic, opportunistic, absent-stays-absent; `total_cost_usd`
   published as a public `cost` tag when reported. Walked forward on D2's design as intended.
3. **Receipt attests the delivered object** (D4, from piece-10) — `delivery_integrity_hash` +
   `delivery_kind` added to the kind-3400 schema **and** the signed preimage.

**Named deferred (not built):** independent measurement/attestation of exec-metadata (it is seller
self-report); encrypted-delivery rail for detailed metadata (DP-1); the claude-agent-acp ACP-wire
transport probe; **ACP-usage capture through the driver** (the seller run surface exposes no token
counts today, so the built block emits `harness`/`usage_transport`/`metadata_trust`/`wall_time` and
leaves `tokens`/`model`/`cost` absent — Q2 makes them public *when reported*); folding exec-metadata
into the co-signed preimage (today `exec_metadata_commitment` = empty marker, Item 1). Public-cost is
**ruled** (Q2 — public where reported), no longer deferred.

## Acceptance (spec bar)

- Both event schemas specified with exact tag names + units + required-if-present rules; **receipt
  preimage defined** (incl. delivery-binding); **real signature verification** against external anchors.
- Buyer/seller behavior + the named piece-6 transition; **deterministic receipt id**; **id recorded**;
  **empty-relay fail-closed**; **dedup by `(author, job-hash)`**; receipt **attests the delivered
  object** (D4).
- Back-compat: legacy discriminated by **state+kind/version** (not missing-id); missing
  `delivery_integrity_hash`/exec-metadata = legacy, not a failure; no reject/re-pay of legacy.
- Usage aligned to locked checkpoint-a; `usage_transport` + `metadata_trust` required; **cost PUBLIC
  where the harness reports it** (Q2 — supersedes D2's private default); opportunistic + absent-stays-
  absent; margin-exposure trade-off accepted.
- Both probe legs included (claude rich; cursor static + cost-absent).
- Operator rulings D1/D2/D4 folded; codex deep PARKED (version gate + spend cap; re-run on ping).

## Fence / reality class

**SPEC-DRAFT (design).** The build lands on branch `scribe/piece9-code-receipt-usage` (off
`e2196db`): the real `publish_receipt` (deterministic-id kind-3400 via `receipt_draft`, real schnorr
verification over the defined preimage incl. the delivery-binding tags, `receipt_id` = the 3400 id,
empty-relay fail-closed) wired into the **existing** live `ReceiptPublished` effect; `result_draft`
extended with the optional PUBLIC exec-metadata tag set (Q2); `receipt_draft` extended with the
delivery-binding tags + an optional filtered echo. Cost is PUBLIC where the harness reports it (Q2);
the built seller block is opportunistic (emits harness/usage_transport/metadata_trust/wall_time;
tokens/model/cost absent pending ACP-usage capture). Reality: today's `ReceiptPublished` was
**BUILT-BUT-STUBBED**; this makes it real.

## Reference

- `payment.rs`: `ReceiptRecord` :196, `ReceiptPublished` :214, `ReceiptEvidence` :451 (no relay field),
  `ReceiptAuthority::verify` :464 (membership-only), `publish_receipt` trait :522, `advance` :605,
  send-gate :656, receipt transition :667-679.
- `authorize_pay.rs:215-221` — the stub `publish_receipt` (receipt_id = payment_id; hardcoded signers).
- `gateway.rs`: `result_draft` :397 (optional `GitResultTags` pattern), `receipt_draft` :479
  (zero live callers — the gap). `seller_daemon.rs:414` — seller result-publish site.
- Delivery binding (D4): [PIECE-10-FREELANCE-PR-DELIVERY.md](PIECE-10-FREELANCE-PR-DELIVERY.md).
- Locked usage schema: [USAGE-MATRIX-CPB.md](USAGE-MATRIX-CPB.md). Payment SM:
  [PIECE-6-PAYMENT-SM.md](PIECE-6-PAYMENT-SM.md). Delivery: [PIECE-7-GIT-DELIVERY.md](PIECE-7-GIT-DELIVERY.md).
