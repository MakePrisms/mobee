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
use crate::delivery::{CommitOid, Delivery, DeliveryError, GitDelivery};
use crate::delivery_git::PayPathDeliveryVerifier;
use crate::gateway;
use crate::home::{self, MobeeHome};
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
    /// Piece-14: SHA-256 hex of the seller-authored NUT-18 payment request (`creqA…`), sourced
    /// from the accepted claim's `creq` tag (threaded through the accept-bind). `None` for a v1
    /// claim with no `creq` — the attempt id and receipt preimage then bind byte-identically to
    /// before piece-14. NOT mandatory this job (A′ flips requirements); `Some` once Job C authors
    /// the creq. Bound into the [`PaymentKey`] attempt id and the co-signed receipt preimage.
    pub creq_hash: Option<String>,
    /// Piece-14 Job E: the seller-authored `creq`'s accepted-mint list (`m`), read off the
    /// accepted claim. The buyer pays from a mint it holds balance at that appears here; empty for
    /// a v1 claim with no `creq` — the buyer then pays from the pinned default mint exactly as
    /// before piece-14.
    #[allow(clippy::struct_field_names)]
    pub accepted_mints: Vec<String>,
    /// Piece-10 contribution binds. `None` ⇒ from-scratch job — EXACTLY today's path (no new
    /// verify, byte-identical produced artifacts). `Some(..)` ⇒ the fork contribution verify-path
    /// (custody fetch + base-from-pin + descendant + content) + the authorship tuple seam run
    /// pre-pay, all against these buyer-controlled binds.
    pub contribution: Option<ContributionPayBinds>,
}

/// Buyer-controlled contribution binds threaded from the signed offer / accept-bind into the pay
/// path. `repo`/`branch`/`commit_oid` on the enclosing request ARE the fork (`fork_ref` + fork tip);
/// these add the pinned target + base + the seller's authorship signature. All authority is the
/// buyer's signed offer — never a seller echo (MUST-4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContributionPayBinds {
    /// Pinned target owner pubkey (hex) — from the buyer's signed offer.
    pub target_owner_pubkey: String,
    /// Pinned target clone URL — base_oid is fetched from HERE (MUST-2), never the seller echo.
    pub target_clone_url: String,
    /// Base branch the exact `base_oid` lives on in the pinned target.
    pub base_branch: String,
    /// The exact commit the delivery must descend from (from the buyer's signed offer).
    pub base_oid: String,
    /// Seller schnorr signature (hex) over the signed-result authorship tuple (`sig/seller-contribution`).
    pub tuple_signature: String,
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
///
/// CALLER CONTRACT (contribution gating): this function trusts `request.contribution` —
/// `None` is treated as a from-scratch job, so the four contribution gates + the
/// authorship-tuple bind are skipped. The offer's `job-class` lives on the relay, which this
/// function deliberately does not read (no network beyond the delivery fetch). EVERY
/// production caller therefore inherits the guard obligation: refuse to pay a
/// `job-class=contribution` offer with `contribution: None` (the MCP pay tool re-derives the
/// class and refuses fail-closed; a bind-built request resolves it at accept). A new caller
/// that skips this check reopens the gate bypass this contract exists to prevent.
pub async fn authorize_pay_async(
    home: &MobeeHome,
    gate: &mut BudgetGate,
    request: AuthorizePayRequest,
) -> Result<AuthorizePayOutcome, AuthorizePayError> {
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
    // Piece-14 Job E + issue #49: choose the realized mint the buyer pays at from the seller's
    // `creq` `m` list, keyed off the buyer's CONFIGURED mint (same source `buyer_fund` opens the
    // spending wallet at) — never a compile-time pin.
    let mint = resolve_realized_mint(
        home.config.default_mint(),
        &request.accepted_mints,
        home.config.allow_real_mints,
    )?;
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
        request.creq_hash.clone(),
    );
    let attempt_id = key.attempt_id();

    let commit_oid = CommitOid::parse(request.commit_oid)?;
    // Re-type onto the typed [`Delivery`]: the sole live variant is `Commit` (fork tip). Its
    // `delivery_kind()` (→ `Fork`/`"fork"`) replaces the former hardcode in the receipt preimage;
    // for `Commit` this is byte-identical. The buyer tip-match gate above stays a raw compare of
    // `delivery_integrity_hash == commit_oid` — for `Commit`, `commit_oid` IS the variant's bound
    // oid (routing it through the parsed `Delivery::bound_oid()` would lowercase the oid and
    // reorder the parse-vs-gate refusals, i.e. change behavior on the refuse path).
    let delivery = Delivery::Commit(GitDelivery::new(request.repo, request.branch, commit_oid)?);
    let delivery_kind = delivery.delivery_kind();

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

    // Buyer-owned custody verifier (no wallet dependency; created before the pre-pay seam so the
    // contribution verify runs against custody BEFORE any spend). The payment-journal is created
    // LATER (after the pre-pay seam) so a pre-pay refusal leaves NO journal on disk.
    let custody = home.root.join("custody");
    let mut verifier = PayPathDeliveryVerifier::new(custody);

    // Piece-10 contribution verify-path — ALL PRE-PAY (before the budget gate ⇒ zero spend on any
    // refusal), ALL against BUYER-CONTROLLED binds. The fork (`delivery`) is custody-fetched +
    // tip-matched, `base_oid` is fetched from the PINNED target (never the seller echo), the
    // delivery must DESCEND from base, and the content gate + buyer policy hook must pass. The
    // authorship tuple sig is then verified at the ONE pre-pay seam below (extending the receipt
    // cosig). From-scratch jobs skip this block entirely (`contribution == None`).
    let contribution_cosig = if let Some(binds) = request.contribution.as_ref() {
        let base_oid = CommitOid::parse(binds.base_oid.clone())
            .map_err(|error| AuthorizePayError::Input(format!("contribution base_oid: {error}")))?;
        let Delivery::Commit(fork) = &delivery;
        let fork = fork.clone();
        let policy = contribution_policy(home);
        verifier
            .verify_contribution(
                &fork,
                &binds.target_clone_url,
                &binds.base_branch,
                &base_oid,
                &policy,
            )
            .map_err(AuthorizePayError::Delivery)?;
        // Reconstruct the exact tuple the seller signed (from BUYER-controlled binds) and carry its
        // digest + the seller's signature to the pre-pay seam. A tuple field tampered post-signing
        // (or a sig over a different commit_oid) fails there with ZERO spend.
        let tuple = crate::contribution::AuthorshipTuple {
            job_id: request.job_id.clone(),
            seller_pubkey: seller_hex.clone(),
            target: crate::contribution::TargetRepoPin::new(
                binds.target_owner_pubkey.clone(),
                binds.target_clone_url.clone(),
            )
            .map_err(|error| AuthorizePayError::Input(error.to_string()))?,
            base_oid: binds.base_oid.clone(),
            fork: crate::contribution::ForkRef::new(fork.repo(), fork.branch())
                .map_err(|error| AuthorizePayError::Input(error.to_string()))?,
            commit_oid: fork.commit_oid().as_str().to_owned(),
        };
        Some((tuple.digest_bytes(), binds.tuple_signature.clone()))
    } else {
        None
    };

    // THE LOAD-BEARING PRE-PAY TOOTH (cross-bind / forged-cosig). Rebuild the EXACT receipt
    // preimage the pay path will co-sign and publish (same `receipt_preimage_for` constructor
    // as `build_and_publish_receipt`, so the verified bytes cannot drift from the published
    // bytes) and verify the seller's `sig/seller` over it against the claim-seller anchor —
    // BEFORE the budget gate commits spent and BEFORE the wallet opens. For a contribution the
    // SAME seam ALSO verifies the seller's signed-result authorship tuple (one seam, more binds).
    // Fail-closed here ⇒ ZERO spend: no `authorize_then_attempt`, no lock/mint/send, no receipt,
    // no journal record.
    let prepay_preimage =
        receipt_preimage_for(&key, &buyer_nostr.to_hex(), &seller_hex, delivery_kind);
    let contribution_bind = contribution_cosig
        .as_ref()
        .map(|(digest, sig)| crate::payment::ContributionCosig {
            tuple_digest: *digest,
            tuple_signature_hex: sig.as_str(),
        });
    authority
        .verify_seller_prepay_cosig(&prepay_preimage, &request.seller_signature, contribution_bind)
        .map_err(AuthorizePayError::Payment)?;

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
                delivery_kind,
                key,
            )
        },
    )
    .map_err(|error| AuthorizePayError::Effects(error.to_string()))?;

    // Payment journal — created only AFTER the pre-pay seam passed (a pre-pay refusal leaves no
    // journal on disk, preserving the zero-spend / no-record invariant).
    let journal_dir = home.root.join("payment-journal");
    std::fs::create_dir_all(&journal_dir)
        .map_err(|error| AuthorizePayError::Home(format!("payment journal dir: {error}")))?;
    let journal = FsPaymentJournal::new(journal_dir.join(format!("{}.jsonl", attempt_id.as_str())));

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

/// Resolve the buyer's piece-10 content policy hook (MUST-5) from `[contribution]` config, or the
/// FLOOR (refuse only empty diffs) when unconfigured. Buyer-side; never seller-influenced.
fn contribution_policy(home: &MobeeHome) -> crate::contribution::ContentPolicy {
    match &home.config.contribution {
        Some(cfg) => crate::contribution::ContentPolicy {
            allowed_paths: cfg.allowed_paths.clone(),
            forbidden_paths: cfg.forbidden_paths.clone(),
            max_diff_bytes: cfg.max_diff_bytes,
        },
        None => crate::contribution::ContentPolicy::floor(),
    }
}

/// PIECE-14 Job E buyer mint selection (§ The Lightning bridge), config-driven (issue #49).
///
/// `buyer_mint_url` is the mint the buyer's wallet spends from — the home config's default mint
/// ([`crate::home::MobeeConfig::default_mint`]), the SAME source `buyer_fund` opens the wallet at.
/// The buyer pays from a mint it holds balance at that the seller listed in its `creq` `m` array;
/// since the buyer wallet is single-mint, that reduces to: is the buyer's configured mint listed?
///
/// - **empty creq list (v1 / no creq):** pay from the buyer's configured mint.
/// - **configured mint listed:** pay directly from it (the direct path).
/// - **configured mint NOT listed:** the buyer would have to BRIDGE over Lightning
///   ([`crate::payment_wallet::bridge_to_accepted_mint`]). That live cross-mint path is fail-closed
///   (single-mint wallet — no second reachable mint to mint-quote/melt against), so it refuses
///   `mint_unreachable_pay`; no funds move, no binding is committed. The bridge mechanism is
///   implemented and unit-tested for a future multi-mint enablement.
fn resolve_realized_mint(
    buyer_mint_url: &str,
    accepted_mints: &[String],
    allow_real_mints: bool,
) -> Result<MintUrl, AuthorizePayError> {
    // Real-mint fence (issue #49): the buyer's own paying mint must be admissible under the flag.
    // Default (`allow_real_mints=false`) admits only the testnut/dev allow-list; a real mint is
    // refused fail-closed before any spend unless the operator opts in.
    if !crate::home::mint_allowed(buyer_mint_url, allow_real_mints) {
        return Err(AuthorizePayError::Input(format!(
            "real-mint fence: buyer mint {buyer_mint_url} is not an allow-listed testnut/dev mint; \
             set allow_real_mints=true to pay at a real mint"
        )));
    }
    let buyer_mint = MintUrl::from_str(buyer_mint_url)
        .map_err(|error| AuthorizePayError::Input(format!("buyer mint url: {error}")))?;
    if accepted_mints.is_empty() {
        return Ok(buyer_mint);
    }
    let listed = accepted_mints
        .iter()
        .map(|entry| MintUrl::from_str(entry))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            AuthorizePayError::Input(format!("creq accepted mint url: {error}"))
        })?;
    if listed.contains(&buyer_mint) {
        return Ok(buyer_mint);
    }
    Err(AuthorizePayError::Wallet(PaymentWalletError::Wallet(format!(
        "mint_unreachable_pay: buyer mint {buyer_mint} is not in the creq mint list {listed:?}; \
         the live cross-mint Lightning bridge is disabled (single-mint wallet)"
    ))))
}

fn cashu_compressed_from_nostr(key: &NostrPublicKey) -> Result<CashuPublicKey, AuthorizePayError> {
    CashuPublicKey::from_str(&format!("02{}", key.to_hex())).map_err(|error| {
        AuthorizePayError::Input(format!("cashu pubkey from nostr key: {error}"))
    })
}

/// The SINGLE co-signed-receipt-preimage constructor for this trade.
///
/// Used by BOTH the pre-pay seller-cosig tooth (before any spend) and
/// [`build_and_publish_receipt`] (at publish), so the bytes the buyer verifies pre-spend are
/// byte-identical to the bytes it later co-signs and publishes — the two can never drift.
/// `delivery_kind` is derived from the typed [`Delivery`] variant (`Commit` → `"fork"`);
/// `exec_metadata_commitment` is the empty marker (exec-metadata is seller-claimed, not
/// co-signed — piece-9 Item 2). Field set / order matches `receipt.rs` `ReceiptPreimage`.
fn receipt_preimage_for(
    key: &PaymentKey,
    buyer_pubkey_hex: &str,
    seller_pubkey_hex: &str,
    delivery_kind: DeliveryKind,
) -> ReceiptPreimage {
    ReceiptPreimage {
        job_hash: key.job_hash.as_str().to_owned(),
        offer_id: key.job_id.as_str().to_owned(),
        amount: key.amount.to_u64(),
        unit: key.unit.to_string(),
        mint: key.mint.to_string(),
        buyer_pubkey: buyer_pubkey_hex.to_owned(),
        seller_pubkey: seller_pubkey_hex.to_owned(),
        delivery_integrity_hash: key.delivery_integrity_hash.as_str().to_owned(),
        delivery_kind: delivery_kind.as_str().to_owned(),
        exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
        // Piece-14: bind the seller-authored request hash the key carries, so the pre-pay tooth
        // and the published receipt co-sign the same bytes (byte-identical when `None` — v1).
        creq_hash: key.creq_hash.clone(),
    }
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
    delivery_kind: DeliveryKind,
    key: &PaymentKey,
) -> Result<ReceiptEvidence, EffectError> {
    let buyer_hex = buyer_keys.public_key().to_hex();
    let mint = key.mint.to_string();
    let amount = key.amount.to_u64();
    // offer_id == job_id in this codebase (the offer event id is the job id). Built via the
    // SINGLE shared constructor the pre-pay tooth also uses, so the co-signed bytes published
    // here are byte-identical to the bytes verified before the spend (they cannot drift).
    let preimage = receipt_preimage_for(key, &buyer_hex, seller_hex, delivery_kind);
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
        // Piece-14: the receipt event carries the bound request hash (absent for a v1 trade).
        key.creq_hash.as_deref(),
        Some(gateway::ReceiptDelivery {
            integrity_hash: key.delivery_integrity_hash.as_str(),
            kind: delivery_kind.as_str(),
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
    use crate::home::{self, DEFAULT_MINT_URL};

    // A real (non-testnut) mint — admissible ONLY when `allow_real_mints` is true (issue #49).
    const REAL_MINT: &str = "https://minibits.example";

    // Empty creq list (v1 / no creq) → pay from the buyer's configured mint (config-driven, #49).
    // Default flag (false): the configured testnut/dev mint resolves.
    #[test]
    fn resolve_realized_mint_empty_creq_uses_configured_mint() {
        let mint = resolve_realized_mint(DEFAULT_MINT_URL, &[], false).unwrap();
        assert_eq!(mint, MintUrl::from_str(DEFAULT_MINT_URL).unwrap());
    }

    // Direct path: the buyer's configured mint is one the seller listed → pay from it directly.
    #[test]
    fn resolve_realized_mint_direct_when_configured_mint_is_listed() {
        let mint = resolve_realized_mint(
            DEFAULT_MINT_URL,
            &[
                "https://other.example".to_string(),
                DEFAULT_MINT_URL.to_string(),
            ],
            false,
        )
        .unwrap();
        assert_eq!(mint, MintUrl::from_str(DEFAULT_MINT_URL).unwrap());
    }

    // Configured mint NOT in the creq list → refuse `mint_unreachable_pay` fail-closed (no spend).
    #[test]
    fn resolve_realized_mint_refuses_when_configured_mint_not_listed() {
        let error = resolve_realized_mint(
            DEFAULT_MINT_URL,
            &["https://other.example".to_string()],
            false,
        )
        .unwrap_err();
        assert!(matches!(error, AuthorizePayError::Wallet(_)));
        assert!(error.to_string().contains("mint_unreachable_pay"));
    }

    // Issue #49 real-mint switch: a buyer configured at a real mint X is REFUSED by the fence when
    // `allow_real_mints` is false (default safety posture)...
    #[test]
    fn resolve_realized_mint_real_mint_refused_by_default() {
        let error = resolve_realized_mint(REAL_MINT, &[REAL_MINT.to_string()], false).unwrap_err();
        assert!(matches!(error, AuthorizePayError::Input(_)));
        assert!(error.to_string().contains("real-mint fence"));
    }

    // ...and ADMITTED (pays at X when the creq lists X) once the operator opts in with the flag.
    #[test]
    fn resolve_realized_mint_real_mint_admitted_when_flag_true() {
        let paid = resolve_realized_mint(REAL_MINT, &[REAL_MINT.to_string()], true).unwrap();
        assert_eq!(paid, MintUrl::from_str(REAL_MINT).unwrap());

        // Even with the flag on, a mint the creq did NOT list still refuses (membership unchanged).
        let refused =
            resolve_realized_mint(REAL_MINT, &[DEFAULT_MINT_URL.to_string()], true).unwrap_err();
        assert!(refused.to_string().contains("mint_unreachable_pay"));
    }

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
            creq_hash: None,
            accepted_mints: Vec::new(),
            contribution: None,
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
            creq_hash: None,
            accepted_mints: Vec::new(),
            contribution: None,
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
            creq_hash: None,
            accepted_mints: Vec::new(),
            contribution: None,
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
        // A VALID seller co-signature is now required to reach this point: the pre-pay tooth
        // runs first, so a bad/empty sig would refuse at ZERO spend and this test would no
        // longer exercise the write-before-effect (spent==2) path it guards. Signing here lets
        // the pre-pay gate PASS, so the pay path still refuses at the ext locator AFTER spent.
        let valid_sig = seller_cosig(
            &home,
            &prepay_preimage(&home, "job-ext", "result-ext", &"bb".repeat(32), &"aa".repeat(20), 2),
        );
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
            seller_signature: valid_sig,
            creq_hash: None,
            accepted_mints: Vec::new(),
            contribution: None,
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

    // --- PRE-PAY seller-cosig tooth (the cross-bind / forged-cosig fix) ------------------
    // Rebuild the co-signed receipt preimage EXACTLY as `authorize_pay_async` does (via the
    // shared `receipt_preimage_for`), for a home where buyer == seller == the home key. Used to
    // mint a REAL seller co-signature (or one over tampered bytes) for the pre-pay tooth.
    fn prepay_preimage(
        home: &MobeeHome,
        job_id: &str,
        result_id: &str,
        job_hash: &str,
        oid: &str,
        amount_sats: u64,
    ) -> ReceiptPreimage {
        let hex = home::public_key_hex(home).expect("pubkey");
        let seller_nostr = NostrPublicKey::parse(&hex).expect("seller nostr");
        let seller_p2pk = cashu_compressed_from_nostr(&seller_nostr).expect("p2pk");
        let terms = PaymentTerms::new(
            MintUrl::from_str(DEFAULT_MINT_URL).expect("mint"),
            Amount::from(amount_sats),
            CurrencyUnit::Sat,
            seller_nostr,
            seller_p2pk,
        );
        let key = PaymentKey::new(
            JobId::new(job_id).expect("job id"),
            ResultId::new(result_id).expect("result id"),
            DeliveryIntegrityHash::from_hex(oid).expect("oid"),
            JobHash::from_hex(job_hash).expect("job hash"),
            &terms,
            None,
        );
        // buyer == seller == home key in these tests; `Commit` → delivery_kind "fork".
        receipt_preimage_for(&key, &hex, &hex, DeliveryKind::Fork)
    }

    fn seller_cosig(home: &MobeeHome, preimage: &ReceiptPreimage) -> String {
        let secret = home::read_secret_key_hex(home).expect("secret");
        let keys = Keys::parse(&secret).expect("keys");
        keys.sign_schnorr(&Message::from_digest(preimage.digest_bytes()))
            .to_string()
    }

    // THE LOAD-BEARING TOOTH: a forged/mismatched seller signature — a REAL schnorr sig by an
    // unrelated key over the CORRECT preimage (buyer-cosig would PASS / seller-cosig FAILs: the
    // live 21-sat receipt shape) — refuses BEFORE any spend. gate.spent()==0, no wallet opened,
    // no payment journal, never Sent. `repo: ext::…` is chosen so that a REVERTED gate
    // (red-on-revert) still refuses hermetically at the pay-path verifier — but only AFTER
    // committing spent, so removing this tooth flips gate.spent() 0→2.
    #[test]
    fn authorize_pay_refuses_forged_seller_signature_with_zero_spend() {
        let root = std::env::temp_dir().join(format!(
            "mobee-authorize-pay-forged-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let oid = "aa".repeat(20);
        let job_hash = "bb".repeat(32);
        let preimage = prepay_preimage(&home, "job-forged", "result-forged", &job_hash, &oid, 2);
        // Real schnorr signature, but by an unrelated key — not the claim seller.
        let attacker = Keys::generate();
        let forged_sig = attacker
            .sign_schnorr(&Message::from_digest(preimage.digest_bytes()))
            .to_string();
        let request = AuthorizePayRequest {
            job_id: "job-forged".into(),
            result_id: "result-forged".into(),
            delivery_integrity_hash: oid.clone(),
            job_hash,
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 2,
            repo: "ext::sh -c evil".into(),
            branch: "main".into(),
            commit_oid: oid,
            seller_signature: forged_sig,
            creq_hash: None,
            accepted_mints: Vec::new(),
            contribution: None,
        };
        let err = authorize_pay(&home, &mut gate, request).expect_err("forged sig refused pre-pay");
        assert!(
            err.to_string().contains("pre-pay seller co-signature invalid"),
            "must be the pre-pay tooth refusal, got: {err}"
        );
        assert_eq!(gate.spent(), 0, "forged-sig refuse must be ZERO spend (pre-pay tooth)");
        assert!(
            !home.root.join("payment-journal").exists(),
            "no payment journal may be created (refused before the payment SM / any Sent)"
        );
        let reloaded = BudgetGate::from_home(&home).expect("reload");
        assert_eq!(reloaded.spent(), 0, "durable spent must stay 0");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Tampered-field parity: a seller signature over the honest preimage no longer verifies
    // once ANY covered field is flipped post-signing (the sig covers the exact canonical
    // bytes). Same refusal, zero spend — checked for the amount field and the delivery oid.
    #[test]
    fn authorize_pay_refuses_tampered_preimage_field_with_zero_spend() {
        let root = std::env::temp_dir().join(format!(
            "mobee-authorize-pay-tamper-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let seller_hex = home::public_key_hex(&home).expect("pubkey");
        let honest_oid = "aa".repeat(20);
        let honest_hash = "bb".repeat(32);

        // (a) amount tampered: seller signed amount=2, request carries amount=3.
        let sig_over_2 = seller_cosig(
            &home,
            &prepay_preimage(&home, "job-tamper", "result-tamper", &honest_hash, &honest_oid, 2),
        );
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let tampered_amount = AuthorizePayRequest {
            job_id: "job-tamper".into(),
            result_id: "result-tamper".into(),
            delivery_integrity_hash: honest_oid.clone(),
            job_hash: honest_hash.clone(),
            seller_pubkey: seller_hex.clone(),
            amount_sats: 3,
            repo: "ext::sh -c evil".into(),
            branch: "main".into(),
            commit_oid: honest_oid.clone(),
            seller_signature: sig_over_2,
            creq_hash: None,
            accepted_mints: Vec::new(),
            contribution: None,
        };
        let err = authorize_pay(&home, &mut gate, tampered_amount).expect_err("tampered amount");
        assert!(
            err.to_string().contains("pre-pay seller co-signature invalid"),
            "amount tamper must refuse at the pre-pay tooth, got: {err}"
        );
        assert_eq!(gate.spent(), 0, "tampered amount must be zero spend");

        // (b) delivery oid tampered: seller signed oid=aa.., request binds oid=cc..
        let tampered_oid = "cc".repeat(20);
        let sig_over_aa = seller_cosig(
            &home,
            &prepay_preimage(&home, "job-tamper2", "result-tamper2", &honest_hash, &honest_oid, 2),
        );
        let mut gate2 = BudgetGate::from_home(&home).expect("gate");
        let tampered_delivery = AuthorizePayRequest {
            job_id: "job-tamper2".into(),
            result_id: "result-tamper2".into(),
            delivery_integrity_hash: tampered_oid.clone(),
            job_hash: honest_hash,
            seller_pubkey: seller_hex,
            amount_sats: 2,
            repo: "ext::sh -c evil".into(),
            branch: "main".into(),
            commit_oid: tampered_oid,
            seller_signature: sig_over_aa,
            creq_hash: None,
            accepted_mints: Vec::new(),
            contribution: None,
        };
        let err2 = authorize_pay(&home, &mut gate2, tampered_delivery).expect_err("tampered oid");
        assert!(
            err2.to_string().contains("pre-pay seller co-signature invalid"),
            "oid tamper must refuse at the pre-pay tooth, got: {err2}"
        );
        assert_eq!(gate2.spent(), 0, "tampered oid must be zero spend");
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
            None,
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
