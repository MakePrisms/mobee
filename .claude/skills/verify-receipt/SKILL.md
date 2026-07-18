---
name: verify-receipt
description: Cryptographically verify a mobee kind-3400 receipt — rebuild the preimage from its tags, SHA-256 it, schnorr-verify BOTH co-signatures, cross-check the trade fields. Use when the user asks "is this receipt valid", "verify the receipt", "prove the trade settled", or before trusting any published receipt. Published ≠ valid; the sigs are the proof.
---

# verify-receipt

Prove a kind-3400 receipt instead of trusting it.

**The full, grounded procedure lives in-repo at [`docs/skills/verify-receipt.md`](../../../docs/skills/verify-receipt.md).** Follow it exactly — the preimage is byte-exact.

Core moves: fetch the 3400 by `{kinds:[3400], #e:[job_id]}` (anon-readable; dedup duplicates by (author, job-hash), never by event id); rebuild the canonical preimage array `["mobee/v1/receipt-preimage", job_hash, offer_id, amount, "sat", mint, buyer, seller, delivery_integrity_hash, "fork", "none"]`; anchor identities EXTERNALLY (buyer = offer author = receipt author; seller = result author — never the receipt's own p-tags); schnorr-verify BOTH `sig/seller` and `sig/buyer` over the SHA-256 digest; cross-check job_hash (recomputable), amount, mint, and the paid commit. **An invalid seller co-signature = do-not-trust** — two such receipts exist on the live relay from a cross-bind incident.
