//! Buyer wallet fund path for packaged `~/.mobee`.
//!
//! One universal, mint-agnostic flow: request a mint quote → surface its bolt11 invoice → poll the
//! quote state until the invoice is paid → mint the proofs. There is no per-mint branch — a
//! testnut/fake mint simply auto-marks its invoice paid, so the same poll returns immediately, while
//! a real mint's quote flips to paid once the invoice is paid externally. The wallet mint is the
//! configured mint ([`crate::home::MobeeConfig::default_mint`]).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod};
use cdk::wallet::Wallet;
use cdk::Amount;
use cdk_sqlite::wallet::WalletSqliteDatabase;
use sha2::{Digest, Sha256};

use crate::home::{self, HomeError, MobeeHome};

/// Small default fund amount for first-run setup (sats).
pub const DEFAULT_FUND_AMOUNT_SATS: u64 = 21;

/// Result of a successful fund (or already-funded status read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundOutcome {
    pub mint_url: String,
    pub invoice: Option<String>,
    pub funded_sats: u64,
    pub balance_sats: u64,
    pub already_funded: bool,
}

#[derive(Debug)]
pub enum FundError {
    Home(HomeError),
    Wallet(String),
    /// The mint quote was still unpaid when the poll window elapsed. Fail-closed (no mint), but
    /// the bolt11 `invoice` is reported so a slow external payment (real mint) is not lost — pay it
    /// and re-run to complete the mint.
    QuoteUnpaid { invoice: String, quote_id: String },
    /// The configured mint is not permitted under the real-mint fence (issue #49): a real mint with
    /// `allow_real_mints == false`. Fail-closed before opening/quoting the wallet.
    MintNotAllowed { mint_url: String },
}

impl std::fmt::Display for FundError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Home(error) => write!(formatter, "{error}"),
            Self::Wallet(message) => write!(formatter, "wallet fund error: {message}"),
            Self::QuoteUnpaid { invoice, quote_id } => write!(
                formatter,
                "mint quote {quote_id} still unpaid; pay the invoice and re-run to complete: {invoice}"
            ),
            Self::MintNotAllowed { mint_url } => write!(
                formatter,
                "mint {mint_url} not allowed (allow_real_mints is off; set MOBEE_ALLOW_REAL_MINTS to opt in)"
            ),
        }
    }
}

impl std::error::Error for FundError {}

impl From<HomeError> for FundError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}

/// Expand the 32-byte nostr secret into a 64-byte cdk wallet seed (deterministic).
pub fn seed_from_secret_hex(secret_hex: &str) -> Result<[u8; 64], FundError> {
    let bytes = hex::decode(secret_hex.trim())
        .map_err(|error| FundError::Wallet(format!("secret key hex decode: {error}")))?;
    if bytes.len() != 32 {
        return Err(FundError::Wallet(format!(
            "secret key must decode to 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut seed = [0u8; 64];
    seed[..32].copy_from_slice(&bytes);
    let digest = Sha256::digest(&bytes);
    seed[32..].copy_from_slice(&digest);
    Ok(seed)
}

fn sqlite_path(wallet_dir: &Path) -> std::path::PathBuf {
    wallet_dir.join("cdk-wallet.sqlite")
}

/// Open the wallet at the configured default mint (async). Prefer this inside an existing
/// runtime — [`open_wallet_blocking`] fails fast if a Tokio runtime is
/// already current (no nested `block_on` panic).
pub async fn open_wallet_async(home: &MobeeHome) -> Result<Wallet, FundError> {
    // The wallet opens at the CONFIGURED mint (`MobeeConfig::default_mint`), the same
    // source of truth the pay path resolves the realized mint from — no compile-time pin.
    open_wallet_at_mint_async(home, home.config.default_mint()).await
}

/// Open the shared wallet store at an EXPLICIT mint (async). Multi-mint redemption: a buyer may
/// pay at any mint in the seller's accepted set, so the seller must open the wallet at the
/// buyer's REALIZED mint (the NUT-18 payload mint), not the seller default — otherwise
/// `require_wallet_matches` refuses every non-default-mint payment. The store (sqlite) is shared
/// across mints; only the `Wallet`'s bound mint differs.
pub async fn open_wallet_at_mint_async(
    home: &MobeeHome,
    mint_url: &str,
) -> Result<Wallet, FundError> {
    // Real-mint fence (issue #49): fail closed BEFORE opening/quoting if this mint is a real mint
    // and the operator has not opted in (`allow_real_mints == false`), the same gate the
    // send/melt/receive paths enforce. Callers may have already fenced the realized mint; this
    // re-checks so the helper is safe on its own.
    if !home::mint_allowed(mint_url, home.config.allow_real_mints) {
        return Err(FundError::MintNotAllowed {
            mint_url: mint_url.to_owned(),
        });
    }
    let secret = home::read_secret_key_hex(home)?;
    let seed = seed_from_secret_hex(&secret)?;
    let path = sqlite_path(&home.wallet_dir);
    let store = WalletSqliteDatabase::new(path)
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    Wallet::new(mint_url, CurrencyUnit::Sat, Arc::new(store), seed, None)
        .map_err(|error| FundError::Wallet(error.to_string()))
}

/// Thin sync wrapper for non-async callers (CLI / tests).
/// Do **not** call from inside an existing tokio runtime — use
/// [`open_wallet_async`] instead. Nested call fails fast (no panic).
pub fn open_wallet_blocking(home: &MobeeHome) -> Result<Wallet, FundError> {
    crate::runtime_guard::refuse_nested_block_on("open_wallet_blocking")
        .map_err(FundError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    runtime.block_on(open_wallet_async(home))
}

/// Read current wallet balance against the configured default mint.
pub async fn wallet_balance_sats(home: &MobeeHome) -> Result<u64, FundError> {
    let wallet = open_wallet_async(home).await?;
    let balance = wallet
        .total_balance()
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    Ok(balance.to_u64())
}

/// Fund via mint-quote → wait for payment → mint. Idempotent if balance > 0.
pub async fn fund_wallet(
    home: &MobeeHome,
    amount_sats: u64,
) -> Result<FundOutcome, FundError> {
    let wallet = open_wallet_async(home).await?;
    let existing = wallet
        .total_balance()
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?
        .to_u64();
    if existing > 0 {
        return Ok(FundOutcome {
            mint_url: home.config.default_mint().to_owned(),
            invoice: None,
            funded_sats: 0,
            balance_sats: existing,
            already_funded: true,
        });
    }

    let amount = Amount::from(amount_sats);
    let quote = wallet
        .mint_quote(PaymentMethod::BOLT11, Some(amount), None, None)
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    let invoice = quote.request.clone();
    let quote_id = quote.id.clone();

    // Poll HTTP quote status — do not use wait_and_mint_quote (its WS stream can hang; polling is
    // deterministic). Same path for every mint: wait until the quote flips to Paid/Issued. A
    // testnut/fake mint marks its invoice paid immediately, so this returns at once; a real mint's
    // quote flips once the invoice is paid externally.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let status = wallet
            .check_mint_quote(&quote_id)
            .await
            .map_err(|error| FundError::Wallet(error.to_string()))?;
        match status.state {
            MintQuoteState::Paid | MintQuoteState::Issued => break,
            MintQuoteState::Unpaid => {
                if tokio::time::Instant::now() >= deadline {
                    // Fail closed (no mint) but report the invoice so a slow external payment is
                    // not lost — the caller can pay it and re-run to complete the mint.
                    return Err(FundError::QuoteUnpaid {
                        invoice: invoice.clone(),
                        quote_id: quote_id.clone(),
                    });
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }

    let proofs = wallet
        .mint(&quote_id, Default::default(), None)
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    let funded = proofs
        .iter()
        .map(|proof| proof.amount.to_u64())
        .fold(0u64, |acc, value| acc.saturating_add(value));
    let balance = wallet
        .total_balance()
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?
        .to_u64();
    if balance == 0 {
        return Err(FundError::Wallet(
            "mint completed but observed balance is 0".into(),
        ));
    }
    Ok(FundOutcome {
        mint_url: home.config.default_mint().to_owned(),
        invoice: Some(invoice),
        funded_sats: funded,
        balance_sats: balance,
        already_funded: false,
    })
}

/// Blocking wrapper for CLI / tests (current-thread runtime).
/// Nested call from an async context fails fast — use [`fund_wallet`].
pub fn fund_wallet_blocking(
    home: &MobeeHome,
    amount_sats: u64,
) -> Result<FundOutcome, FundError> {
    crate::runtime_guard::refuse_nested_block_on("fund_wallet_blocking")
        .map_err(FundError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    runtime.block_on(fund_wallet(home, amount_sats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::{bootstrap, MobeeConfig, DEFAULT_MINT_URL};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-fund-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn seed_is_deterministic_64_bytes() {
        let secret = "11".repeat(32);
        let a = seed_from_secret_hex(&secret).expect("a");
        let b = seed_from_secret_hex(&secret).expect("b");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    // The wallet opens at the CONFIGURED mint, not a compile-time pin — a buyer
    // configured at a non-default mint spends from that mint (no `MintPinned` refusal). A real
    // (non-testnut) mint requires the operator opt-in (`allow_real_mints`; see the real-mint fence).
    #[test]
    fn wallet_opens_at_the_configured_mint() {
        let root = temp_home("cfg-mint");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        home.config.accepted_mints = vec!["https://minibits.example".into()];
        home.config.allow_real_mints = true;
        let wallet = open_wallet_blocking(&home).expect("open at configured mint");
        assert_eq!(wallet.mint_url.to_string(), "https://minibits.example");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Z2 (multi-mint redeem): the seller opens the shared store at the buyer's REALIZED mint, which
    // may be a NON-default member of accepted_mints (default_mint == accepted_mints[0]). The opened
    // wallet binds to the requested realized mint, not the config default — the fix that lets
    // `require_wallet_matches` pass for a non-default-mint payment.
    #[tokio::test(flavor = "current_thread")]
    async fn open_wallet_at_mint_binds_to_realized_non_default_mint() {
        let root = temp_home("realized-mint");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        // accepted[0] (== default) is the seller default; accepted[1] is the realized mint.
        home.config.accepted_mints = vec![
            "https://default-testnut.example".into(),
            "https://realized-testnut.example".into(),
        ];
        home.config.allow_real_mints = true;
        assert_eq!(home.config.default_mint(), "https://default-testnut.example");
        let realized = "https://realized-testnut.example";
        let wallet = open_wallet_at_mint_async(&home, realized)
            .await
            .expect("open at realized mint");
        assert_eq!(wallet.mint_url.to_string(), realized);
        assert_ne!(wallet.mint_url.to_string(), home.config.default_mint());
        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding T(2): opening the buyer wallet fails closed on a non-allowlisted REAL mint when
    // allow_real_mints=false — BEFORE any network open/quote — the same fence send/melt/receive
    // enforce. With the opt-in it opens (covered by `wallet_opens_at_the_configured_mint`).
    #[test]
    fn open_wallet_refuses_real_mint_when_disallowed() {
        let root = temp_home("open-real-mint-fence");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        home.config.accepted_mints = vec!["https://real-mint.example/".into()];
        home.config.allow_real_mints = false;
        let err = open_wallet_blocking(&home).expect_err("real mint must refuse under allow_real_mints=false");
        assert!(
            matches!(&err, FundError::MintNotAllowed { mint_url } if mint_url == "https://real-mint.example/"),
            "expected MintNotAllowed, got {err:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_fund_refuses_inside_runtime() {
        let root = temp_home("nested-refuse");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = fund_wallet_blocking(&home, DEFAULT_FUND_AMOUNT_SATS)
            .expect_err("must refuse nested block_on");
        let message = err.to_string();
        assert!(
            message.contains("nested block_on refused"),
            "unexpected: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_open_refuses_inside_runtime() {
        let root = temp_home("nested-open-refuse");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = open_wallet_blocking(&home).expect_err("must refuse nested block_on");
        let message = err.to_string();
        assert!(
            message.contains("nested block_on refused"),
            "unexpected: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn live_testnut_fund_observes_balance_gt_zero() {
        let root = temp_home("live");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        assert_eq!(home.config.default_mint(), DEFAULT_MINT_URL);
        let outcome = fund_wallet_blocking(&home, DEFAULT_FUND_AMOUNT_SATS)
            .expect("live testnut fund");
        assert!(!outcome.already_funded);
        assert!(outcome.balance_sats > 0, "balance={}", outcome.balance_sats);
        assert!(outcome.invoice.as_ref().is_some_and(|invoice| !invoice.is_empty()));
        assert_eq!(outcome.mint_url, DEFAULT_MINT_URL);

        // Idempotent second call — balance still visible, no double-fund required.
        let again = fund_wallet_blocking(&home, DEFAULT_FUND_AMOUNT_SATS)
            .expect("already funded");
        assert!(again.already_funded);
        assert!(again.balance_sats > 0);
    }

    #[test]
    fn default_config_is_testnut() {
        assert_eq!(MobeeConfig::default().default_mint(), DEFAULT_MINT_URL);
    }
}
