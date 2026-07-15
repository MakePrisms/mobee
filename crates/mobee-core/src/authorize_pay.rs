//! MCP / CLI pay entry: BudgetGate → piece-6 [`PaymentService::run`] only.
//!
//! By construction the delivery verifier is [`PayPathDeliveryVerifier`] (allowlist sealed).
//! Stable [`PaymentKey::attempt_id`] feeds `run()`'s reconcile saga — no bespoke pay path,
//! no [`PaymentService::advance`] from this surface.

use std::fmt;
use std::str::FromStr;

use cashu::{Amount, CurrencyUnit, MintUrl, PublicKey as CashuPublicKey};
use nostr_sdk::Keys;
use nostr_sdk::PublicKey as NostrPublicKey;

use crate::budget::{BudgetGate, BudgetRefuse};
use crate::buyer_fund::{self, FundError};
use crate::delivery::{CommitOid, DeliveryError, GitDelivery};
use crate::delivery_git::PayPathDeliveryVerifier;
use crate::home::{self, MobeeHome, DEFAULT_MINT_URL};
use crate::payment::{
    DeliveryIntegrityHash, FsPaymentJournal, JobHash, JobId, PaymentError, PaymentKey,
    PaymentService, PaymentState, PaymentTerms, ReceiptAuthority, ReceiptEvidence, ResultId,
};
use crate::payment_send::NostrPaymentSend;
use crate::payment_wallet::{CdkPaymentEffects, PaymentWalletError};

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
    let buyer_cashu = cashu_compressed_from_nostr(&buyer_nostr)?;
    let seller_cashu = terms.seller_p2pk_lock;
    let authority = ReceiptAuthority {
        buyer: buyer_cashu,
        seller: seller_cashu,
    };

    let wallet = buyer_fund::open_testnut_wallet_blocking(home)?;
    let payment_send = NostrPaymentSend::new(home.config.relay_url.clone(), keys);
    let mut effects = CdkPaymentEffects::spawn(
        wallet,
        payment_send,
        move |_key: &PaymentKey, payment: &crate::payment_send::PaymentSent| {
            Ok(ReceiptEvidence {
                receipt_id: payment.payment_id.clone(),
                author: buyer_cashu,
                valid_signers: vec![buyer_cashu, seller_cashu],
            })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetGate;
    use crate::home;

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
        // Tiny amount within default caps.
        let request = AuthorizePayRequest {
            job_id: "job-ext".into(),
            result_id: "result-ext".into(),
            delivery_integrity_hash: "aa".repeat(20),
            job_hash: "bb".repeat(32),
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 1,
            repo: "ext::sh -c evil".into(),
            branch: "main".into(),
            commit_oid: "aa".repeat(20),
        };
        let err = authorize_pay(&home, &mut gate, request.clone()).expect_err("ext refused");
        let message = err.to_string();
        assert!(
            message.contains("ext") || message.contains("refused") || message.contains("transport"),
            "unexpected error: {message}"
        );
        // Write-before-effect: spent was committed before run() refused.
        assert_eq!(gate.spent(), 1);

        // Reconciled retry of the same PaymentKey attempt_id must not re-count spent.
        let err2 = authorize_pay(&home, &mut gate, request).expect_err("retry still refuses");
        let message2 = err2.to_string();
        assert!(
            message2.contains("ext")
                || message2.contains("refused")
                || message2.contains("transport"),
            "unexpected retry error: {message2}"
        );
        assert_eq!(gate.spent(), 1, "retry must not double-count spent");
        let reloaded = BudgetGate::from_home(&home).expect("reload");
        assert_eq!(reloaded.spent(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
