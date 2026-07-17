//! MCP / CLI pay entry: BudgetGate → piece-6 [`PaymentService::run`] only.
//!
//! By construction the delivery verifier is [`PayPathDeliveryVerifier`] (allowlist sealed).
//! Stable [`PaymentKey::attempt_id`] feeds `run()`'s reconcile saga — no bespoke pay path,
//! no [`PaymentService::advance`] from this surface.

use std::fmt;
use std::str::FromStr;

use cashu::{Amount, CurrencyUnit, MintUrl, PublicKey as CashuPublicKey};
use nostr_sdk::secp256k1::{Message, Secp256k1};
use nostr_sdk::Keys;
use nostr_sdk::PublicKey as NostrPublicKey;
use nostr_sdk::Timestamp;

use crate::budget::{BudgetGate, BudgetRefuse};
use crate::buyer_fund::{self, FundError};
use crate::delivery::{CommitOid, DeliveryError, GitDelivery};
use crate::delivery_git::PayPathDeliveryVerifier;
use crate::gateway;
use crate::home::{self, MobeeHome, DEFAULT_MINT_URL};
use crate::payment::{
    DeliveryIntegrityHash, EffectError, FsPaymentJournal, JobHash, JobId, PaymentError, PaymentKey,
    PaymentService, PaymentState, PaymentTerms, ReceiptAuthority, ReceiptEvidence, ResultId,
};
use crate::payment_send::NostrPaymentSend;
use crate::payment_wallet::{CdkPaymentEffects, PaymentWalletError};
use crate::receipt::{DeliveryKind, ReceiptPreimage, EXEC_METADATA_COMMITMENT_EMPTY};

/// Inputs for the authorize_pay composed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizePayRequest {
    pub job_id: String,
    pub result_id: String,
    /// Buyer's independent commitment (full git oid) — must tip-match after verify.
    pub delivery_integrity_hash: String,
    pub job_hash: String,
    pub seller_pubkey: String,
    pub amount_sats: u64,
    pub repo: String,
    pub branch: String,
    pub commit_oid: String,
    /// Seller schnorr signature (hex) over the piece-9 receipt preimage — read from the
    /// accepted result's `sig/seller` tag. Empty ⇒ the buyer cannot co-sign a valid
    /// receipt (the receipt authority fails closed at publish).
    pub seller_signature: String,
}

/// Successful composed pay outcome (state + attempt id + spent accounting).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizePayOutcome {
    pub state: PaymentState,
    pub attempt_id: String,
    pub amount_sats: u64,
    pub spent_total_sats: u64,
    pub remaining_sats: u64,
}

#[derive(Debug)]
pub enum AuthorizePayError {
    Input(String),
    Budget(BudgetRefuse),
    Fund(FundError),
    Delivery(DeliveryError),
    Payment(PaymentError),
    Wallet(PaymentWalletError),
    Home(String),
    Effects(String),
}

impl fmt::Display for AuthorizePayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => write!(formatter, "authorize_pay input: {message}"),
            Self::Budget(refuse) => write!(formatter, "{refuse}"),
            Self::Fund(error) => write!(formatter, "{error}"),
            Self::Delivery(error) => write!(formatter, "authorize_pay delivery: {error}"),
            Self::Payment(error) => write!(formatter, "authorize_pay payment: {error}"),
            Self::Wallet(error) => write!(formatter, "authorize_pay wallet: {error}"),
            Self::Home(message) => write!(formatter, "authorize_pay home: {message}"),
            Self::Effects(message) => write!(formatter, "authorize_pay effects: {message}"),
        }
    }
}

impl std::error::Error for AuthorizePayError {}

impl From<BudgetRefuse> for AuthorizePayError {
    fn from(value: BudgetRefuse) -> Self {
        Self::Budget(value)
    }
}

impl From<FundError> for AuthorizePayError {
    fn from(value: FundError) -> Self {
        Self::Fund(value)
    }
}

impl From<DeliveryError> for AuthorizePayError {
    fn from(value: DeliveryError) -> Self {
        Self::Delivery(value)
    }
}

impl From<PaymentError> for AuthorizePayError {
    fn from(value: PaymentError) -> Self {
        Self::Payment(value)
    }
}

impl From<PaymentWalletError> for AuthorizePayError {
    fn from(value: PaymentWalletError) -> Self {
        Self::Wallet(value)
    }
}

/// Authorize spend under [`BudgetGate`], then pay only through
/// [`PaymentService::run`] with a [`PayPathDeliveryVerifier`].
///
/// Spent is keyed by stable `PaymentKey::attempt_id()`: first authorize persists
/// spent **before** `run()` (write-before-mint); a reconciled retry does not
/// re-count. `run()` delivery-verifies first and reconciles inside the piece-6 saga.
pub fn authorize_pay(
    home: &MobeeHome,
    gate: &mut BudgetGate,
    request: AuthorizePayRequest,
) -> Result<AuthorizePayOutcome, AuthorizePayError> {
    crate::runtime_guard::refuse_nested_block_on("authorize_pay")
        .map_err(AuthorizePayError::Effects)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AuthorizePayError::Effects(error.to_string()))?;
    runtime.block_on(authorize_pay_async(home, gate, request))
}

/// Async authorize_pay for callers already on a Tokio runtime (MCP dispatch).
///
/// LOGIC identical to the sync path — only wallet open is `await` (no nested
/// `block_on`). Verify-fetch timeout still fails CLOSED (no pay / zero burn).
pub async fn authorize_pay_async(
    home: &MobeeHome,
    gate: &mut BudgetGate,
    request: AuthorizePayRequest,
) -> Result<AuthorizePayOutcome, AuthorizePayError> {
    if home.config.mint_url != DEFAULT_MINT_URL {
        return Err(FundError::MintPinned {
            configured: home.config.mint_url.clone(),
        }
        .into());
    }

    // D2 (both job_id and explicit forms): buyer tip-match hash is required and must
    // equal the seller-advertised commit_oid. Never derive/default the hash from the
    // claim/result oid — caller must supply it; mismatch refuses.
    if request.delivery_integrity_hash.trim().is_empty() {
        return Err(AuthorizePayError::Input(
            "delivery_integrity_hash is required (buyer tip-match); never auto-filled from claim/result oid".into(),
        ));
    }
    if request.delivery_integrity_hash != request.commit_oid {
        return Err(AuthorizePayError::Input(format!(
            "delivery_integrity_hash {} does not match seller-advertised commit_oid {} (buyer tip-match required; refuse mismatch)",
            request.delivery_integrity_hash, request.commit_oid
        )));
    }

    let job_id = JobId::new(request.job_id.clone())
        .map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let result_id = ResultId::new(request.result_id.clone())
        .map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let delivery_integrity_hash = DeliveryIntegrityHash::from_hex(request.delivery_integrity_hash)
        .map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let job_hash = JobHash::from_hex(request.job_hash)
        .map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let seller_nostr = NostrPublicKey::parse(&request.seller_pubkey)
        .map_err(|error| AuthorizePayError::Input(format!("seller_pubkey: {error}")))?;
    let seller_p2pk = cashu_compressed_from_nostr(&seller_nostr)?;
    let mint = MintUrl::from_str(DEFAULT_MINT_URL)
        .map_err(|error| AuthorizePayError::Input(format!("mint url: {error}")))?;
    let terms = PaymentTerms::new(
        mint,
        Amount::from(request.amount_sats),
        CurrencyUnit::Sat,
        seller_nostr,
        seller_p2pk,
    );
    let key = PaymentKey::new(
        job_id,
        result_id,
        delivery_integrity_hash,
        job_hash,
        &terms,
    );
    let attempt_id = key.attempt_id();

    let commit_oid = CommitOid::parse(request.commit_oid)?;
    let delivery = GitDelivery::new(request.repo, request.branch, commit_oid)?;

    let secret_hex = home::read_secret_key_hex(home)
        .map_err(|error| AuthorizePayError::Home(error.to_string()))?;
    let keys = Keys::parse(&secret_hex)
        .map_err(|error| AuthorizePayError::Home(format!("buyer key parse: {error}")))?;
    let buyer_nostr = keys.public_key();
    let authority = ReceiptAuthority {
        // External anchors (piece-9 Item 1): buyer == the offer's author (this buyer's own
        // key), seller == the accepted-claim seller. NEVER the receipt's own p-tags.
        buyer: buyer_nostr,
        seller: seller_nostr,
    };
    // Capture receipt-publish inputs before `keys` is moved into the payment sender.
    let buyer_receipt_keys = keys.clone();
    let receipt_relay = home.config.relay_url.clone();
    let seller_hex = seller_nostr.to_hex();
    let seller_signature = request.seller_signature.clone();

    let wallet = buyer_fund::open_testnut_wallet_async(home).await?;
    // Dust guard (live keyset N=1 floor, fail-closed). lock_or_reconcile re-checks
    // against CDK input-count send_fee after prepare_send.
    crate::payment_wallet::require_fee_safe_amount(&wallet, terms.amount)
        .await
        .map_err(AuthorizePayError::Wallet)?;
    let payment_send = NostrPaymentSend::new(home.config.relay_url.clone(), keys);
    let mut effects = CdkPaymentEffects::spawn(
        wallet,
        payment_send,
        move |key: &PaymentKey, _payment: &crate::payment_send::PaymentSent| {
            build_and_publish_receipt(
                &buyer_receipt_keys,
                &receipt_relay,
                &seller_hex,
                &seller_signature,
                key,
            )
        },
    )
    .map_err(|error| AuthorizePayError::Effects(error.to_string()))?;

    let journal_dir = home.root.join("payment-journal");
    std::fs::create_dir_all(&journal_dir)
        .map_err(|error| AuthorizePayError::Home(format!("payment journal dir: {error}")))?;
    let journal = FsPaymentJournal::new(journal_dir.join(format!("{}.jsonl", attempt_id.as_str())));
    let custody = home.root.join("custody");
    let mut verifier = PayPathDeliveryVerifier::new(custody);

    let amount = request.amount_sats;
    let state = gate.authorize_then_attempt(attempt_id.as_str(), amount, || {
        PaymentService::new(&journal).run(
            &delivery,
            &mut verifier,
            &key,
            &terms,
            &authority,
            &mut effects,
        )
    })??;

    Ok(AuthorizePayOutcome {
        state,
        attempt_id: attempt_id.as_str().to_owned(),
        amount_sats: amount,
        spent_total_sats: gate.spent(),
        remaining_sats: gate.remaining(),
    })
}

fn cashu_compressed_from_nostr(key: &NostrPublicKey) -> Result<CashuPublicKey, AuthorizePayError> {
    CashuPublicKey::from_str(&format!("02{}", key.to_hex())).map_err(|error| {
        AuthorizePayError::Input(format!("cashu pubkey from nostr key: {error}"))
    })
}

/// Piece-9 Item 1: build + publish the buyer-authored kind-3400 receipt for a sent
/// payment, and return the co-signature evidence the [`ReceiptAuthority`] verifies.
///
/// The buyer reconstructs the SAME receipt preimage the seller signed at delivery (binds
/// the trade + the delivered git object, D4; `exec_metadata_commitment` = empty marker —
/// exec-metadata is seller-claimed, not co-signed), counter-signs it deterministically,
/// builds the kind-3400 with a FRESH wall-clock `created_at`, and publishes it. `receipt_id`
/// is that 3400 event id — NOT the kind-1059 payment envelope — and is now NON-deterministic
/// per publish attempt (see [`receipt_created_at`]). Empty `relay_success` is enforced
/// fail-closed by [`ReceiptAuthority::verify`]; piece-6 recovery re-runs this publish (a fresh
/// id each attempt — verify-irrelevant, never a re-sent payment).
fn build_and_publish_receipt(
    buyer_keys: &Keys,
    relay_url: &str,
    seller_hex: &str,
    seller_signature: &str,
    key: &PaymentKey,
) -> Result<ReceiptEvidence, EffectError> {
    let buyer_hex = buyer_keys.public_key().to_hex();
    let mint = key.mint.to_string();
    let amount = key.amount.to_u64();
    // offer_id == job_id in this codebase (the offer event id is the job id).
    let preimage = ReceiptPreimage {
        job_hash: key.job_hash.as_str().to_owned(),
        offer_id: key.job_id.as_str().to_owned(),
        amount,
        unit: key.unit.to_string(),
        mint: mint.clone(),
        buyer_pubkey: buyer_hex.clone(),
        seller_pubkey: seller_hex.to_owned(),
        delivery_integrity_hash: key.delivery_integrity_hash.as_str().to_owned(),
        delivery_kind: DeliveryKind::Fork.as_str().to_owned(),
        exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
    };
    let digest = preimage.digest_bytes();
    // Buyer counter-signature (no aux-rand): a `sig/buyer` tag that is a pure function of the
    // preimage. This makes only the co-SIGNATURE deterministic — NOT the event id, which also
    // hashes the (now fresh) `created_at` and so differs per publish (see `receipt_created_at`).
    let secp = Secp256k1::new();
    let keypair = buyer_keys.secret_key().keypair(&secp);
    let buyer_signature = secp
        .sign_schnorr_no_aux_rand(&Message::from_digest(digest), &keypair)
        .to_string();

    let draft = gateway::receipt_draft(
        key.job_id.as_str(),
        key.result_id.as_str(),
        &buyer_hex,
        seller_hex,
        &mint,
        amount,
        key.job_hash.as_str(),
        seller_signature,
        &buyer_signature,
        Some(gateway::ReceiptDelivery {
            integrity_hash: key.delivery_integrity_hash.as_str(),
            kind: DeliveryKind::Fork.as_str(),
        }),
        // No exec-metadata echo in this arc: the commitment is the empty marker, so echoing
        // seller-claimed tags here would be cosmetic-only (a named follow-up).
        &[],
    );
    let builder = gateway::nostr::event_builder(&draft)
        .map_err(|error| EffectError::new(format!("receipt event builder: {error}")))?;
    let event = builder
        .custom_created_at(receipt_created_at(&digest))
        .sign_with_keys(buyer_keys)
        .map_err(|error| EffectError::new(format!("receipt sign: {error}")))?;
    // Non-deterministic per publish attempt now (fresh `created_at`); `receipt_id` records
    // whichever id the accepted publish produced — verify-irrelevant metadata.
    let receipt_id = event.id.to_hex();
    let relay_success = publish_receipt_event(relay_url, buyer_keys, &event)?;

    Ok(ReceiptEvidence {
        receipt_id,
        author: buyer_keys.public_key(),
        preimage,
        seller_signature: seller_signature.to_owned(),
        buyer_signature,
        relay_success,
    })
}

/// FRESH wall-clock `created_at` for each kind-3400 receipt publish attempt.
///
/// SUPERSEDES the former `deterministic_created_at`, which derived `created_at` from the
/// preimage digest (windowed into 2023-11 .. ~2027) so a recovery republish reproduced the
/// SAME event id — relay-native idempotency (a relay stores an event once, by id). But that
/// digest-derived timestamp almost never fell inside a real relay's accept window (mobee-relay
/// ≈ ±30 min of server time), so the receipt was rejected and the payment held at `Sent`
/// forever. A fresh wall-clock timestamp satisfies the relay window, so the receipt publishes.
///
/// DELIBERATE TRADE-OFF (a deterministic id and a fresh timestamp are mutually exclusive — the
/// event id hashes `created_at`): the receipt event id is now NON-deterministic per attempt.
/// Money-safe: [`ReceiptAuthority::verify`] never uses the id (it gates on relay acceptance +
/// author + preimage + both schnorr co-signatures), and re-publishing a receipt never re-sends
/// money (the send is durable at `Sent`; the reducer re-runs only the receipt leg). In the
/// normal path the first attempt publishes and the state advances `Sent`→`ReceiptPublished`,
/// so there is no second attempt. A duplicate (inert) kind-3400 is possible ONLY if the process
/// crashes AFTER the relay accepts but BEFORE the WAL records `ReceiptPublished`; nothing in the
/// money path reads kind-3400 back, so it is harmless today.
///
/// FOLLOW-UP (named, NOT built here — branch scribe/receipt-created-at-fix): if/when a Rust
/// receipts-reader is added it MUST dedup on read by (author, job-hash), NOT by event id, to
/// collapse such a duplicate — this replaces the removed relay-native id-dedup.
///
/// `_digest` is accepted only for call-site parity with the superseded digest-derived form and
/// is intentionally unused: the timestamp must track wall-clock, never the preimage.
fn receipt_created_at(_digest: &[u8; 32]) -> Timestamp {
    Timestamp::now()
}

/// Publish the signed kind-3400 to the relay and return the accepted relay set.
///
/// Runs on a fresh OS thread with its own current-thread runtime: publishing is async and
/// the caller may already hold a Tokio runtime (a nested `block_on` would panic).
///
/// mobee-relay requires NIP-42 AUTH for ALL writes, so this path completes + WAITS FOR the
/// auth handshake before `send_event_to` (mirroring the seller's `wait_for_nip42_auth`); the
/// payment WRAP path already authenticates, only this receipt path did not. On auth
/// timeout/failure the send is NOT reached and an empty `relay_success` is returned (never a
/// forced success) ⇒ [`ReceiptAuthority::verify`] fails closed, the payment reducer holds at
/// `Sent`, and the receipt republishes on recovery (a FRESH id per attempt — see
/// [`receipt_created_at`] — verify-irrelevant and never a re-sent payment).
fn publish_receipt_event(
    relay_url: &str,
    keys: &Keys,
    event: &nostr_sdk::Event,
) -> Result<Vec<String>, EffectError> {
    use nostr_sdk::prelude::{Client, RelayUrl};
    use std::time::Duration;
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| EffectError::new(format!("receipt runtime: {error}")))?;
                runtime.block_on(async {
                    let client = Client::new(keys.clone());
                    // Enable the auto-AUTH responder explicitly (default true; set to mirror
                    // the seller and guard against option drift) so the client answers the
                    // relay's NIP-42 challenge — otherwise the write is rejected auth-required.
                    client.automatic_authentication(true);
                    client.add_relay(relay_url).await.map_err(|error| {
                        EffectError::new(format!("receipt add relay: {error}"))
                    })?;
                    // Subscribe to the relay's notification stream BEFORE connect —
                    // `Authenticated` is emitted once and is not re-emitted (relay quirk; see
                    // the reference `seller_daemon::wait_for_nip42_auth`).
                    let parsed_relay = RelayUrl::parse(relay_url).map_err(|error| {
                        EffectError::new(format!("receipt parse relay url: {error}"))
                    })?;
                    let relay = client
                        .relays()
                        .await
                        .get(&parsed_relay)
                        .cloned()
                        .ok_or_else(|| {
                            EffectError::new("receipt relay missing after add_relay")
                        })?;
                    let mut relay_notifications = relay.notifications();
                    client.connect().await;
                    client.wait_for_connection(Duration::from_secs(20)).await;
                    // Auth gate: the receipt write MUST NOT be sent until the relay confirms
                    // NIP-42 AUTH. On timeout/failure we fail CLOSED with an empty relay set
                    // (send not reached, never a forced success) — the designed-safe
                    // direction (no double-pay; payment holds at `Sent` and retries).
                    let relay_success = if wait_for_nip42_auth(
                        &mut relay_notifications,
                        Duration::from_secs(20),
                    )
                    .await
                    {
                        let output = client.send_event_to([relay_url], event).await;
                        client.disconnect().await;
                        let output = output
                            .map_err(|error| EffectError::new(format!("receipt send: {error}")))?;
                        // Diagnostic (NOT money-semantics): surface the relay's per-relay
                        // rejection reason (e.g. "invalid: event timestamp too far from server
                        // time") — previously discarded. Relay URL + reason only; no key
                        // material. This is the string that named the created_at + auth bugs.
                        if !output.failed.is_empty() {
                            let reasons: Vec<String> = output
                                .failed
                                .iter()
                                .map(|(url, reason)| format!("{url}: {reason}"))
                                .collect();
                            eprintln!(
                                "receipt publish: relay rejected kind-3400 ({})",
                                reasons.join("; ")
                            );
                        }
                        output.success.iter().map(|url| url.to_string()).collect()
                    } else {
                        client.disconnect().await;
                        Vec::new()
                    };
                    Ok::<Vec<String>, EffectError>(relay_success)
                })
            })
            .join()
            .map_err(|_| EffectError::new("receipt publisher thread panicked"))?
    })
}

// interim: dedup with seller_daemon::wait_for_nip42_auth (follow-up: unify after builder-2
// lands). Inlined here to keep this money-core auth fix confined to authorize_pay.rs while a
// concurrent worker owns seller_daemon.rs.
//
/// Drain the relay's notification stream until NIP-42 AUTH resolves. Returns `true` ONLY on
/// [`RelayNotification::Authenticated`]; every other terminal (`AuthenticationFailed` /
/// `Shutdown` / channel closed / lagged / timeout) returns `false`, so the caller fails
/// CLOSED and never reaches the send. The caller MUST obtain `notifications` BEFORE `connect`
/// — `Authenticated` is not re-emitted.
async fn wait_for_nip42_auth(
    notifications: &mut tokio::sync::broadcast::Receiver<nostr_sdk::pool::RelayNotification>,
    timeout: std::time::Duration,
) -> bool {
    use nostr_sdk::pool::RelayNotification;

    tokio::time::timeout(timeout, async {
        loop {
            match notifications.recv().await {
                Ok(RelayNotification::Authenticated) => return true,
                Ok(RelayNotification::AuthenticationFailed) | Ok(RelayNotification::Shutdown) => {
                    return false;
                }
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetGate;
    use crate::home;

    #[tokio::test(flavor = "current_thread")]
    async fn authorize_pay_sync_refuses_inside_runtime() {
        let root = std::env::temp_dir().join(format!(
            "mobee-authorize-pay-nested-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let request = AuthorizePayRequest {
            job_id: "job-nested".into(),
            result_id: "result-nested".into(),
            delivery_integrity_hash: "aa".repeat(20),
            job_hash: "bb".repeat(32),
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 1,
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            commit_oid: "aa".repeat(20),
            seller_signature: String::new(),
        };
        let err = authorize_pay(&home, &mut gate, request).expect_err("must refuse nested block_on");
        let message = err.to_string();
        assert!(
            message.contains("nested block_on refused"),
            "unexpected error: {message}"
        );
        assert_eq!(gate.spent(), 0, "nested refuse must not burn spent");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn authorize_pay_refuses_empty_buyer_hash_without_burn() {
        let root = std::env::temp_dir().join(format!(
            "mobee-authorize-pay-d2-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let request = AuthorizePayRequest {
            job_id: "job-d2-empty".into(),
            result_id: "result-d2".into(),
            delivery_integrity_hash: String::new(),
            job_hash: "bb".repeat(32),
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 1,
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            // Even if commit_oid is set, empty buyer hash must refuse (no auto-fill).
            commit_oid: "aa".repeat(20),
            seller_signature: String::new(),
        };
        let err = authorize_pay(&home, &mut gate, request).expect_err("D2 empty");
        let message = err.to_string();
        assert!(
            message.contains("delivery_integrity_hash is required"),
            "unexpected error: {message}"
        );
        assert_eq!(gate.spent(), 0, "empty-hash refuse must not burn spent");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn authorize_pay_refuses_buyer_hash_mismatch_vs_advertised_commit() {
        let root = std::env::temp_dir().join(format!(
            "mobee-authorize-pay-d2-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let request = AuthorizePayRequest {
            job_id: "job-d2".into(),
            result_id: "result-d2".into(),
            delivery_integrity_hash: "aa".repeat(20),
            job_hash: "bb".repeat(32),
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 1,
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            commit_oid: "cc".repeat(20),
            seller_signature: String::new(),
        };
        let err = authorize_pay(&home, &mut gate, request).expect_err("D2 mismatch");
        let message = err.to_string();
        assert!(
            message.contains("does not match seller-advertised commit_oid"),
            "unexpected error: {message}"
        );
        assert_eq!(gate.spent(), 0, "D2 refuse must not burn spent");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn authorize_pay_refuses_ext_locator_via_pay_path_verifier() {
        let root = std::env::temp_dir().join(format!(
            "mobee-authorize-pay-ext-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        // Fund path not needed: budget check runs first, then run() refuses ext before fetch.
        // Pre-seed spent path via from_home; caps from config.
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        // Fee-safe tiny amount (testnut fee=1 → need ≥2) within default caps.
        // Dust guard runs before delivery verify; amount=1 would false-pass this test.
        let request = AuthorizePayRequest {
            job_id: "job-ext".into(),
            result_id: "result-ext".into(),
            delivery_integrity_hash: "aa".repeat(20),
            job_hash: "bb".repeat(32),
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 2,
            repo: "ext::sh -c evil".into(),
            branch: "main".into(),
            commit_oid: "aa".repeat(20),
            seller_signature: String::new(),
        };
        let err = authorize_pay(&home, &mut gate, request.clone()).expect_err("ext refused");
        let message = err.to_string();
        assert!(
            message.contains("ext") || message.contains("refused") || message.contains("transport"),
            "unexpected error: {message}"
        );
        // Write-before-effect: spent was committed before run() refused.
        assert_eq!(gate.spent(), 2);

        // Reconciled retry of the same PaymentKey attempt_id must not re-count spent.
        let err2 = authorize_pay(&home, &mut gate, request).expect_err("retry still refuses");
        let message2 = err2.to_string();
        assert!(
            message2.contains("ext")
                || message2.contains("refused")
                || message2.contains("transport"),
            "unexpected retry error: {message2}"
        );
        assert_eq!(gate.spent(), 2, "retry must not double-count spent");
        let reloaded = BudgetGate::from_home(&home).expect("reload");
        assert_eq!(reloaded.spent(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    // --- NIP-42 receipt auth-wait gate (the receipt-auth fix) --------------------------
    // Smallest testable seam of the fix: the decision that gates the receipt
    // `send_event_to` on a confirmed relay AUTH. The full live publish is real relay I/O
    // (proven by the coordinator's live re-run); the auth-ordering / fail-closed decision
    // is pure and is asserted here (red-on-revert: defeating the gate turns the
    // fail-closed cases green→red).
    use nostr_sdk::pool::RelayNotification;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn nip42_auth_wait_true_only_on_authenticated() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<RelayNotification>(8);
        tx.send(RelayNotification::Authenticated).expect("send");
        assert!(
            wait_for_nip42_auth(&mut rx, Duration::from_secs(20)).await,
            "Authenticated must gate the send open"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nip42_auth_wait_fails_closed_on_timeout() {
        // Sender kept alive, no Authenticated ever arrives ⇒ the bounded wait elapses ⇒ the
        // send is NOT reached (empty relay_success upstream ⇒ verify holds at `Sent`).
        let (_tx, mut rx) = tokio::sync::broadcast::channel::<RelayNotification>(8);
        assert!(
            !wait_for_nip42_auth(&mut rx, Duration::from_millis(50)).await,
            "auth timeout must fail closed (never a forced success)"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nip42_auth_wait_fails_closed_on_authentication_failed() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<RelayNotification>(8);
        tx.send(RelayNotification::AuthenticationFailed).expect("send");
        assert!(
            !wait_for_nip42_auth(&mut rx, Duration::from_secs(20)).await,
            "AuthenticationFailed must fail closed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nip42_auth_wait_fails_closed_on_shutdown() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<RelayNotification>(8);
        tx.send(RelayNotification::Shutdown).expect("send");
        assert!(
            !wait_for_nip42_auth(&mut rx, Duration::from_secs(20)).await,
            "relay Shutdown before auth must fail closed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nip42_auth_wait_fails_closed_on_channel_closed() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<RelayNotification>(8);
        drop(tx);
        assert!(
            !wait_for_nip42_auth(&mut rx, Duration::from_secs(20)).await,
            "notification channel closed before auth must fail closed"
        );
    }

    // --- created_at freshness (the receipt-created-at fix) --------------------------------
    // The receipt event's `created_at` must be FRESH wall-clock per publish (so a real relay's
    // ±time-window accepts it), NOT derived from the preimage digest. Red-on-revert: restoring
    // the superseded digest-derived body makes `created` land in 2023..2027 (≈1_747_303_441 for
    // this fixed digest), OUTSIDE [before, after], and this assert FAILS.
    #[test]
    fn receipt_created_at_is_fresh_wall_clock_not_digest_derived() {
        let digest = [0x11u8; 32];
        let before = Timestamp::now().as_secs();
        let created = receipt_created_at(&digest).as_secs();
        let after = Timestamp::now().as_secs();
        assert!(
            (before..=after).contains(&created),
            "receipt created_at {created} is not fresh wall-clock (expected within [{before}, {after}])"
        );
    }

    // A fresh `created_at` must NOT disturb the co-signed receipt CONTENT: the built + signed
    // kind-3400 still carries the job-hash and BOTH schnorr co-signature tags (only `created_at`
    // — and therefore the event id — changed).
    #[test]
    fn receipt_event_binds_cosigned_content_with_fresh_created_at() {
        let buyer = Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_hash = "cc".repeat(32);
        let integrity = "aa".repeat(20); // 40-char oid
        let draft = gateway::receipt_draft(
            "offer-id",
            "result-id",
            &buyer_hex,
            "seller-hex",
            "https://testnut.cashu.space",
            7,
            &job_hash,
            "seller-sig-hex",
            "buyer-sig-hex",
            Some(gateway::ReceiptDelivery {
                integrity_hash: &integrity,
                kind: "fork",
            }),
            &[],
        );
        let before = Timestamp::now().as_secs();
        let event = gateway::nostr::event_builder(&draft)
            .expect("event builder")
            .custom_created_at(receipt_created_at(&[0x22u8; 32]))
            .sign_with_keys(&buyer)
            .expect("sign");
        let after = Timestamp::now().as_secs();
        assert!(
            (before..=after).contains(&event.created_at.as_secs()),
            "signed receipt created_at is not fresh wall-clock"
        );
        assert_eq!(event.kind.as_u16(), gateway::JOB_RECEIPT_KIND);
        let tag_value = |name: &str, at: usize| -> Option<String> {
            event.tags.iter().find_map(|tag| {
                let slice = tag.as_slice();
                if slice.first().map(String::as_str) == Some(name) {
                    slice.get(at).cloned()
                } else {
                    None
                }
            })
        };
        assert_eq!(tag_value("job-hash", 1).as_deref(), Some(job_hash.as_str()));
        let sig_labels: Vec<String> = event
            .tags
            .iter()
            .filter_map(|tag| {
                let slice = tag.as_slice();
                if slice.first().map(String::as_str) == Some("sig") {
                    slice.get(1).cloned()
                } else {
                    None
                }
            })
            .collect();
        assert!(
            sig_labels.iter().any(|label| label == "seller"),
            "sig/seller tag missing"
        );
        assert!(
            sig_labels.iter().any(|label| label == "buyer"),
            "sig/buyer tag missing"
        );
    }
}
