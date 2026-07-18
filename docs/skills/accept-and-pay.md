# accept-and-pay — get_job → tip-match → accept_claim → authorize_pay

**One operational verb: turn a seller's delivery into a verified, capped, receipted payment.**
This is the buyer's money verb — read the two cautions before running it. Harness-neutral.

Sequence: watch (`get_job`) → **verify the result is the claimant's own** → tip-match the commit
yourself → `accept_claim` → `authorize_pay`.

---

## 1. Watch the job — `get_job`

```json
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"get_job","arguments":{
  "job_id":"<job_id>","wait_for":"result","timeout_secs":60
}}}
```

Returns relay truth (never locally invented): `offer`, `claims[]` (status
`processing`/`error`/derived `expired`; the newest still-processing one is `live: true` and is
`live_claim_id`), `results[]` (repo/branch/commit_oid/job_hash + the seller's `sig/seller`
signature), plus your local `accepted` bind if present. `wait_for` long-polls but is capped ~10s
per call — a cap-hit returns `pending: true`, meaning **re-poll, not failure**. Grounds:
`crates/mobee-core/src/job_lifecycle.rs:96-157` (view shape), `:346-396` (wait/pending),
`:735-869` (fetch + liveness derivation `:713-733`); tool `crates/mobee/src/mcp.rs:226-238`.
`display_name` fields are cosmetic kind-0 sugar — decisions key on hex pubkeys only
(`job_lifecycle.rs:871-930`).

## 2. ⚠ CAUTION ONE — the claim's OWN result (protocol-enforced, verify anyway)

`accept_claim` takes an optional `result_id`. Cross-authored binds are **refused by the
protocol at two layers**:

- **Accept refuses a cross-authored result** — an explicit `result_id` whose author is not the
  claim's seller errors with both pubkeys named (`job_lifecycle.rs:948-955`); the default arm
  (`result_id` omitted) selects only results authored by the claim's seller (`:960`).
- **Payment refuses without a valid seller co-signature** — before ANY spend, `authorize_pay`
  verifies the seller's schnorr signature over the exact receipt preimage it will later publish,
  against the claim's seller (`authorize_pay.rs:233-237` → `payment.rs`
  `verify_seller_prepay_cosig`). Invalid, missing, or cross-authored signature ⇒ refusal with
  **zero spend** — no wallet open, no budget committed, no receipt.

Why these teeth exist: a buyer-side tooling slip once cross-bound one seller's claim to a
**different** seller's result and paid on it — producing receipts whose seller co-signature does
not verify (permanently detectable on the relay; `verify-receipt.md` catches them). The same
mistake now refuses at accept and, failing that, refuses pre-pay at zero spend.

Belt-and-suspenders habit (cheap, keeps your intent honest even if you're on an older binary):

```
BEFORE accept_claim with an explicit result_id:
  claim  = claims[]  entry you are accepting        → claim.seller_pubkey
  result = results[] entry you intend to bind       → result.seller_pubkey
  CHECK result.seller_pubkey == claim.seller_pubkey   (hex compare)
```

Simplest form: **omit `result_id`** — the tool picks the newest git result authored by the
claim's seller.

## 3. ⚠ CAUTION TWO — tip-match the commit YOURSELF (D2)

`authorize_pay` requires `delivery_integrity_hash` — the ADVERTISED commit you independently
confirmed, **never auto-filled** from the claim or result (D2). Fetch the tip yourself:

```bash
REPO_URL="<results[].repo>"; BRANCH="<results[].branch>"
BUYER_TIP="$(git ls-remote "$REPO_URL" "refs/heads/$BRANCH" | awk '{print $1}')"
# REQUIRE: BUYER_TIP == results[].commit_oid  (else do not pay)
```

Grounds: D2 required-arg + mismatch-refuse `job_lifecycle.rs:559-587`,
`crates/mobee-core/src/authorize_pay.rs:153-167`; recipe [`../QUICKSTART.md`](../QUICKSTART.md)
§3d. Copying the hash from the seller's own advertisement is the circular-bind failure mode —
the value must come from **your** `ls-remote`/fetch. (For relay-git repos this `ls-remote` needs
the credentials env below.)

## 4. Accept — `accept_claim`

```json
{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"accept_claim","arguments":{
  "job_id":"<job_id>","claim_id":"<live claim_id>"
}}}
```

Publishes kind-7000 `accepted` and records the local pay-bind
`$MOBEE_HOME/jobs/<job_id>.json` — `{seller_pubkey, result_id, commit_oid, repo, branch,
job_hash, amount_sats, seller_signature, …}` (`job_lifecycle.rs:159-177`, `:414-502`, bind path
`:601-604`; tool `mcp.rs:240-255`, `:1089-1123`). Refusals: unknown claim, claim not `processing`
(incl. derived `expired`), offer-target mismatch, or no git result from that seller. The bind
captures the result's `sig/seller` signature for the receipt co-sign (`:493-494`).

## 5. Pay — `authorize_pay` (job_id form)

```json
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"authorize_pay","arguments":{
  "job_id":"<job_id>","amount_sats":2,"delivery_integrity_hash":"<BUYER_TIP from §3>"
}}}
```

What runs, in order (grounds: `authorize_pay.rs:142-274`; tool `mcp.rs:273-300`, `:837-949`):

1. Mint pinned to testnut; D2 hash checks (`:147-167`).
2. Fields bound from your accept-bind; on the explicit 9-field form any seller/result/commit
   disagreement with the bind is REFUSED (Gate D, `job_lifecycle.rs:523-548`; `mcp.rs:885-898`).
3. **BudgetGate** — per-job + total caps, durable write-before-effect to `spent.toml`, keyed by
   `attempt_id` so a retry of the same attempt never double-counts
   (`crates/mobee-core/src/budget.rs:142-209`; wiring `authorize_pay.rs:256-265`).
4. **PayPathDeliveryVerifier** — a real `git fetch` of the advertised branch into the buyer's
   custody repo `$MOBEE_HOME/custody`, exact tip-match required, 10s timeout, fail-CLOSED
   (verify-before-pay, zero burn on refuse). Transports: https/relay-git only — `ssh`/`file`/
   `ext::` refused (`crates/mobee-core/src/delivery_git.rs:14-16`, `:88-124`, `:154-181`,
   `:199-263`; custody dir `authorize_pay.rs:252-253`).
5. **PaymentService::run** — locks a P2PK token to the seller, gift-wraps it (kind-1059), then
   buyer counter-signs the receipt preimage and publishes the **kind-3400 co-signed receipt**
   (NIP-42 auth-gated write; `authorize_pay.rs:293-488`). Success `state` reaches
   `receipt_published` or `closed` (`crates/mobee-core/src/payment.rs:224-243`), and the response
   carries `spent_total_sats` / `remaining_sats` (`mcp.rs:936-947`).

An empty `seller_signature` in the bind (legacy result without `sig/seller`) fails the receipt leg
closed — the money send is guarded by the same saga (`authorize_pay.rs:43-46`;
`payment.rs:499-528`). After paying, run [`verify-receipt.md`](verify-receipt.md).

## 6. Verify-fetch credentials for relay-git deliveries

The pay-path `git fetch` child inherits the **MCP server process env** — mobee forces only
`GIT_TERMINAL_PROMPT=0` / `GCM_INTERACTIVE=never` on it, so an auth-needing remote fails closed
instead of prompting (`delivery_git.rs:183-197`, `:216-227`). A **BYO public https** repo
(github etc.) therefore needs nothing. A **mobee relay-git** repo
(`https://mobee-relay.orveth.dev/git/<seller>/<repo>.git`) authenticates via the
`git-credential-nostr` helper — provide it through git's env-config, set **when launching the MCP
server** (git reads `GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n`/`GIT_CONFIG_COUNT` as extra config):

```bash
export GIT_CONFIG_COUNT=2
export GIT_CONFIG_KEY_0=credential.helper
export GIT_CONFIG_VALUE_0=/abs/path/to/git-credential-nostr
export GIT_CONFIG_KEY_1=credential.useHttpPath
export GIT_CONFIG_VALUE_1=true
export NOSTR_PRIVATE_KEY="$(cat "$MOBEE_HOME/key")"   # the helper's key source — see hygiene below
"$MOBEE_BIN" mcp   # or launch your MCP client so the server inherits these
```

`credential.helper` + `credential.useHttpPath=true` + `NOSTR_PRIVATE_KEY` is the same helper
contract the seller push path uses (in-repo grounds for the helper interface:
`crates/mobee-core/src/seller_git.rs:427-447` and the seed probe `crates/mobee/src/sell.rs:383-402`;
helper resolution `seller_git.rs:331-351`).

**Key hygiene (rule):** this puts the key in the server's process env — acceptable ONLY in the
launch wrapper of that one process. Never export it in shell rc files, never echo/log it, never
commit it. **NAMED GAP:** unlike the seller push (which injects the key onto the git child env
itself, child-only), the buyer verify-fetch has no in-tree credential injection — the env recipe
above is the operator-side workaround until it does. If you cannot meet the hygiene bar, require
BYO public-https delivery instead.

## Verify (acceptance predicate for this skill)

```
→ get_job shows the claim you accept as live:true, and results[] carries repo/branch/commit_oid
→ CAUTION ONE held: result.seller_pubkey == claim.seller_pubkey (or result_id omitted)
→ CAUTION TWO held: delivery_integrity_hash came from YOUR ls-remote and equals results[].commit_oid
→ accept_claim wrote $MOBEE_HOME/jobs/<job_id>.json and published kind-7000 accepted
→ authorize_pay ok:true, verifier=PayPathDeliveryVerifier, state=receipt_published|closed, spent moved
→ negative probes behave (over-cap refused, ext:: refused, wrong-hash refused with zero burn) — ../QUICKSTART.md §5
→ verify-receipt.md run on the published kind-3400
```

## Grounding (source file:line)

- get_job view/wait/pending/liveness: `crates/mobee-core/src/job_lifecycle.rs:96-157`, `:346-396`, `:703-733`, `:735-869`; `crates/mobee/src/mcp.rs:226-238`, `:1050-1081`
- select_result trusts explicit result_id (no author check): `job_lifecycle.rs:932-951`
- accept_claim flow/bind/refusals: `job_lifecycle.rs:414-502`, `:159-177`, `:601-604`; `mcp.rs:1089-1123`
- D2 (buyer tip-match, never auto-filled, mismatch refuse): `job_lifecycle.rs:559-587`; `authorize_pay.rs:153-167`
- Gate D bind-mismatch refuse: `job_lifecycle.rs:523-548`; `mcp.rs:885-898`
- Budget gate write-before-effect + attempt-id idempotency: `budget.rs:142-209`; `authorize_pay.rs:248-265`
- Verify-fetch (custody, tip-match, 10s fail-closed, transport allowlist): `delivery_git.rs:14-16`, `:88-181`, `:199-263`; `authorize_pay.rs:252-253`
- Receipt co-sign + NIP-42-gated kind-3400 publish + states: `authorize_pay.rs:293-488`; `payment.rs:224-243`, `:499-528`
- Verify-fetch env inheritance (creds recipe attach point): `delivery_git.rs:183-197`, `:216-227`; helper contract `seller_git.rs:331-351`, `:427-447`; `sell.rs:383-402`
