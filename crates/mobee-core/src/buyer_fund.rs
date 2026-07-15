//! Buyer wallet fund path for packaged `~/.mobee` (testnut only).
//!
//! Flow: mint quote → (testnut FakeWallet auto-marks paid) → mint.
//! Mint URL is hard-pinned to [`crate::home::DEFAULT_MINT_URL`].

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod};
use cdk::wallet::Wallet;
use cdk::Amount;
use cdk_sqlite::wallet::WalletSqliteDatabase;
use sha2::{Digest, Sha256};

use crate::home::{self, HomeError, MobeeHome, DEFAULT_MINT_URL};

/// Default fund amount for first-run setup (sats). Small; testnut only.
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
    MintPinned { configured: String },
    Wallet(String),
}

impl std::fmt::Display for FundError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Home(error) => write!(formatter, "{error}"),
            Self::MintPinned { configured } => write!(
                formatter,
                "fund path hard-pinned to {DEFAULT_MINT_URL}; configured mint_url is {configured}"
            ),
            Self::Wallet(message) => write!(formatter, "wallet fund error: {message}"),
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

fn require_testnut_mint(home: &MobeeHome) -> Result<(), FundError> {
    if home.config.mint_url != DEFAULT_MINT_URL {
        return Err(FundError::MintPinned {
            configured: home.config.mint_url.clone(),
        });
    }
    Ok(())
}

fn sqlite_path(wallet_dir: &Path) -> std::path::PathBuf {
    wallet_dir.join("cdk-wallet.sqlite")
}

async fn open_wallet(home: &MobeeHome) -> Result<Wallet, FundError> {
    require_testnut_mint(home)?;
    let secret = home::read_secret_key_hex(home)?;
    let seed = seed_from_secret_hex(&secret)?;
    let path = sqlite_path(&home.wallet_dir);
    let store = WalletSqliteDatabase::new(path)
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    Wallet::new(
        DEFAULT_MINT_URL,
        CurrencyUnit::Sat,
        Arc::new(store),
        seed,
        None,
    )
    .map_err(|error| FundError::Wallet(error.to_string()))
}

/// Read current wallet balance against the hard-pinned testnut mint.
pub async fn wallet_balance_sats(home: &MobeeHome) -> Result<u64, FundError> {
    let wallet = open_wallet(home).await?;
    let balance = wallet
        .total_balance()
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    Ok(balance.to_u64())
}

/// Fund via mint-quote → wait (testnut auto-pay) → mint. Idempotent if balance > 0.
pub async fn fund_testnut_wallet(
    home: &MobeeHome,
    amount_sats: u64,
) -> Result<FundOutcome, FundError> {
    require_testnut_mint(home)?;
    let wallet = open_wallet(home).await?;
    let existing = wallet
        .total_balance()
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?
        .to_u64();
    if existing > 0 {
        return Ok(FundOutcome {
            mint_url: DEFAULT_MINT_URL.to_owned(),
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

    // Poll HTTP quote status — do not use wait_and_mint_quote (WS stream hung against
    // testnut in this environment). FakeWallet marks bolt11 paid; poll until Paid/Issued.
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
                    return Err(FundError::Wallet(format!(
                        "timed out waiting for testnut mint quote {quote_id} to become paid"
                    )));
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
        mint_url: DEFAULT_MINT_URL.to_owned(),
        invoice: Some(invoice),
        funded_sats: funded,
        balance_sats: balance,
        already_funded: false,
    })
}

/// Blocking wrapper for MCP / CLI (current-thread runtime).
pub fn fund_testnut_wallet_blocking(
    home: &MobeeHome,
    amount_sats: u64,
) -> Result<FundOutcome, FundError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    runtime.block_on(fund_testnut_wallet(home, amount_sats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::{bootstrap, MobeeConfig};
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

    #[test]
    fn non_testnut_mint_is_refused() {
        let root = temp_home("pin");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        home.config.mint_url = "https://evil.example".into();
        let err = fund_testnut_wallet_blocking(&home, 1).expect_err("pin");
        assert!(matches!(err, FundError::MintPinned { .. }));
        assert!(err.to_string().contains(DEFAULT_MINT_URL));
    }

    #[test]
    fn live_testnut_fund_observes_balance_gt_zero() {
        let root = temp_home("live");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        assert_eq!(home.config.mint_url, DEFAULT_MINT_URL);
        let outcome = fund_testnut_wallet_blocking(&home, DEFAULT_FUND_AMOUNT_SATS)
            .expect("live testnut fund");
        assert!(!outcome.already_funded);
        assert!(outcome.balance_sats > 0, "balance={}", outcome.balance_sats);
        assert!(outcome.invoice.as_ref().is_some_and(|invoice| !invoice.is_empty()));
        assert_eq!(outcome.mint_url, DEFAULT_MINT_URL);

        // Idempotent second call — balance still visible, no double-fund required.
        let again = fund_testnut_wallet_blocking(&home, DEFAULT_FUND_AMOUNT_SATS)
            .expect("already funded");
        assert!(again.already_funded);
        assert!(again.balance_sats > 0);
    }

    #[test]
    fn default_config_is_testnut() {
        assert_eq!(MobeeConfig::default().mint_url, DEFAULT_MINT_URL);
    }
}
