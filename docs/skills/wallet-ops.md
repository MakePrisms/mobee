# wallet-ops — balance, redeem, reconcile

**One operational verb: inspect and manage the seller's ecash wallet.** Harness-neutral.

Assumes `MOBEE_BIN` / `MOBEE_HOME` set as in [`run-seller.md`](run-seller.md).

> **Testnut only. No real funds.** The default mint is the canonical dev testnut mint
> **`https://testnut.cashudevkit.org`** (grounds: `DEFAULT_MINT_URL`,
> `crates/mobee-core/src/home.rs:16`). A dead `testnut.cashu.space` in an old config is
> auto-migrated to this host on bootstrap (`home.rs:262-270`). The seller daemon refuses to run
> against any non-testnut mint (`crates/mobee-core/src/seller_daemon.rs:258-263`). Never print,
> log, or commit the key or any token you handle.

The `mobee wallet` subcommand surface (grounds: `crates/mobee/src/wallet_cli.rs:24-56`):

```
mobee wallet balance [--mint <url>] [--home <path>]
mobee wallet mint    <amount> [--mint <url>] [--home <path>]
mobee wallet send    <amount> [--mint <url>] [--home <path>]
mobee wallet receive <token>  [--home <path>]
mobee wallet melt    <bolt11> [--mint <url>] [--home <path>]
mobee wallet invoice <amount> [--mint <url>] [--home <path>]
mobee wallet mints   list|add <url>|remove <url> [--home <path>]
```

---

## Balance (the real number)

```bash
"$MOBEE_BIN" wallet balance --home "$MOBEE_HOME"
```

Output: one `mint=… role=default|extra balance_sats=…` line per configured mint, then
`total_sats=…` (`wallet_cli.rs:191-205`). Filter to one mint with `--mint <url>`.

> The seller **receipt/journal records the FACE (offer) amount**, but the mint charges an input fee
> on redeem, so your wallet holds **`face − mint fee`**. This balance is the truth; a receipt's
> `amount_received` is the accounting face, not sats pocketed. Grounds:
> [`../SELLER-QUICKSTART.md`](../SELLER-QUICKSTART.md) §7.

---

## Redeem a raw token (manual receive)

If you hold a raw cashu token string (e.g. handed one out-of-band, or one you produced with
`wallet send`), redeem it into the wallet:

```bash
"$MOBEE_BIN" wallet receive "<cashu-token>" --home "$MOBEE_HOME"
# → received_sats=… balance_sats=… mint=…
```

Grounds: `wallet_cli.rs:307-342`. Normal seller collection is automatic (the daemon unwraps the
buyer's gift-wrap and redeems fee-aware); `wallet receive` is for a raw token you already hold.

---

## "Stuck-not-lost" — an unredeemed gift-wrap on the relay

If a payment arrived but was not auto-redeemed (e.g. the daemon restarted in the deliver→pay
window — see [`seller-diagnose.md`](seller-diagnose.md) §I), the buyer's payment is a **NIP-17
gift-wrap (kind-1059) addressed to your seller pubkey that stays on the relay**. The sats are **not
lost** — they are just not swapped into your wallet yet. This forfeits revenue only, never safety
(no receipt released, no double-pay). Grounds:
[`../meta/PIECE-11-CLAIM-LIFECYCLE.md`](../meta/PIECE-11-CLAIM-LIFECYCLE.md) "Known limitations";
in-memory binding `seller_daemon.rs:240-253`.

**Reconcile today:**
- The daemon auto-reconciles buffered wraps against a delivered job **within the same process run**
  (`seller_daemon.rs:1106-1121`) — so if the wrap simply arrived early, it lands once delivery is
  recorded, no action needed.
- Across a **restart**, the binding is gone. **NAMED GAP:** there is no in-repo command to unwrap a
  stuck gift-wrap on the relay back into a redeemable token. The real fix — journaling the
  delivered-unpaid binding so payment survives a restart — is a named follow-up (PIECE-11), not yet
  built. Mitigation: avoid restarting in the deliver→pay window.

---

## Other subcommands (rarely needed for a seller)

- `wallet mint <amount>` — fund a wallet (testnut auto-pays the quote); sellers do **not** need
  this to sell (`wallet_cli.rs:209-260`).
- `wallet send <amount>` — mint a token string to hand out (prints the token on stdout,
  `wallet_cli.rs:262-305`). Treat the token like cash; do not log it.
- `wallet invoice <amount>` / `wallet melt <bolt11>` — Lightning bridge (needs external pay for
  non-testnut mints); not part of the seller path.
- `wallet mints list|add|remove` — manage extra mints; the default testnut mint stays pinned
  (`wallet_cli.rs:434-515`).

---

## Verify (acceptance predicate for this skill)

```
→ `mobee wallet balance --home $MOBEE_HOME` prints per-mint balance_sats + total_sats
→ default mint is https://testnut.cashudevkit.org (testnut pinned; seller refuses non-testnut)
→ explains wallet net = face − mint fee (balance is truth, receipt face is not sats pocketed)
→ `mobee wallet receive <token>` redeems a raw token
→ "stuck-not-lost" defined (gift-wrap on relay, sats safe); manual gift-wrap unwrap is a NAMED GAP
→ never prints key or token material to a durable log
```

## Grounding (source file:line)

- Wallet CLI surface + outputs: `crates/mobee/src/wallet_cli.rs:24-56`, balance `:146-207`, receive `:307-342`, send `:262-305`, mint `:209-260`, mints `:434-515`
- Testnut mint pin + migration + seller refuse: `crates/mobee-core/src/home.rs:16`, `:262-270`; `crates/mobee-core/src/seller_daemon.rs:258-263`
- Fee-aware net (face − fee): `../SELLER-QUICKSTART.md` §7; redeem `seller_daemon.rs:515-591`
- Stuck-not-lost / in-process reconcile / restart gap: `seller_daemon.rs:240-253`, `:1106-1121`; `../meta/PIECE-11-CLAIM-LIFECYCLE.md`
