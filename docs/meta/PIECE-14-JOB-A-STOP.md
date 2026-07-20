# PIECE-14 Job A — STOP report: offer `mint_url` removal cannot compile standalone

**Status: no code changes delivered.** Per the job contract — *"if that fallout reaches into
seller.rs/seller_daemon.rs/payment files beyond deleting a field access or a fixture line, STOP and
deliver a written explanation instead"* — that condition is triggered. This document is the
deliverable.

All `file:line` references are to the current tree (branch head `089fc06`), which matches the design
doc's `dev@fb64946` anchor at every cited site.

## Why Job A cannot be built within its fence

Job A's scope is `gateway.rs` only: remove `OfferDraft.mint_url` (+ the emitted `mint` tag), remove
`ParsedOffer.mint_url` (+ the parse read), bump `PROTOCOL_VERSION` to `"2"`. The acceptance requires
`cargo build --workspace` green.

Removing `OfferDraft.mint_url` is fine — its only out-of-crate consumers are constructor call sites
(`job_lifecycle.rs:431/:439` via `build_offer_draft`, plus test fixtures), all mechanical argument
deletions.

Removing **`ParsedOffer.mint_url`** is the blocker. It has six live consumers in exactly the
gate-fenced files, and none of them is fixable by deleting a field access — each one *uses the value*
and needs a replacement mint source, which is the design decision the later jobs own:

1. **`seller_daemon.rs:522-526` — the offer mint gate.**
   `if offer.mint_url != DEFAULT_MINT_URL { Skip(OfferSkip::NonTestnutMint { mint_url }) }`.
   The fix is to delete the gate and retire the `OfferSkip::NonTestnutMint` variant — which is
   verbatim **Job B** scope (design §(b), Job B acceptance greps for `NonTestnutMint` removal).

2. **`seller_daemon.rs:812` + `:826-827` — redeem-path `PaymentPolicy`.**
   `let mint = job.offer.mint_url.clone();` … `PaymentPolicy::new([mint_url(&mint)?])` …
   `policy.terms_for_offer(&offer, …)`. This is the seller's money redeem guard. Without the offer
   field the mint must come from somewhere else (`config.accepted_mints` per design §(b)) — a
   money-semantics substitution assigned to **Job B** (and reshaped again by **Job E**).

3. **`seller_daemon.rs:1043` — receipt binding.**
   `ReceiptPreimage { … mint: active.offer.mint_url.clone(), … }`. This is the co-signed receipt
   preimage; re-sourcing `mint` to the *realized* mint is **Job D** (design §(d)). Any stopgap here
   changes what both parties cryptographically commit to.

4. **`seller_daemon.rs:1483` — journal episode fill.**
   `episode.mint = offer.mint_url.clone();` (Piece-13 offer-facts). Needs a substitute value or a
   schema change to the journal — not a deletion.

5. **`payment_wallet.rs:95-101` — `terms_for_offer`.**
   `MintUrl::from_str(&offer.mint_url)` builds the `PaymentTerms` the redeem guard enforces. Design
   §(b)/(e) says "`terms_for_offer` no longer reads a mint off the offer" — that rewrite is **Job
   B/E** territory, plus its tests (`payment_wallet.rs:1065/:1080/:1100`, fixture `:2340-2341`).

6. **`seller.rs:479-486` — `ParsedOffer` test fixture.** The design doc itself defers this: *"this
   fixture edit rides with job B"* (§(a), gateway.rs:104-105 of the doc). Allowed as a fixture line,
   but moot given 1–5.

Additionally, on the buyer side, `job_lifecycle.rs` `OfferView.mint_url` (`:134`, populated from
`parsed.mint_url` at `:1126-1128`) flows into the MCP driver's `get_job`/list output
(`mobee/src/mcp.rs`), so the field deletion also ripples into the buyer-facing tool schema — outside
the fence list but beyond "gateway.rs plus its own tests".

## Why this was foreseeable from the design doc — and what the doc under-states

Design §(a) lists the Job A fallout as gateway.rs lines plus one seller.rs fixture. But the doc's own
§(b)–(d) inventories cite consumers 1–3 above as *Job B and Job D change points* — i.e. the design
already knows `ParsedOffer.mint_url` is load-bearing in the fenced files. The build order
"schema → gates → claim → binding → pay" therefore has a dependency inversion: **the schema field
cannot be deleted until every downstream reader has been re-pointed**, which happens in B, D and E.
Job A as specified compiles only in a workspace where B/D/E have already landed.

## What was NOT delivered, deliberately

No partial implementation (e.g. bumping only `PROTOCOL_VERSION`, or removing only the `OfferDraft`
side while keeping `ParsedOffer.mint_url`) — the job says implement exactly the Job A scope or stop,
and a half-removed schema would leave the tree in a state no later job description matches.

## Options for the buyer (pick one, re-post)

1. **Reorder: land Job A last.** Run B (gates + `accepted_mints`), then D (binding), then E
   (pay/redeem re-source) first — each keeping `ParsedOffer.mint_url` temporarily unread — and make
   A the final cleanup that deletes the now-dead field and bumps the version. A then really is
   gateway.rs-only.
2. **Merge A into B, and pre-authorize the two remaining stopgaps.** A+B together resolves consumers
   1, 2, 4, 6. Consumers 3 (`receipt` mint) and 5 (`terms_for_offer`) still need an authorized
   interim source (e.g. `DEFAULT_MINT_URL` / the policy's single allowed mint) until D/E land — an
   explicit money-path decision that needs buyer-side review, which is exactly what the fence exists
   to force.
3. **Widen Job A's fence explicitly.** Re-post Job A authorizing a named, minimal substitution at
   each of the six sites (spelled per-site, before→after, as §(a)–(d) of the design doc do), so the
   seller is not making unreviewed money-path choices.

Option 1 is the cleanest: it preserves the fence, keeps every job compilable, and changes only the
order, not the content, of the decomposition.
