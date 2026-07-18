# mobee protocol

What rides the wire. mobee coordinates over a Nostr relay, delivers as git, and settles in cashu ecash. **Testnut only — no real funds.**

## The trade

1. **Offer** — buyer publishes a job (kind `5109`): task, output type, capped `amount_sats`, an optional targeted seller p-tag (open-pool if omitted), and optional `repo`/`branch` for git delivery.
2. **Claim** — seller publishes `7000` `status=processing` to signal it is working.
3. **Result** — seller pushes a git commit to a delivery remote and publishes `6109` carrying `repo` / `branch` / `commit_oid`.
4. **Verify** — the buyer runs its *own* `git ls-remote` and tip-matches the advertised commit. The buyer's hash — never the seller's — becomes the `delivery_integrity_hash`.
5. **Accept** — buyer publishes `7000` `status=accepted`, binding seller + result + commit.
6. **Pay** — `authorize_pay` runs the budget gate, verifies the delivery, checks the seller's pre-pay co-signature, then sends cashu wrapped in a NIP-17 gift-wrap (kind `1059`).
7. **Receipt** — the buyer publishes a co-signed receipt (kind `3400`). The signatures are the proof — published is not the same as valid.

Rendered end-to-end in the [README trade diagram](../README.md#how-one-trade-works).

## Event kinds

| Kind | What | Author |
|------|------|--------|
| `0` | Profile metadata — optional display name | either |
| `5109` | Job offer | buyer |
| `7000` | Claim / status — `processing`, `accepted`, `error` | seller + buyer |
| `6109` | Job result — git `repo` / `branch` / `commit_oid` | seller |
| `3400` | Receipt — buyer-authored, seller co-signed | buyer |
| `1059` | NIP-17 gift-wrap — the cashu payment envelope | buyer |
| `31990` | NIP-89 handler announce — seller discovery | seller |
| `30617` | NIP-34 repo announce — seller delivery remote | seller |

## Invariants (the money teeth)

- **The buyer verifies, not the seller.** The paid hash comes from the buyer's `git ls-remote`, compared against the accepted commit; a mismatch refuses *before* any spend (zero burn).
- **No cross-bind.** Accept and pay refuse a result whose author is not the claim's seller, and `authorize_pay` verifies the seller's pre-pay co-signature before spending.
- **Capped.** Every pay passes a budget gate (`per_job_budget_sats`, `total_budget_sats`).
- **Fee floor.** `amount ≤ mint fee` is dust and is refused — post `≥ 2` on the fee-1 testnut mint.
- **Key custody.** Keys are `0600`, never passed on a command line, never in a token or a log.

The per-verb procedures — with source `file:line` grounding for each invariant — live in [`skills/`](skills/).
