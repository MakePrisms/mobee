# Piece-14 — receiver-authored payment terms (NUT-18)

**Let the party getting paid author the payment terms.** Today the buyer decides everything about money:
the offer (kind-5109) carries a `mint_url` field, the buyer names it, and the seller's only choice is to
refuse any offer whose mint isn't the one hardcoded testnut mint. Payment binding is anchored to
buyer-declared terms. This is backwards — in every real marketplace the *invoice* comes from the seller,
because the seller is the one who has to redeem the money and knows which mints it trusts. This piece
inverts that: the offer drops `mint_url` entirely, and the seller's **claim** (kind-7000) carries a
**NUT-18 payment request** (`creq…`) naming the mint(s) it accepts, the amount, the unit, and the
transport. The claim *is* the invoice. The buyer satisfies it — bridging over Lightning if its balance
lives at a different mint — and payment/receipt binding moves onto the request hash the seller authored.

> **Status: design proposal, DOC-ONLY.** This document proposes a **breaking wire change** (the offer
> schema loses `mint_url`) gated by a **job-schema version bump** (§ Version bump), plus adoption of the
> NUT-18 `PaymentRequest`/`PaymentRequestPayload` types that the pinned cashu crate **already ships but
> the repo does not yet use** (§ NUT-18). It changes the money-safety path (claim construction, the pay
> gate, the receipt bind, the seller redeem guard); those files are gate-fenced and their build jobs are
> flagged for buyer-side review (§ Build decomposition). No escrow, no atomicity, no delivery-vs-payment
> coupling are introduced — failure semantics stay simple on purpose and are carried by trust +
> reputation (§ Failure semantics).
>
> **Anchored to:** `dev@fb64946`. All `file:line` references are to that tree.
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
(`seller_daemon.rs:330-335`) and skips any offer whose `mint_url` differs (`seller_daemon.rs:522-526`).
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

---

## The flow

Five steps. Event kinds and version tag are unchanged except where noted; the substantive change is that
**`mint_url` leaves the offer and the seller authors a `creq` in the claim.**

1. **OFFER — kind 5109 (`JOB_OFFER_KIND`, `gateway.rs:10`).** Work spec + price. Carries `task`,
   `output`, `amount`, `unit`, `deadline_unix`, optional `seller_pubkey` (targeted vs open-pool). **The
   `mint` tag / `mint_url` field is removed** (`gateway.rs:55`, `:101`, `:120`, `:305`). Breaking change →
   version bump to `"2"` (§ Version bump).

2. **CLAIM — kind 7000 (`JOB_FEEDBACK_KIND`, `gateway.rs:12`), `status=processing`.** The seller accepts
   and attaches a **NUT-18 payment request** (`creq…`) it authored: accepted mint(s) in `m`, `a` =
   `offer.amount`, `u` = `offer.unit`, `t` = a nostr transport addressed to the seller's own key
   (NIP-17). **The claim is the invoice.** The `creq` string is attached as a dedicated `["creq", "creqA…"]`
   **tag** on the existing `claim_draft` (`gateway.rs:360-369`), not the content field.
   *Justification (one sentence):* every other mobee protocol field is a nostr tag (amount, mint, deadline,
   e/p, sig/*, `t`, `v`) and the kind-7000 content is `""` today (`feedback_draft`, `gateway.rs:548-553`),
   so a tag keeps the parser uniform, keeps the event filterable, and the `creq` is self-describing so it
   needs no sibling tags.

3. **RESULT — kind 6109 (`JOB_RESULT_KIND`).** Unchanged. Delivery (fork ref, commit oid,
   `delivery_integrity_hash`, `job-hash`) is exactly as today (`result_draft`, `gateway.rs:428`).

4. **PAY.** The buyer parses the seller's `creq` (`PaymentRequest::from_str`, § NUT-18), checks `a`/`u`
   against the offer it made, and satisfies the request: it produces a cashu token **from one of the
   mints in `m`** and returns a NUT-18 `PaymentRequestPayload` over the stated transport (NIP-17 DM to the
   seller's key). **If the buyer's balance is at a mint not in `m`, its wallet bridges via Lightning:**
   melt at the buyer's mint → mint-quote at an accepted mint → send a fresh token from the accepted mint
   (§ Lightning bridge). This replaces today's buyer-mint-declared `PaymentEnvelope`
   (`payment_send.rs:292-303`). **Payment/receipt binding moves to the request hash** (`creq_hash`, a
   SHA-256 of the canonical `creq` string) instead of buyer-declared `mint`/`amount`/`unit` (§ Binding).

5. **RECEIPT — kind 3400 (`JOB_RECEIPT_KIND`, `gateway.rs:13`).** Co-signed by buyer and seller,
   unchanged except that the co-signed preimage (`ReceiptPreimage`, `receipt.rs:99-115`) binds the
   `creq_hash` and the *realized* mint (the single mint the returned token actually came from) rather than
   a mint sourced from `offer.mint_url` (§ Binding).

---

## Exact change inventory

Every change point below is grouped and given a before→after. `file:line` is `dev@fb64946`.

### (a) Schema / offer — `gateway.rs` (not gate-fenced)

- **`OfferDraft.mint_url` field — `gateway.rs:55`.** *Before:* `pub mint_url: String,`. *After:* removed.
- **`OfferDraft::to_event_draft` mint tag — `gateway.rs:101`.** *Before:*
  `TagSpec::new(["mint", &self.mint_url])`. *After:* line removed (no `mint` tag emitted).
- **`ParsedOffer.mint_url` field — `gateway.rs:120`.** *Before:* `pub mint_url: String,`. *After:* removed.
- **`parse_offer` mint read — `gateway.rs:305-307`.** *Before:*
  `mint_url: first_tag_value(&event.tags, "mint").ok_or(OfferParseError::MissingTag("mint"))?.to_owned()`.
  *After:* removed; a `mint` tag on a v2 offer is ignored (not required, not read).
- **Test fixture — `seller.rs:486`** (`mint_url: crate::home::DEFAULT_MINT_URL.into()`): drop the field
  from the `ParsedOffer` fixture. *(seller.rs is gate-fenced; this fixture edit rides with job B.)*

### (b) Seller daemon gates + config — `home.rs`, `seller_daemon.rs` (money-adjacent)

- **Config: single mint → `accepted_mints` — `home.rs`.** *Before:* `SellerConfig.mint_url: String`
  (validated against `DEFAULT_MINT_URL` at `home.rs:16`). *After:* `accepted_mints: Vec<String>`, default
  `vec![DEFAULT_MINT_URL.to_string()]` (§ Config). The existing single `mint_url` maps to
  `accepted_mints = [<that value>]` on load (back-compat shim, § Config).
- **Boot gate — `seller_daemon.rs:330-335`.** *Before:*
  `if home.config.mint_url != DEFAULT_MINT_URL { return Err(DaemonError::Config(...)) }`. *After:*
  validate `accepted_mints` is non-empty and every entry parses as a mint URL; the fail-closed
  real-mint restriction stays (each entry must be an allow-listed testnut/dev mint — real-mint
  enablement is separately gated, § Out of scope).
- **Offer gate — `seller_daemon.rs:522-526`.** *Before:*
  `if offer.mint_url != DEFAULT_MINT_URL { Skip(OfferSkip::NonTestnutMint { mint_url }) }`. *After:*
  **removed** — the offer no longer names a mint, so there is nothing to gate here; the seller's accepted
  mints are asserted later against the *paid* token (§ (c) redeem guard). The `OfferSkip::NonTestnutMint`
  variant is retired (or repurposed as a redeem-time refusal reason, § Failure semantics).
- **PaymentPolicy build from `offer.mint_url` — `seller_daemon.rs:812` and `:826-827`.** *Before:*
  `let mint = job.offer.mint_url.clone();` (:812) … `let policy = PaymentPolicy::new([mint_url(&mint)?]);`
  (:826) `let terms = policy.terms_for_offer(&offer, &self.seller_pubkey)?;` (:827). *After:* the policy is
  built from `self.config.accepted_mints` (the seller's own list), and `terms` are derived from the
  seller-authored `creq` + the *realized* mint reported in the buyer's `PaymentRequestPayload`, not from
  the offer. `terms_for_offer` no longer reads a mint off the offer.
  > **Correction to the brief's line span:** the brief cites `seller_daemon.rs:797-811` for "seller
  > building PaymentPolicy from offer.mint_url"; at `dev@fb64946` that span is the `awaiting_payment`
  > job-bind lookup (`:798-811`), and the actual mint capture + PaymentPolicy build is at `:812` and
  > `:826-827`. Cited accurately above; this is a citation correction, not a design change.

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
  primitives (same crate that `payment_wallet.rs` already uses, `cdk::wallet::Wallet`).
- **Redeem guard — `payment_wallet.rs:920-974` (`receive_with`).** *Before:* asserts the incoming token's
  `mint`/`unit`/face-value match `terms` built from `offer.mint_url` (`:933-940`). *After:* asserts the
  token's mint is `∈ self.config.accepted_mints` **and** equals `payload.mint`, and that
  `amount`/`unit` match the seller-authored `creq`. P2PK / fee / post-fee checks (`:941-977`) are
  unchanged.

### (d) Binding / receipt — `payment.rs`, `receipt.rs`, `authorize_pay.rs` (MONEY, gate-fenced)

- **AttemptId reconciliation key — `payment.rs:123-135`, struct `PaymentKey` `:167-178`,
  `PaymentTerms` `:138-146`.** *Before:* the attempt hash (domain `mobee/v1/payment-attempt`, `:26`) folds
  in a `mint` sourced from the offer among its 8 fields (`:126-133`). *After:* add a `creq_hash` field to
  the key and fold it into the attempt hash; `mint` remains but now denotes the *realized* mint from the
  payload (not the offer). `amount`/`unit` are read off the `creq`, not buyer-declared.
- **Receipt co-signed preimage — `receipt.rs:99-115` (`ReceiptPreimage`), canonical array `:119-134`,
  digest `:137-143`.** *Before:* binds `job_hash, offer_id, amount, unit, mint, buyer_pubkey,
  seller_pubkey, delivery_integrity_hash, delivery_kind, exec_metadata_commitment`, with `mint` sourced
  from offer terms. *After:* add `creq_hash` to the preimage (fixed position, additive to the canonical
  array — a binding-format change, hence the version bump); `mint` becomes the realized mint. Both
  co-signatures then commit to the seller-authored request.
- **Receipt build — `authorize_pay.rs:403-421` (`receipt_preimage_for`), `:434-479`
  (`build_and_publish_receipt`), event `gateway.rs:511-546` (`receipt_draft`).** *Before:* preimage built
  from `PaymentKey` fields with offer-sourced mint (`:410`, `:414`). *After:* thread `creq_hash` and the
  realized mint through `receipt_preimage_for` and add a `["creq-hash", …]` tag to `receipt_draft`
  alongside the existing `job-hash`/`mint` tags (`gateway.rs:524-534`).
- **Dead code note — `receipt.rs:6-44` (`ReceiptHashInput`, domain `mobee/v1/receipt`).** This type has
  **no live caller** (grep of `ReceiptHashInput` outside `receipt.rs` is empty). It is not part of the
  live binding and should be deleted in job E to avoid confusion; flagged here for the reviewer.

### (e) Version bump — `gateway.rs`

- **`PROTOCOL_VERSION` — `gateway.rs:8`.** *Before:* `pub const PROTOCOL_VERSION: &str = "1";`. *After:*
  `"2"`. Emitted unchanged on all four kinds via `version_tag()` (`gateway.rs:575-577`; offer `:107`,
  result `:428`, receipt `:544`, feedback `:551`).
- **Validation — `parse_offer` — `gateway.rs:260-263`.** *Before/after unchanged in shape:* it already
  rejects any `v != PROTOCOL_VERSION` with `OfferParseError::UnsupportedVersion`. After the bump a v1
  offer is rejected by a v2 seller and vice versa — the desired explicit refusal (§ Compatibility).
  > **Note:** the `v` tag is emitted on all four kinds but **validated only on the offer parse path**
  > (`gateway.rs:260-263`); result/receipt/feedback parsers do not check it. This piece does not add
  > validation to the other kinds (they flow inside an already-version-checked job), but the compat
  > section relies on the offer-parse check being the single gate — see § Compatibility.

---

## Config: seller `accepted_mints`

The single `mint_url` config becomes a list. Schema (`[seller]` in `config.toml`):

```toml
[seller]
# The mints this seller will accept payment at. First entry is the default the
# seller advertises first in the creq `m` list. Real-mint entries are gate-fenced
# (see Out of scope) — testnut/dev mints only in v2.
accepted_mints = ["https://testnut.cashudevkit.org"]
```

- **Default:** `vec![DEFAULT_MINT_URL.to_string()]` — i.e. exactly the current testnut default
  (`home.rs:16`), so an operator who sets nothing behaves identically to today.
- **Migration of the existing single field:** on load, if a legacy `mint_url = "…"` is present and
  `accepted_mints` is absent, map it to `accepted_mints = ["<that value>"]` (a small back-compat shim in
  the config loader, next to the existing `migrate_dead_mint_url` shim at `home.rs:439-443`). This keeps
  every current seller config working unchanged.
- **Boot validation:** non-empty; every entry a well-formed mint URL; every entry within the allow-listed
  real-mint fence (testnut/dev only in v2). The old "must equal `DEFAULT_MINT_URL`" scalar check
  (`seller_daemon.rs:330-335`) becomes "every entry is allow-listed."
- **Where the list is used:** authoring the `creq` `m` array (claim), building the redeem
  `PaymentPolicy` (`seller_daemon.rs:826`), and the redeem guard membership check
  (`payment_wallet.rs:933-940`).

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
`Transport`, `TransportBuilder`, `TransportType` (`nuts/mod.rs` in cashu 0.17.2). `PaymentRequest` has
`FromStr`/`Display` implementing the `creqA` prefix, and `TransportType::Nostr` exists. **The repo does
not use any of these today** (grep of `nut18`/`PaymentRequest` across `crates/` is empty) — the current
flow rolls its own `PaymentEnvelope`. The build MUST adopt the cashu types for `creq` encode/decode and
for the payload rather than re-implement CBOR/base64 by hand.

**What the seller authors.** `PaymentRequestBuilder` with `a = offer.amount`, `u = offer.unit`,
`m = config.accepted_mints`, one `Transport { t: Nostr, a: <seller nprofile>, g: [["n","17"]] }`,
`i = <job/attempt id>`, `s = true` (single-use — one claim, one payment). `.build()`, then `Display` →
the `creq…` string that goes in the claim's `["creq", …]` tag.

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
attempt refuses (§ Failure semantics) and the buyer retries; no partial state is committed because the
receipt only co-signs after the seller confirms redemption.

---

## Failure semantics

Simple **on purpose** — no escrow, no atomicity, no delivery-vs-payment coupling. Trust + reputation carry
the residual risk. Each row: refusal reason (machine-readable) and retry behavior.

| Situation | Detected at | Reason code | Retry semantics |
|---|---|---|---|
| Buyer pays a token from a mint **not** in `creq.m` | seller redeem guard (`payment_wallet.rs:933-940`) | `wrong_mint` | Seller refuses (kind-7000 `status=error`, reason in content); buyer re-pays from a listed mint. No funds move (token not swapped). |
| Buyer's `creq` reference is **unknown / unparseable** (stale/garbled) | buyer parse (`PaymentRequest::from_str`) or seller on payload | `unknown_creq` | Buyer aborts the attempt and re-fetches the claim; if the seller receives a payload for a `creq_hash` it never authored, it refuses `unknown_creq`. |
| **Accepted mint unreachable at PAY** (buyer can't mint/bridge) | buyer wallet (mint-quote / melt) | `mint_unreachable_pay` | Buyer retries within a bounded window (try the next mint in `m`); if all listed mints are down, buyer walks away — no payload sent, no binding. |
| **Seller mint unreachable at REDEEM** (token valid, mint down) | seller redeem (`payment_wallet.rs` receive) | `mint_unreachable_redeem` | Seller retries redemption within a window; on exhaustion it walks away (token stays unredeemed-but-not-lost, redeem-sweep is a separate PIECE, § Out of scope). No receipt is co-signed. |
| **Amount mismatch** (payload value ≠ `creq.a` after fees) | seller redeem guard (`require_received_amount_after_fee`, `payment_wallet.rs:973-977`) | `amount_mismatch` | Seller refuses; buyer re-pays the exact amount. Existing post-fee check, unchanged in shape. |

All refusals are **explicit** kind-7000 `status=error` events (`error_draft`, `gateway.rs:467-483`) whose
content carries the reason code — never a silent drop. Reputation (a separate chapter) reads these
outcomes; the money path never blocks on them.

---

## Compatibility

The version bump to `"2"` makes cross-version encounters **refuse explicitly, never silently fall back.**

- **v1 buyer → v2 seller.** The buyer emits a v1 offer (with a `mint` tag, `v=1`). The v2 seller's
  `parse_offer` rejects it: `v != PROTOCOL_VERSION` → `OfferParseError::UnsupportedVersion("1")`
  (`gateway.rs:260-263`). Today that is a soft skip (`seller_daemon.rs:517-520`) — **this piece upgrades
  it to an explicit kind-7000 `status=error` with reason `unsupported_version` keyed on the offer id**, so
  the v1 buyer gets a machine-readable refusal instead of silence.
- **v2 buyer → v1 seller.** The buyer emits a v2 offer (no `mint` tag, `v=2`). The v1 seller's
  `parse_offer` rejects it with `UnsupportedVersion("2")` and skips — the v1 seller predates the explicit
  refusal upgrade, so the buyer simply gets no claim (a v1 seller can't be taught new behavior). The v2
  buyer treats "no claim before deadline" as no-match and moves on. No silent *fallback* occurs on either
  side — neither party downgrades its schema.
- **No silent fallback rule:** a v2 seller never accepts a v1 offer by inferring a mint; a v2 buyer never
  re-emits a v1 offer to satisfy an old seller. The version tag is the hard gate.

---

## Build decomposition

Five independent marketplace jobs, each ≤ a few hundred lines. **Money files are isolated:** jobs C, D, E
touch the gate-fenced set (`seller.rs`, `payment*.rs`, `authorize_pay.rs`, `receipt.rs`,
`payment_wallet.rs`, seller daemon money path) and are **flagged for buyer-side review**. Order: schema →
seller config/gates → claim-invoice → binding → buyer pay. Each ends with an artifact-predicate
`acceptance` block (commands + expected output — never "works correctly").

### Job A — offer schema drops `mint_url` + version bump (not gate-fenced)
Remove `mint_url` from `OfferDraft`/`ParsedOffer`, stop emitting/reading the `mint` tag, bump
`PROTOCOL_VERSION` to `"2"`. `gateway.rs` only (+ its tests).

```acceptance
- [ ] grep -n 'mint_url' crates/mobee-core/src/gateway.rs  → no matches (field and tag gone)
- [ ] grep -n 'PROTOCOL_VERSION: &str = "2"' crates/mobee-core/src/gateway.rs  → 1 match
- [ ] cargo test -p mobee-core parse_offer  → passes; a v2 offer round-trips with no `mint` tag
- [ ] cargo test -p mobee-core -- unsupported_version  → a v=1 event fails parse_offer with UnsupportedVersion("1")
```

### Job B — seller `accepted_mints` config + gate removal (money-adjacent)
Replace `SellerConfig.mint_url` with `accepted_mints: Vec<String>` (default `[DEFAULT_MINT_URL]`, legacy
single-field shim), relax the boot gate to allow-list membership, delete the offer mint gate. `home.rs`,
`seller_daemon.rs`, `seller.rs` fixture.

```acceptance
- [ ] grep -n 'accepted_mints' crates/mobee-core/src/home.rs  → field defined, default = [DEFAULT_MINT_URL]
- [ ] grep -n 'NonTestnutMint' crates/mobee-core/src/seller_daemon.rs  → offer-gate skip removed (variant retired or redeem-only)
- [ ] cargo test -p mobee-core -- accepted_mints_default  → an empty config yields accepted_mints == [DEFAULT_MINT_URL]
- [ ] cargo test -p mobee-core -- legacy_mint_url_migrates  → a config with only mint_url loads as accepted_mints == [that value]
- [ ] cargo build -p mobee-core  → compiles (boot gate now checks membership, not equality)
```

### Job C — seller authors the `creq` in the claim (MONEY — buyer-side review)
Build a NUT-18 `PaymentRequest` (cashu `PaymentRequestBuilder`) from `offer.amount`/`offer.unit` +
`config.accepted_mints` + a nostr transport to the seller key, attach as a `["creq", …]` tag on
`claim_draft`. `gateway.rs`, `seller_daemon.rs`.

```acceptance
- [ ] grep -n 'PaymentRequestBuilder\|creqA\|"creq"' crates/mobee-core/src  → creq authored + tagged on the claim
- [ ] cargo test -p mobee-core -- claim_carries_creq  → the kind-7000 claim has a `creq` tag whose value starts with "creqA"
- [ ] cargo test -p mobee-core -- creq_roundtrip  → PaymentRequest::from_str(tag) yields a=offer.amount, u=offer.unit, m=accepted_mints, one nostr transport to seller
- [ ] cargo build -p mobee-core  → compiles against cashu 0.17.2 nut18 types (no hand-rolled CBOR)
```

### Job D — binding moves onto the `creq` hash (MONEY — buyer-side review)
Add `creq_hash` to `PaymentKey`/attempt hash (`payment.rs`) and to `ReceiptPreimage` (`receipt.rs`); mint
in both becomes the realized mint; thread it through `authorize_pay.rs` and add a `creq-hash` receipt tag;
delete dead `ReceiptHashInput`.

```acceptance
- [ ] grep -n 'creq_hash' crates/mobee-core/src/payment.rs crates/mobee-core/src/receipt.rs  → present in PaymentKey and ReceiptPreimage
- [ ] grep -rn 'ReceiptHashInput' crates/mobee-core/src  → no matches (dead type removed)
- [ ] cargo test -p mobee-core -- receipt_preimage  → preimage digest changes when creq_hash changes (binding is anchored to the request)
- [ ] cargo test -p mobee-core -- attempt_id  → AttemptId differs for two claims with different creq_hash, same offer
```

### Job E — buyer pay path + Lightning bridge + redeem guard (MONEY — buyer-side review)
Buyer parses the `creq`, sends a NUT-18 `PaymentRequestPayload` (bridging over Lightning if its balance is
at a non-listed mint); seller redeem guard checks token mint ∈ `accepted_mints` and equals `payload.mint`.
`authorize_pay.rs`, `payment_wallet.rs`, `payment_send.rs`.

```acceptance
- [ ] grep -n 'PaymentRequestPayload' crates/mobee-core/src/payment_send.rs  → buyer emits the NUT-18 payload (old PaymentEnvelope replaced)
- [ ] cargo test -p mobee-core -- pay_matches_creq  → a payload whose mint ∉ creq.m is refused with reason `wrong_mint`
- [ ] cargo test -p mobee-core -- lightning_bridge  → with balance only at an unlisted mint, the buyer melts→mint-quotes→sends a token from a listed mint (mock wallet)
- [ ] cargo test -p mobee-core -- redeem_guard  → seller receive accepts a token from a listed mint == payload.mint, refuses otherwise
- [ ] cargo test -p mobee-core  → full crate suite green
```

---

## Out of scope

- **Escrow, atomicity, delivery-vs-payment coupling** — deliberately not introduced; trust + reputation
  carry residual risk (§ Failure semantics).
- **Real-mint enablement** — v2 keeps the fail-closed testnut/dev allow-list; accepting real mints is a
  separate, separately-gated change (the boot allow-list is the seam).
- **Redeem-sweep** — recovering unredeemed-but-not-lost tokens after a `mint_unreachable_redeem` walk-away
  is tracked in a separate PIECE, not here.
- **Reputation scoring** — the failure-reason stream feeds a future reputation chapter; not built here.
- **Multi-mint splitting in one payment** — v2 pays from a single mint (`payload.mint` is one mint);
  splitting a payment across several accepted mints is future.
- **`v`-tag validation on result/receipt/feedback kinds** — this piece relies on the offer-parse gate
  (§ Compatibility); adding version checks to the other parsers is a separate hardening.
