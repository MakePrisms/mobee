# Spike lessons (rebuild constraints)

Captured 2026-07-13 from buzz replies to mobee-meta ask (`9e850c60…`).
Sources: **metadex** (`a2f8dc22…`), **keeper:mobee-orchestrator** (`a7b8ec00…`).
Awaiting: Sting, gudnuf (optional addenda).

These constrain how we rebuild onto MakePrisms/mobee `main`.
Spike is reference — do **not** re-import the scars listed under Refuse.

## MUST-fix (design from commit 1)

### Boundaries
- Almost no buyer/seller **policy** in `cli.rs`. Binary = parse args, wire runtimes, print JSON.
- Payment, idempotency journal, receipt validation, offer parse/validate, token checks → **`mobee-core`** (testable modules).
- Root cause of weak spike tests: god-function `authorize_pay` in CLI; helpers tested, full path not.

### Payment state machine
- Explicit states: `intent → token minted/locked → token delivered → receipt published → closed`.
- Not one `authorize_pay` blob with hidden side effects.

### Idempotency / journal
- Stable key: `(job_id, result_id, content_hash, job_hash, seller_pubkey, amount, mint)`.
- **Write-ahead**: durable **pre-pay intent** (flock + fsync) **before** `pay_seller`; mark delivered/receipted under same lock.
- Explicit recovery: paid-but-receipt-missing republish without second pay.
- Full-path test with stubbed pay-counter: `pay_seller` at most once across retry/crash/concurrent.

### Real funds (vs testnut demo)
- Never static/canned token on real path. Mint per trade; verify proof sum == amount; mint URL; P2PK to seller; proof state.
- `wallet` feature gate = intentional safety: no-wallet builds cannot pay.

### Receipts / relay
- Receipt authority = **author + signatures**, not public tags.
- Empty `relay_success` = failure for money-path publish/deliver.

### Targeting
- Checked invariant: `offer.seller_pubkey = Some(X)` → accept_claim + authorize_pay require seller==X.
- Untargeted: buyer chooses accepted seller, then **hard-bind** for rest of flow.
- Name the transition: open offer → accepted seller → seller-bound result/payment.

### Naming
- `job_id` = market/offer (Nostr). `execution_id` = ACP/spine run/log. Journal/receipt keys explicit about which.

### Testing (merge gates)
- Offer draft/parse goldens (targeted + untargeted).
- Targeting acceptance tests (wrong seller rejected; untargeted binds one seller).
- Hash-bind failures before pay.
- Idempotency suite (double request, pay-ok/receipt-fail, restart journal, malformed fail-closed).
- Forged receipt rejection; empty relay fail.
- Feature: no wallet ⇒ no pay path.
- Fix or drop pre-existing eval flake — do not carry forward knowingly.
- CI encodes “suite must not go backward.”

## SHOULD-FIX

- Core shape: `gateway` (types/drafts), `receipt`, `payment`, `buyer` SM, `seller` SM; binary wires Nostr SDK + FS.
- Feature split clean: `gateway` = events+relay adapter; `wallet` = Cashu; `acp` = agent exec only. Pure core tests without network/wallet/acp.
- Injectable journal trait (FS JSONL demo OK; tests need interface).
- Nix boring targets: CLI+gateway, CLI+gateway+wallet, seller+acp, test/dev — humans don’t memorize features.
- MCP buyer = thin adapter over buyer service; tool contracts tested, not hand-maintained in CLI.
- Honest sync (kill faux-async `block_on`) — serial loop matched MCP and helped avoid in-proc double-pay.
- Fund-isolation test (non-testnut → hard-fail) as standing gate.

## NICE

- Serializable payment/receipt domain structs + golden tests.
- Hermetic fakes for most tests; separate live/testnut smoke lane.

## REFUSE TO COPY from spike

- `authorize_pay` (or equivalent) god-function in `cli.rs`
- Static-token payment as the real path
- Append-after-pay journal (no pre-intent / no flock/fsync)
- Tag-only receipt trust
- `.scratch/` artifacts committed (`hello.txt`, `run.jsonl`, …)

## Implication for first rebuild PRs

- **format + receipt** as pure core modules still correct first slice — but receipt/payment work after that must land as **state library**, not CLI helpers.
- Do not “clean cherry-pick” money-path CLI shape onto main; rebuild as protocol/state + thin skin (metadex’s biggest reversal).
- gudnuf money-path cherry-pick may still land spike code for demo continuity — treat as transitional; rebuild plan remains the end-state architecture.
