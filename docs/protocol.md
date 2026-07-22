# mobee protocol

What rides the wire. mobee coordinates over a Nostr relay, delivers as git, and settles in **cashu** ecash — mint-agnostic (the default is a test mint whose invoices auto-settle; a real mint requires real payment).

Every mobee event is in a dedicated **`3400`–`3405`** kind block and carries a mandatory
`["t","mobee"]` namespace tag; parsers and subscription filters reject anything without it.

## The trade

1. **Offer** — buyer publishes a job (kind `3401`): task, output type, capped `amount_sats`, an optional targeted seller p-tag (open-pool if omitted), and optional `repo`/`branch` for git delivery. **The offer no longer names a mint** — the seller quotes accepted mints in its claim.
2. **Claim** — seller publishes `3402` `status=processing` and attaches the NUT-18 payment request (`creq…`) it authored: accepted mint(s), amount, unit, and a NIP-17 transport to itself. **The claim is the invoice.**
3. **Result** — seller pushes a git commit to a delivery remote and publishes `3403` carrying `repo` / `branch` / `commit_oid`.
4. **Verify** — the buyer runs its *own* `git ls-remote` and tip-matches the advertised commit. The buyer's hash — never the seller's — becomes the `delivery_integrity_hash`.
5. **Award / accept** — buyer publishes `3405` `status=accepted`, binding seller + result + commit.
6. **Pay** — `authorize_pay` runs the budget gate, verifies the delivery, checks the seller's pre-pay co-signature, then satisfies the claim's `creq` with a NUT-18 payload wrapped in a NIP-17 gift-wrap (kind `1059`).
7. **Receipt** — the buyer publishes a co-signed receipt (kind `3400`) binding the `creq_hash` and realized mint. The signatures are the proof — published is not the same as valid.

Progress, errors, and refusals at any step are `3404` FEEDBACK events with a machine-readable reason code — never silent drops.

## Event kinds

| Kind | What | Author |
|------|------|--------|
| `0` | Profile metadata — optional display name | either |
| `3400` | Receipt — buyer-authored, seller co-signed | buyer + seller |
| `3401` | Job offer | buyer |
| `3402` | Claim (`status=processing`) — carries the seller's `creq` invoice | seller |
| `3403` | Job result — git `repo` / `branch` / `commit_oid` | seller |
| `3404` | Feedback — progress / error / refusal (closed reason-code enum) | seller |
| `3405` | Award / accept (`status=accepted`) — binds seller + result + commit | buyer |
| `30340` | Seller heartbeat — addressable liveness (`d="mobee-seller"`) | seller |
| `1059` | NIP-17 gift-wrap — the NUT-18 cashu payment payload | buyer |
| `31990` | NIP-89 handler announce — seller discovery | seller |
| `30617` | NIP-34 repo announce — seller delivery remote | seller |

## Invariants (the money teeth)

- **The buyer verifies, not the seller.** The paid hash comes from the buyer's `git ls-remote`, compared against the accepted commit; a mismatch refuses *before* any spend (zero burn).
- **No cross-bind.** Accept and pay refuse a result whose author is not the claim's seller, and `authorize_pay` verifies the seller's pre-pay co-signature before spending.
- **Capped.** Every pay passes a budget gate (`per_job_budget_sats`, `total_budget_sats`).
- **Fee floor.** `amount ≤ mint fee` is dust and is refused — post `≥ 2` on the fee-1 testnut mint.
- **Key custody.** Keys are `0600`, never passed on a command line, never in a token or a log.

Per-verb operator procedures — with source `file:line` grounding for each invariant — are a scrubbed follow-up (#102).
