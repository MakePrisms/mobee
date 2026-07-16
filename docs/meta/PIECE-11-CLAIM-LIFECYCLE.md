# Piece-11 design — job/claim lifecycle hardening (the orphaned-claim fix)

Charter-2 CORE. Class: **MONEY-ADJACENT** (lifecycle / daemon / buyer-view only — the frozen
money-core stays byte-unchanged). This doc is the locked source of truth for claim-lifecycle
**states + transitions + derived expiry** and the three required behaviors.

## The scar this closes

A seller daemon restarted mid-execution (evidence job `0867a213`). Its claim still read live
`processing` 25+ minutes later — past the deadline — with nothing surfacing expiry. Three
independent gaps produced that one symptom:

1. **`active` is in-memory only** — a restart loses the processing slot, but the claim was
   already journaled + kind-7000-published, so a live claim is orphaned with no owner.
2. **Nothing derives expiry** — `get_job` reported the claim's raw relay status (`processing`)
   forever; deadline was never compared against "now".
3. **Single-flight was coupled to delivered-unpaid** — a delivered-but-unpaid job held the
   only slot, so new offers were dropped **silently** (issue #15).

## States

```
                 claim 7000 + journal            agent running
   (idle) ───────────────────────────▶ CLAIMED ───────────────▶ PROCESSING
                                                                     │  kind-6109 result
                                                                     ▼
                                       PAID ◀── payment ─── DELIVERED (unpaid)
   receipt journaled + redeemed          │  received/redeemed
                                         ▼
                                       CLOSED
```

| State | Meaning | Single-flight slot? |
|-------|---------|---------------------|
| `CLAIMED` | kind-7000 `status=processing` published **and** journaled | held |
| `PROCESSING` | agent running toward delivery | **held** |
| `DELIVERED` | kind-6109 result published, **unpaid** | **released** |
| `PAID` | payment received + redeemed, receipt journaled | — |
| `CLOSED` | terminal success | — |

Off-nominal:

| State | Trigger | Who publishes |
|-------|---------|---------------|
| `EXPIRED` | `now > deadline_unix` and not `PAID` — **derived, never stored** | nobody (buyer derives it in `get_job`); seller RELEASEs on restart |
| `RELEASED` | seller can't/won't resume (e.g. restart) → gives up the claim | seller → kind-7000 `status=error` |
| `FAILED` | agent error / push fail / timeout | seller → kind-7000 `status=error` |

`RELEASED` and `FAILED` are the same wire event (kind-7000 `status=error`, via
`gateway::error_draft`); they differ only in the journaled reason. There is no dedicated
kind-7000 "release" status in the protocol — reusing `error` keeps the buyer view simple
(`error` claims are never live) and needs no relay/schema change.

## Derived-expiry rule (the invariant)

> A claim is **EXPIRED** when `now > deadline_unix` and it is not `PAID`.
> Expiry is **DERIVED from an injected `now`**, never stored and never read from the wall
> clock inside a pure path.

- Buyer side: `deadline_unix` is the **offer** deadline (`OfferView.deadline_unix`), the same
  value the seller claimed against. `get_job` compares it to `now`.
- Seller side: the deadline is **journaled on the claim** (`JournalEntry::Claim.deadline_unix`)
  so a restarted daemon can classify expiry offline, without re-fetching the offer.

## The three behaviors (each a MUST, each with a test)

### 1. Single-flight decoupled from delivered-unpaid (#15 silent-drop)

- The single-flight slot (`FLIGHT` + `SellerDaemon.active`) is held **only** for the
  `PROCESSING` phase (claim → deliver). On delivery it is **released**
  (`SellerDaemon::mark_delivered`): the job moves to `awaiting_payment: Vec<ActiveJob>`, which
  does **not** gate new claims.
- A delivered-but-unpaid job **MUST NOT** block claiming/serving new offers.
- Every skip **MUST** be logged with a named reason — there is no silent `return Ok(None)`.
  Reasons are enumerated in `OfferSkip` and rendered by `OfferSkip::reason()`; the admission
  decision is `SellerDaemon::classify_offer` (pure, no relay I/O, `now` injected).
- Payment binding is unchanged in substance: a payment is redeemed **only** against the
  delivered job whose `(job_id, result_id)` it declares (exact match in `try_apply_payment`),
  so decoupling never enables misattribution. Unmatched wraps are buffered, never errored.

Test: `seller_daemon::tests::delivered_unpaid_does_not_block_new_offer_but_processing_does`.

### 2. Restart-reconcile (no silently-orphaned live claim)

- On startup, the daemon reads journaled in-flight claims — journaled `Claim` with **no**
  matching `Receipt` (paid) and **no** matching `Release` (terminal) — and for each either
  resumes within deadline or **RELEASEs**.
- **v1 conservatively RELEASEs every orphan** (money-adjacent: resuming lost in-memory
  execution state — partial agent work, no offer in memory — is not safe to auto-do). The
  lifecycle keeps `RESUME` as a real state; wiring a safe resume (re-fetch + re-verify the
  offer, re-run) is deferred and named here.
- Release is **durable first**: `SellerDaemon::reconcile_journal` writes a terminal
  `JournalEntry::Release` (no relay) so the orphan can never read live again and is never
  re-claimed. It is **idempotent** — a second restart releases nothing.
  `reconcile_on_startup` then best-effort publishes kind-7000 `error` to surface it to the
  buyer (publish failure is logged, not fatal — the buyer view also derives expiry).
- Pure plan: `seller::plan_orphaned_claims(entries, now)` classifies each orphan
  `Expired`/`Live` by the injected `now`. `run_forever` calls `reconcile_on_startup` after
  NIP-42 auth, **before** serving offers.

Test: `seller_daemon::tests::reconcile_journal_releases_real_orphaned_claim_and_is_idempotent`
and `seller::tests::plan_orphaned_claims_from_real_journal_marks_past_deadline_expired` — both
over a **real** journal fixture (journaled in-flight claim + past deadline), no relay mock.

### 3. `get_job` expiry (buyer view)

- `get_job` derives claim liveness from `now` vs `deadline_unix` (+ seller status) via the
  pure `derive_claim_liveness(claims, offer_deadline_unix, now)`.
- A `processing` claim past its deadline surfaces as `status = "expired"`
  (`CLAIM_STATUS_EXPIRED`), `live = false`, and is **excluded from `live_claim_id`**.
- `now` is an **input**, injected at the (impure) call sites (`get_job_async`,
  `accept_claim_async`); the derivation never calls the wall clock (tests pass a fixed `now`).
- Side effect: `accept_claim` now refuses an expired claim (its status is no longer
  `processing`) — you cannot accept past the deadline.

Test: `job_lifecycle::tests::processing_claim_past_deadline_is_expired_not_live`,
`liveness_flips_with_injected_now_only`, and neighbours.

## Back-compat

- `JournalEntry::Claim` gained `deadline_unix` / `claim_id` / `buyer_pubkey`, all
  `#[serde(default)]`. **Pre-piece-11 claim lines still parse** (missing → `0` / `""`). A
  legacy claim with `deadline_unix = 0` classifies `Expired` for any `now > 0` — the safe
  default: an old orphan is released, never left live.
- New terminal variant `JournalEntry::Release`. Old journals simply have none; reconcile adds
  them going forward. `has_claim` now also treats a `Release` as "seen" (no re-claim).
- `ClaimView` gains no field; an expired claim reuses the existing `status` string
  (`"expired"`) — no new buyer-view schema.
- Frozen money-core (`payment_wallet.rs`, `authorize_pay.rs`, `payment.rs`) is **byte-unchanged**.

## Known limitations + forward-compat (v1)

- **Revenue-forfeiture across a crash/eviction (money-SAFE — not a safety bug).** The
  delivered-but-unpaid binding lives only in the in-memory `awaiting_payment` Vec (it is **not**
  journaled). A crash in the deliver→pay window, or eviction past the cap (16 delivered-unpaid),
  means a later payment for that job is buffered/ignored and never redeemed; on restart the job
  (Claim, no Receipt, no Release) is RELEASEd. This forfeits **revenue, never safety** — the money
  was never received, no receipt is released, no double-pay. It is ~pre-existing (base also lost
  `active` on restart and buffered the payment into the void); piece-11 improves on base by
  surfacing a kind-7000 to the buyer. Fixing it (journal the delivered-unpaid binding so a payment
  survives a restart) is a named follow-up, **not** core money-safety.
- **Release vocabulary stays OPEN (forward-compat).** `RELEASED` reuses kind-7000 `status=error`
  — the seller's only honest exit today (there is no claim-retract event, and claim-then-renege
  *for gain* is forbidden by the business-manager design). A future fast-claim / decline path
  **extends this same claim-release vocabulary** rather than inventing a conflicting one; the state
  machine is intentionally left open here, not specced shut.
- **`RESUME` deferred** (see § behavior 2): v1 RELEASEs every orphan. Ground truth — today's
  working resolution is release + buyer re-post; a safe resume (re-fetch + re-verify the offer,
  re-run) is future work.
- **`awaiting_payment` `.expect(result_id)` (seller_daemon.rs) is a panic-on-invariant** — low
  severity. It holds structurally today (only `mark_delivered` pushes, on the Ok path after
  `result_id` is set; failures go through `fail_active`, which never pushes). A future refactor
  that pushed a no-result job should return an error, not panic — a named hardening, not a live
  defect.

## Files

- `crates/mobee-core/src/seller.rs` — journal (`Claim` fields, `Release`, `append_claim`,
  `append_release`, `has_release`), `OrphanClaim` / `ClaimLiveness`, pure `plan_orphaned_claims`.
- `crates/mobee-core/src/seller_daemon.rs` — `awaiting_payment`, `classify_offer` / `OfferSkip`
  / `OfferDisposition`, `mark_delivered`, `reconcile_plan` / `reconcile_journal` /
  `reconcile_on_startup`, run-loop + startup wiring.
- `crates/mobee-core/src/job_lifecycle.rs` — `CLAIM_STATUS_EXPIRED`, pure
  `derive_claim_liveness`, injected `now` through `fetch_job_view_async`.
