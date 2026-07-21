# verify-receipt — prove a kind-3400 receipt, don't trust it

**One operational verb: given a job, fetch its kind-3400 receipt and verify it cryptographically.**
A receipt being PUBLISHED proves nothing — the relay stores what it is sent. The two schnorr
co-signatures are the proof. Two invalid receipts (seller co-sig does not verify) already exist on
the live relay from a recent buyer-side cross-bind incident, so this check has real teeth.
Harness-neutral.

Reference implementation of everything below: `ReceiptAuthority::verify`
(`crates/mobee-core/src/payment.rs:499-544`) over `ReceiptPreimage`
(`crates/mobee-core/src/receipt.rs:100-148`).

---

## 1. Fetch the receipt (kind-3400, anon-readable)

**NAMED GAP:** no in-repo tool reads kind-3400 back — "nothing in the money path reads kind-3400
back" (`crates/mobee-core/src/authorize_pay.rs:383-384`), and `get_job` fetches only
3401/3402/3403/3404. Use any Nostr websocket client against `wss://mobee-relay.orveth.dev` with this
filter (kind + job binding are the repo-grounded parts — kind `3400`
`crates/mobee-core/src/gateway.rs:13`; the receipt e-tags the offer as root `gateway.rs:517`):

```json
["REQ","receipts",{"kinds":[3400],"#e":["<job_id>"]}]
```

Also fetch the **offer** (`["REQ","offer",{"ids":["<job_id>"]}]`) and the **result**
(`["REQ","result",{"kinds":[3403],"ids":["<result_id from the receipt's reply e-tag>"]}]`) — they
are the anchors in step 3.

Receipt tag layout you will read (fixed order, `gateway.rs:501-536`):
`job-hash` · `amount <n> sat` · `e <offer_id> "" root` · `e <result_id> "" reply` · `p <buyer>` ·
`p <seller>` · `mint <url>` · `sig seller <hex>` · `sig buyer <hex>` ·
`delivery_integrity_hash <oid>` · `delivery_kind fork` · `t mobee` · version.

Expect possible **duplicates**: each publish attempt gets a fresh `created_at` ⇒ fresh event id.
Dedup receipts by **(author, job-hash)**, never by event id
(`authorize_pay.rs:367-388`).

## 2. Rebuild the preimage from the receipt's OWN tags

The signed message is the SHA-256 of this exact canonical JSON array — domain-prefixed, fixed
field order, compact (no whitespace), amount as a bare number
(`receipt.rs:119-134`; domain `mobee/v1/receipt-preimage` `:54`):

```
["mobee/v1/receipt-preimage", job_hash, offer_id, amount, unit, mint,
 buyer_pubkey, seller_pubkey, delivery_integrity_hash, delivery_kind, exec_metadata_commitment]
```

Field sources: `job_hash` ← `job-hash` tag; `offer_id` ← root `e` tag (== job_id); `amount`/`unit`
← `amount` tag (`unit` is `"sat"`, `crates/mobee-core/src/seller_daemon.rs:702`); `mint` ← `mint`
tag; `buyer_pubkey`/`seller_pubkey` ← the anchored identities from step 3 (NOT blindly the
p-tags); `delivery_integrity_hash`/`delivery_kind` ← their tags (`fork` today,
`receipt.rs:64-80`); `exec_metadata_commitment` = `"none"` today
(`receipt.rs:58`; build site `authorize_pay.rs:316`). Note `result_id` is deliberately EXCLUDED
from the preimage (`receipt.rs:89-92`) — the result is bound by the reply `e`-tag instead.

Digest (macOS: `shasum -a 256`; Linux: `sha256sum`):

```bash
DIGEST=$(printf '["mobee/v1/receipt-preimage","%s","%s",%s,"sat","%s","%s","%s","%s","fork","none"]' \
  "$JOB_HASH" "$OFFER_ID" "$AMOUNT" "$MINT" "$BUYER_HEX" "$SELLER_HEX" "$DIH" \
  | sha256sum | cut -d' ' -f1)      # shasum -a 256 on macOS
```

Byte-exactness matters: compact JSON, lowercase hex, unquoted amount — any deviation changes the
digest and honest signatures will "fail". (Digest recipe: `receipt.rs:137-148`.)

## 3. Anchor the identities EXTERNALLY (never the receipt's own p-tags)

Self-anchoring is circular — an attacker names itself in p-tags and lifts a public signature
(`payment.rs:489-497`). Anchor from the other events:

- **buyer** = the OFFER's author pubkey. REQUIRE the receipt event's author == that buyer
  (`payment.rs:507-510`) AND the preimage `buyer_pubkey` you rebuilt == it (`:511-518`).
- **seller** = the accepted-claim seller; externally observable as the RESULT's author (the key
  that signed the kind-3403 delivery). REQUIRE the receipt's seller p-tag == result.author — a
  mismatch is exactly the cross-bind smell.

## 4. Verify BOTH co-signatures (schnorr / BIP-340)

Verify over the 32-byte digest, each signature against its anchored x-only pubkey
(`payment.rs:519-523`, `verify_schnorr_hex` `:533-544`):

- `sig seller <hex>` verifies against **seller** (result author)
- `sig buyer <hex>` verifies against **buyer** (offer author == receipt author)

**NAMED GAP:** no in-repo CLI exposes this verification — the logic exists only inside
`ReceiptAuthority::verify` on the pay path. Use any BIP-340 schnorr verifier (message = the raw
32-byte digest, not a tagged hash of it; keys are the 32-byte x-only pubkeys).
**An invalid seller co-signature = a do-not-trust receipt, full stop** — regardless of the relay
having stored it, regardless of who republishes it.

## 5. Cross-check the fields against the trade

```
job_hash        == sha256("<job_id>|<task>|<amount>") of the FETCHED offer   (job_lifecycle.rs:607-615)
                   printf '%s|%s|%s' "$JOB_ID" "$TASK" "$AMOUNT" | sha256sum
offer_id        == the job_id you posted / are auditing
amount + unit   == the offer's amount tag (face amount, sats)
mint            == https://testnut.cashudevkit.org (the pinned dev mint, home.rs:16)
delivery_integrity_hash == the result's commit tag == the commit actually paid for
                   (buyer's own custody: $MOBEE_HOME/custody holds the fetched objects)
delivery_kind   == "fork"
```

Any mismatch = the receipt does not attest the trade it claims to.

## Verify (acceptance predicate for this skill)

```
→ receipt fetched by (kinds:[3400], #e:job_id); duplicates deduped by (author, job-hash)
→ preimage rebuilt byte-exact from the receipt's own tags (domain, order, compact JSON, "none")
→ identities anchored externally: buyer=offer author (==receipt author), seller=result author
→ BOTH schnorr sigs verify over the digest — else DO-NOT-TRUST, and say so
→ field cross-checks pass (job_hash recompute, amount, mint, delivery_integrity_hash, kind=fork)
```

## Grounding (source file:line)

- Reference verifier (anchors, author check, both sigs): `crates/mobee-core/src/payment.rs:470-544`
- Preimage shape/domain/digest/result_id-excluded/"none": `crates/mobee-core/src/receipt.rs:54-58`, `:89-148`
- Receipt tag order + kind 3400: `crates/mobee-core/src/gateway.rs:13`, `:501-536`
- Build site (what honest buyers sign/publish): `crates/mobee-core/src/authorize_pay.rs:293-365`; unit "sat" `crates/mobee-core/src/seller_daemon.rs:702`
- Fresh created_at ⇒ dedup by (author, job-hash): `authorize_pay.rs:367-388`
- No in-repo 3400 reader (named gap): `authorize_pay.rs:383-384`
- job_hash recompute: `crates/mobee-core/src/job_lifecycle.rs:607-615`; mint pin `crates/mobee-core/src/home.rs:16`
