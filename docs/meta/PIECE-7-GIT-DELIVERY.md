# Piece-7 — git delivery verification · MONEY

Source of truth for the git-delivery gate. Simplified shape ruled by the operator
2026-07-14 ("keep it simple for now, but build to where we want to go"). Buyer-side
verification that the advertised work is in the buyer's custody before payment is
authorized. Composes with piece-6 as an injected effect; no state-machine change.

## Shape (locked, simplified)

1. **Seller** pushes the work branch to the repo named in the offer and advertises the
   commit OID in the result event (existing result kind + delivery fields, spike vocabulary).
   Seller push conventions are spec text, not mobee code — sellers already push.
2. **Buyer, pre-pay:** fetch the advertised branch; the advertised full commit OID must
   resolve **exactly** (tip-match). The buyer's local fetch **is** custody —
   possession-solves-durability: the buyer holds the objects locally before authorizing pay.
3. **Content verification is the buyer agent's job** — it reads the diff to decide the work
   is worth paying for. No descendant / identity / repo-protection gates as protocol MUSTs.
4. **Receipt** binds the verified commit OID as `delivery_integrity_hash` (existing H-tuple
   field) — no second hash invented.

## Build shape

- Library in `mobee-core`, feature-gated like `wallet`; the **default build carries no git
  dependency**.
- Delivery verification = fetch + exact-tip-match resolve, returning a typed
  `VerifiedDelivery { commit_oid }` the piece-6 flow consumes **before `Intent`**.
- **Composition point:** an injected delivery-verify effect fired **before the first journal
  write**. Refusal ⇒ journal empty + zero wallet effects (maps to piece-6's write-ahead —
  nothing commits until delivery verifies). **SM transitions unchanged**; if a transition
  change proves necessary, flag before building it.
- Buyer-side only. `git` mechanism is the builder's call (argv-only subprocess, `gix`, or
  `git2`); justify the choice + name any executable dependency in the PR body.
- Repo residency: `rebuild/piece-7-git-delivery` off landed main; draft PR, file-backed
  heads protocol.

## Deferred hardening (named, NOT deleted — the five-gate shape)

The spike ran the full gate set; the simplified shape keeps three of them as **deferred
hardening**. Each names the attack it covers and the condition that justifies re-adding it.
Do **not** delete this knowledge; do **not** build these now.

- **strict-descendant** (spike `verify_git_descendant`) — covers a seller advertising a commit
  that is not a descendant of the agreed baseline (replay / unrelated-history swap). Re-add
  when verification is automated/blind (no human reading the diff), or for high-value trades.
- **kind-30617 identity-pin** (spike `ensure_repo_job_protection`) — covers a repo whose
  authoritative owner/`d`-tag identity is spoofed, or a force-push replacing history. Re-add
  when the relay/repo host is untrusted (hostile-relay), or repo authorization must be
  machine-proven rather than agent-judged.
- **buzz-protect / no-force-push** (spike ref-pattern + channel bind) — covers post-delivery
  history rewrite that moves the advertised tip out from under a paid receipt. Re-add with the
  identity-pin, same hostile-relay / high-value conditions.

Re-add triggers, in one line: **automated/blind verification · high-value trades · hostile
relay.** Until then, the buyer agent reading the diff (point 3) is the trust anchor.

## Acceptance (MONEY bar: composition + Temper adversarial + codex deep → COMPOSED-DONE)

- tip-match: advertised OID resolves exactly on fetch; wrong/moved tip = fail-closed refuse
  (named RED→GREEN).
- custody: verification returns only after objects are local (fetch completed); the typed
  result carries the OID.
- no network trust: repo URL / identity taken from offer/result events as data; no gate
  claims beyond tip-match.
- deferred-hardening section present (above) with the three gates + re-add conditions.
- default build carries no git dependency (feature-gated).
- piece-6 composition demonstrated with the `test-support` fakes: delivery-verify effect
  refuses ⇒ no `Intent` (journal empty, zero wallet effects).

## Fence

Buyer-side only. Not in piece-7: seller push automation (spec text), the three deferred gates,
CLI/MCP re-skin (piece-8). Reality class **BUILT-BUT-OFF** until the composed loop runs on main.

## Reference

Spike gates (refuse-list source, not copy): `cli.rs` `verify_git_delivery` :4388,
`verify_git_descendant` :4412, `ensure_repo_job_protection` :2779,
`parse_relay_git_repo_identity` :4123. Live proof of the delivery loop: checkpoints (c)/(c2),
[RUNS-C2.md](RUNS-C2.md). Composes onto piece-6 ([PIECE-6-PAYMENT-SM.md](PIECE-6-PAYMENT-SM.md)).
