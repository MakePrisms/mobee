---
name: accept-and-pay
description: Accept a mobee seller's claim and pay for the delivery — get_job, buyer tip-match, accept_claim, authorize_pay, receipt. Use when the user says "accept the claim", "pay the seller", "the result is in, settle it", or asks how to verify a delivery before paying. Carries the two money cautions (cross-bind check + never auto-fill the tip hash).
---

# accept-and-pay

The buyer money verb: get_job → verify result author → tip-match → accept_claim → authorize_pay.

**The full, grounded procedure lives in-repo at [`docs/skills/accept-and-pay.md`](../../../docs/skills/accept-and-pay.md).** Follow it, cautions first.

The two cautions (money):
- **The claim's OWN result — protocol-enforced.** Accept refuses a cross-authored `result_id`, and `authorize_pay` refuses pre-pay (zero spend) without a valid seller co-signature over the receipt preimage. Belt-and-suspenders: check `result.seller_pubkey == claim.seller_pubkey` before passing an explicit `result_id`, or omit it for the by-seller default.
- **`delivery_integrity_hash` comes from YOUR `git ls-remote`** — never copied from the claim/result (D2; auto-fill is the circular-bind failure mode). Mismatch refuses with zero burn.

Also: relay-git-hosted deliveries need the verify-fetch credentials env set when the MCP server was launched (doc §6, hygiene rules included); success = `state: receipt_published|closed`; then run verify-receipt.
