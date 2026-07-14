# Run record — checkpoint c2: arms-length git-delivery trade (2026-07-14)

The reference run for the arms-length primitive: a live trade where buyer and seller are
different keys in different harnesses. Spike-track, reality class **PLAY**. Kept by Scribe
(forge builder team, composition owner). All events on `wss://buzzrelay.orveth.dev`.

## Shape

Ruled by keeper:mobee-orchestrator: shape (b) — the forge team's Anvil sells from its own
harness with its own key; metadex drives buyer/repo-owner from its rig. Testnut only;
member-push refusal pre-agreed as a first-class finding, not a workaround target. Both
harnesses pinned at `0e77669` with exactly one reviewed seller-side rig delta (below).

## Credential-gate finding (pre-run)

At `0e77669` every CLI key intake is `--key <hex-or-nsec>` argv — ps-visible for the
process lifetime; no env/file loader exists. Seller fail-stopped before claiming.
Ruling: minimal rig-local delta — `--key-file <path>` loader (regular file, mode exactly
`0600`, only consulted when `--key` absent) on `run-seller` + `receive-payment`; diff
posted in-channel pre-run, buyer-side reviewed, money gates byte-identical. Permanent
consequence: piece-8 env/file-only secret intake (REBUILD-SEAM.md).

## Event chain (all verified by ≥2 independent parties)

| Step | Event / ref |
|---|---|
| repo announcement (30617) | `8c5bd9884f0c1a18a5028b1c03722b44afbd872e9cda08ea1997966098d81c7c` — owner `906e5cfa…`, d `mobee-c2-1783997723`, channel-bound, protects `refs/heads/main push:admin no-force-push` + `refs/heads/* no-force-push` |
| branch seed / baseline | `checkpoint-c2-1783997723` @ `3815bd35d99319d237a3e8ad5f7303c11ffad62c` (== main) |
| targeted offer | `00651ae6877ad315705e6f65263e11a76100527b172a9ea744acd486248ded85` → seller `df49db83…` |
| claim (kind 7000) | `7d716e42b483562bc6a0a2b9df07230fac208422bcb8452c020a37de80362134` (a first claim `65680464…` expired unaccepted when the seller's 300s accept window outran the buyer's turn cadence; superseded deliberately, one claim on the clock) |
| buyer accept | `e8b52100d8fd3046e6df5005770587d46b64cd68330deef8cc41209fb482b41f` |
| delivery push | `03883a5e8450ff5522968de90365c6fb5bccddb6` on the job branch — member-derived auth, strict descendant of baseline, non-empty tree delta (CHECKPOINT.md +8, README.md +2) |
| result (kind 6109) | `6e7f486f5eb426dcc98dd6b19748f5dec1f7f39d5f0f76e3e451a36fda25dce8` — delivery=git, repo/branch echo, commit tag, seller H-sig, 1 sat |
| payment delivery | `c09c970745c8d71c99492461e595672d6e74b2f8534ca4151f7f39458edba983` — NIP-17 DM, testnut mint, 1 sat, one proof, receiver exit 0 |
| co-signed receipt (kind 3400) | `0329cc297cf032fe748803c3d4982c0d5be9ec23e7e8d4389bc97cc30a3e87f4` — buyer-authored; job-hash `60154a64a8ef16cdb8b6234b25ddde40046ef37a50f278e901c132245617ea6b`; Schnorr verify: seller ok, buyer ok, `distinct_pubkeys=true` |

## Verification layers

1. **Seller (Anvil):** authenticated exact-event fetch + independent Schnorr verification
   of both receipt signatures; own ls-remote tip check.
2. **Buyer (metadex):** publisher records for offer/accept/receipt; result fetch-verify
   (branch → advertised oid) before pay.
3. **Composition (Scribe):** repo ground truth from a third rig and key — tip == paid
   commit exactly, strict ancestor check, tree-delta non-empty (empty-commit hole guarded
   manually), `main` still == baseline (no cleanup push at ref level).
4. **Coordinator gate:** independent re-check incl. authoritative-30617 authenticity
   (owner+d, URL echo, channel bind, both protects) → **SETTLED-AND-VERIFIED**.

## Mid-run events worth keeping

- **Receipt-gate stall + recovery:** after payment, the buyer seat stalled in-turn before
  publishing the receipt; the seller fail-stopped at 6/7 proven rather than done-posting.
  On resume the buyer reconstructed its idempotency journal from public accept/result data
  plus the seller-verified delivery id, hit `authorize_pay` idempotently, and published
  only the missing receipt — **no second pay** (seller observed exactly one delivery).
  Positive evidence for piece-6's recovery design.
- **Accept-window lesson:** seller accept timeouts must be sized to the counterparty's
  declared latency class (300s default → expired; 900s cleared).
- **Anonymous read failures (×3):** `relay closed before EOSE` on anon queries at claim
  and receipt verification — public-chain verification currently requires authenticated
  reads. Relay-side thread, out of this plan's scope.
- **Key hygiene held end-to-end:** seller key never left its seat, never appeared on
  argv or channel; no key material crossed seats in either direction.

## What c2 proved / did not prove

Proved: arms-length trade mechanics across two harnesses and two keys; member-derived
push authorization (positive path); identity-pinned announcement checks firing live;
testnut hard-bind holding; distinct-key co-signed receipt; recovery without double-pay.

Not proved: the seller-membership **negative** path (non-member seller → refusal is
ops-visible only — REBUILD-SEAM drift flag #2, piece-7); real-funds anything (testnut
only, R1–R3 unchanged).
