# Cross-bind pre-pay tooth (money-path bugfix)

> **Status: FIXED on `dev`.** The cross-bind is closed by protocol teeth — `select_result` refuses a cross-authored result, and `authorize_pay` verifies the seller's pre-pay co-signature before any spend. This is the incident + fix writeup (dated below), not an open bug.

## The live bug (HIGH)

A buyer paid 21 sats on a **cross-bind**: seller **A**'s claim was accepted with seller
**B**'s *result*, so the buyer paid A (the claim/target seller, whose p2pk lock the token
is bound to) for an artifact B produced. The co-signed receipt (piece-9) caught it — the
published kind-3400 fails seller-cosig verification, publicly and unforgeably — but only
**after** the sats moved. Detection, not prevention.

## Class

**An authorization gate placed downstream of the irreversible effect.** The seller
co-signature is the buyer's proof it is paying the right party for the bound object, yet it
was verified only at the `Sent → ReceiptPublished` transition — i.e. *after* the send. The
receipt was a post-hoc detector; nothing verified the seller's authorization *before* the
spend. Compounded by an accept path that trusted an operator-supplied `result_id` without
binding `result.author == claim.seller`.

## Spend-vs-verify ordering AS IT WAS (traced at d80b1b8)

Accept (`job_lifecycle.rs`):
- `accept_claim_async` → `select_result(results, claim.seller_pubkey, request.result_id)`
  (`job_lifecycle.rs:449`). `select_result` (`:932`) — when `result_id` is `Some(id)` —
  returned the by-id result **without** checking its author against the claim seller
  (`:937-942`). Only the auto path (`result_id == None`) filtered by author (`:945`). So an
  explicit `result_id` from a *different* seller was bound: `AcceptedBind { seller_pubkey =
  claim.seller (A), result_id / commit_oid / seller_signature = the other seller's (B) }`
  (`:481-495`).

Authorize / pay (`authorize_pay.rs` → `payment.rs`):
- `authorize_pay_async` builds `PaymentKey`, `PaymentTerms` (p2pk-locked to the claim
  seller), the `ReceiptAuthority { buyer, seller }`, and captures `seller_signature`
  (`:213-223`). It then calls `gate.authorize_then_attempt(attempt_id, amount, || run())`
  (`:256`). The budget gate commits **spent before** the closure runs (write-before-mint,
  `budget.rs:190-209`), and `run()` advances the SM.
- SM (`payment.rs::PaymentService::advance`): `Intent → Locked` = `lock_or_reconcile`
  (**the wallet mint/lock**, `:708`); `Locked → Sent` = `send_payment` (**the SPEND**,
  `:734`); `Sent → ReceiptPublished` = `authority.verify(publish_receipt(..))` — **the only
  place the seller signature was verified** (`:751`, via `ReceiptAuthority::verify` →
  `verify_schnorr_hex`, `:500-544`).

So the seller-cosig check sat *two transitions after the send*. The `seller_signature` was
merely carried (request → the receipt-publish closure) until publish. That is **why piece-9
protects the receipt transition but not the spend**: `verify` runs at
`Sent → ReceiptPublished`, reachable only once the money has already moved.

## The fix — two teeth + one seam

1. **Accept refuses cross-authored results** (`job_lifecycle.rs::select_result`). Whichever
   result is selected — explicit `result_id` **or** auto — its author (kind-6109 event
   pubkey) must equal the claim seller, else refuse (`Targeting`, naming both public keys).
   Operator input is not trusted. In the accept path itself, not the tool/UI layer.

2. **THE LOAD-BEARING TOOTH — pre-pay seller-cosig verification, fail-closed**
   (`authorize_pay.rs`). Before `gate.authorize_then_attempt` (and before the wallet opens),
   rebuild the EXACT receipt preimage the pay path will publish (one shared constructor,
   `receipt_preimage_for`, used by both this gate and `build_and_publish_receipt` so they
   cannot drift) and verify the seller's schnorr signature over it against the **claim-seller
   anchor**. No valid seller cosig ⇒ refuse with **zero spend** (`gate.spent()==0`): no
   `authorize_then_attempt`, no lock/mint/send, no receipt, no payment-journal record.

3. **Shared seam** — the pre-pay verification is one named point,
   `ReceiptAuthority::verify_seller_prepay_cosig` (`payment.rs`), reusing the frozen
   `receipt.rs` preimage machinery and the same `verify_schnorr_hex` as the post-spend
   `verify`. **Extension point (doc-only here):** piece-10 Step-1 (freelance-PR fork,
   `PIECE-10-FREELANCE-PR-DELIVERY.md`) adds its signed-6109 tuple bind
   `{job_id, seller_pubkey, target_repo, base_oid, fork_ref, commit_oid}` as an additional
   checked seller bind **at this one seam** — never a parallel pre-pay gate.

Rider: the seller's own preimage (`seller_daemon.rs`) now derives `delivery_kind` from the
typed `Delivery::Commit(..).delivery_kind()` (was a `DeliveryKind::Fork` hardcode) so buyer
and seller derive the discriminator from the same abstraction. Behavior-identical (`"fork"`).

Frozen: `payment_wallet.rs`, `receipt.rs` byte-unchanged (reused read-only). Valid-pair
artifacts are byte-identical (proven by the Step-0 equivalence harness).
