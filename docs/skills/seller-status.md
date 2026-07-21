# seller-status — is the seller alive, and what has it done?

**One operational verb: report the current health and trade history of a seller from ground
truth** (process + logs + journal + wallet). Read-only. Harness-neutral.

Assumes `MOBEE_BIN` and `MOBEE_HOME` are set the same way [`run-seller.md`](run-seller.md) set them
(default home is `~/.mobee`).

---

## 1. Daemon liveness (process + log heartbeat)

```bash
# (a) Is a sell daemon process running?
pgrep -af "mobee sell" || echo "no mobee sell process"

# (b) Did the last startup reach the LIVE gate? (needs the tee'd log from run-seller.md step 4)
grep -q "seller daemon online" "$MOBEE_HOME/sell.log" 2>/dev/null && \
grep    "nip42="               "$MOBEE_HOME/sell.log" | tail -1

# (c) Heartbeat: most recent daemon activity line (offers/skips/deliveries/receipts)
tail -n 20 "$MOBEE_HOME/sell.log" 2>/dev/null
```

Interpretation:

- Process present **and** `nip42=authenticated` on the last "seller daemon online" → live and able
  to receive. `nip42=no-challenge` → degraded auth; receive may be impaired (see
  [`seller-diagnose.md`](seller-diagnose.md)).
- Process absent → not running; relaunch with [`run-seller.md`](run-seller.md) / bounce with
  [`seller-update.md`](seller-update.md).

Grounds: online+auth line `crates/mobee-core/src/seller_daemon.rs:1424-1429`.

---

## 2. Wallet balance (what you have actually collected)

```bash
"$MOBEE_BIN" wallet balance --home "$MOBEE_HOME"
```

Prints one `mint=… role=default|extra balance_sats=…` line per configured mint, then
`total_sats=…` (`crates/mobee/src/wallet_cli.rs:191-205`). The default mint is testnut. Remember:
receipts record the FACE amount, but the wallet holds `face − mint fee` — this balance is the real
number. See [`wallet-ops.md`](wallet-ops.md).

---

## 3. Trades: claims / deliveries / payments from the journal

The durable seller journal is an append-only JSONL file at **`$MOBEE_HOME/seller-journal.jsonl`**
(grounds: `crates/mobee-core/src/seller.rs:19`, `:230-240`). Each line is one entry tagged by
`kind` (grounds: `seller.rs:121-156`):

| `kind` | Meaning | Key fields |
|--------|---------|-----------|
| `claim` | An offer was claimed (kind-3402 processing published) | `job_id`, `claim_id`, `buyer_pubkey`, `deadline_unix`, `ts` |
| `receipt` | Payment redeemed — trade CLOSED (paid) | `job_id`, `result_id`, `amount_received`, `mint`, `buyer`, `swap_ok`, `ts` |
| `release` | Claim given up (orphan reneged on restart / undeliverable) | `job_id`, `reason`, `ts` |

The journal **never** holds token or key material — ids, amounts, mint, buyer, `swap_ok`, `ts` only
(`seller.rs:367`, test `:609-615`).

Read it (works with or without `jq`):

```bash
J="$MOBEE_HOME/seller-journal.jsonl"

# Counts by kind:
echo "claims:   $(grep -c '"kind":"claim"'   "$J" 2>/dev/null || echo 0)"
echo "receipts: $(grep -c '"kind":"receipt"' "$J" 2>/dev/null || echo 0)"
echo "releases: $(grep -c '"kind":"release"' "$J" 2>/dev/null || echo 0)"

# Total FACE sats receipted (face amounts; wallet net is face − fee per receipt):
if command -v jq >/dev/null; then
  jq -rs '[.[] | select(.kind=="receipt") | .amount_received] | add // 0' "$J"
fi

# Recent activity, newest last:
tail -n 10 "$J" 2>/dev/null
```

---

## 4. Pending: delivered-but-unpaid jobs

A **delivered-but-unpaid** job is one the daemon delivered (kind-3403 published) but for which
payment has not yet been redeemed. In the journal this is a `claim` line whose `job_id` has **no**
`receipt` and **no** `release`:

```bash
if command -v jq >/dev/null; then
  jq -rs '
    (reduce .[] as $e ({}; .[$e.job_id] += [$e.kind])) as $byjob
    | $byjob | to_entries
    | map(select((.value|index("claim")) and (.value|index("receipt")|not) and (.value|index("release")|not)))
    | .[].key
  ' "$MOBEE_HOME/seller-journal.jsonl"
fi
# Each printed job_id = claimed, not yet paid, not released → either in-flight/processing now,
# delivered-and-awaiting-payment, or an orphan not yet reconciled.
```

> **Caveat (grounded limitation):** the delivered-but-unpaid → payment binding lives only in the
> daemon's **in-memory** `awaiting_payment` list; it is NOT journaled. So the journal alone cannot
> distinguish "delivered, waiting for pay" from "still processing" from "orphaned". Cross-check the
> live daemon log (`seller published 3403 result_id=…` means delivered). If the daemon restarted
> after delivering but before the payment redeemed, that binding is lost and the pending job will
> be RELEASED on the next startup — see [`seller-diagnose.md`](seller-diagnose.md) "payment arrived
> but not redeemed after restart". Grounds: in-memory binding
> `crates/mobee-core/src/seller_daemon.rs:240-253`; forfeiture note
> [`../meta/PIECE-11-CLAIM-LIFECYCLE.md`](../meta/PIECE-11-CLAIM-LIFECYCLE.md) "Known limitations".

Per-job execution logs (agent transcript for a claimed job) live at
`$MOBEE_HOME/seller-jobs/<job_id>/seller-run.jsonl` (`seller_daemon.rs:1016`).

---

## Verify (acceptance predicate for this skill)

```
→ reports whether a `mobee sell` process is running (pgrep)
→ reports last startup auth state (nip42=authenticated | no-challenge | absent)
→ prints wallet total_sats from `mobee wallet balance`
→ counts claim/receipt/release lines from $MOBEE_HOME/seller-journal.jsonl
→ lists job_ids that are claimed-but-not-receipted-and-not-released (pending)
→ never prints key or token material (journal has none by construction)
```

## Grounding (source file:line)

- Liveness/online+auth line: `crates/mobee-core/src/seller_daemon.rs:1424-1429`
- Wallet balance CLI + output: `crates/mobee/src/wallet_cli.rs:24-38`, `:146-207`
- Journal path/format/entries/no-secrets: `crates/mobee-core/src/seller.rs:19`, `:121-156`, `:230-240`, `:367`
- In-memory delivered-unpaid binding + forfeiture: `seller_daemon.rs:240-253`; `../meta/PIECE-11-CLAIM-LIFECYCLE.md` Known limitations
- Per-job workdir + run log: `seller_daemon.rs:888-890`, `:1016`
