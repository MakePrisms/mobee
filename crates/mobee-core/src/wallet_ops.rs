//! Flexible ecash wallet ops for `mobee wallet` / MCP mirrors.
//!
//! Additive surface over the packaged CDK wallet at `home/.mobee/wallet`.
//! Does **not** replace the toy [`crate::buyer_fund::fund_testnut_wallet`] path
//! (`setup_wallet` keeps hardcoded 21 + `already_funded`).

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use cashu::{MintUrl, Token};
use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use cdk::Amount;
use cdk_sqlite::wallet::WalletSqliteDatabase;

use crate::buyer_fund::seed_from_secret_hex;
use crate::home::{self, HomeError, MobeeHome, DEFAULT_MINT_URL};

#[derive(Debug)]
pub enum WalletOpsError {
    Home(HomeError),
    MintNotAllowed { mint_url: String },
    MintPinnedDefault,
    Wallet(String),
}

impl std::fmt::Display for WalletOpsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Home(error) => write!(formatter, "{error}"),
            Self::MintNotAllowed { mint_url } => write!(
                formatter,
                "mint {mint_url} is not configured; add it with `mobee wallet mints add` (default stays {DEFAULT_MINT_URL})"
            ),
            Self::MintPinnedDefault => write!(
                formatter,
                "cannot remove the default mint ({DEFAULT_MINT_URL}); only extra_mints are removable"
            ),
            Self::Wallet(message) => write!(formatter, "wallet error: {message}"),
        }
    }
}

impl std::error::Error for WalletOpsError {}

impl From<HomeError> for WalletOpsError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintBalance {
    pub mint_url: String,
    pub balance_sats: u64,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintOutcome {
    pub mint_url: String,
    pub invoice: String,
    pub funded_sats: u64,
    pub balance_sats: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOutcome {
    pub mint_url: String,
    pub sent_sats: u64,
    pub balance_sats: u64,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveOutcome {
    pub mint_url: String,
    pub received_sats: u64,
    pub balance_sats: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeltOutcome {
    pub mint_url: String,
    pub paid_sats: u64,
    pub fee_sats: u64,
    pub balance_sats: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceOutcome {
    pub mint_url: String,
    pub invoice: String,
    pub amount_sats: u64,
    pub funded_sats: u64,
    pub balance_sats: u64,
}

fn sqlite_path(wallet_dir: &Path) -> std::path::PathBuf {
    wallet_dir.join("cdk-wallet.sqlite")
}

fn normalize_mint_url(raw: &str) -> Result<String, WalletOpsError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(WalletOpsError::Wallet("mint URL is empty".into()));
    }
    let parsed = MintUrl::from_str(trimmed)
        .map_err(|error| WalletOpsError::Wallet(format!("invalid mint URL: {error}")))?;
    Ok(parsed.to_string())
}

/// Configured mints: default `mint_url` first, then opt-in `extra_mints` (deduped).
pub fn configured_mints(home: &MobeeHome) -> Result<Vec<String>, WalletOpsError> {
    let mut out = Vec::new();
    let default = normalize_mint_url(&home.config.mint_url)?;
    out.push(default.clone());
    for extra in &home.config.extra_mints {
        let normalized = normalize_mint_url(extra)?;
        if !out.iter().any(|existing| existing == &normalized) {
            out.push(normalized);
        }
    }
    Ok(out)
}

fn mint_is_allowed(home: &MobeeHome, mint_url: &str) -> Result<String, WalletOpsError> {
    let normalized = normalize_mint_url(mint_url)?;
    let allowed = configured_mints(home)?;
    if allowed.iter().any(|entry| entry == &normalized) {
        Ok(normalized)
    } else {
        Err(WalletOpsError::MintNotAllowed {
            mint_url: normalized,
        })
    }
}

fn resolve_mint(home: &MobeeHome, mint_override: Option<&str>) -> Result<String, WalletOpsError> {
    match mint_override {
        Some(url) => mint_is_allowed(home, url),
        None => normalize_mint_url(&home.config.mint_url),
    }
}

/// Open the packaged CDK wallet for one allowed mint (shared sqlite + seed).
pub async fn open_wallet_async(
    home: &MobeeHome,
    mint_url: &str,
) -> Result<Wallet, WalletOpsError> {
    let mint_url = mint_is_allowed(home, mint_url)?;
    let secret = home::read_secret_key_hex(home)?;
    let seed = seed_from_secret_hex(&secret).map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let path = sqlite_path(&home.wallet_dir);
    let store = WalletSqliteDatabase::new(path)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    Wallet::new(
        mint_url.as_str(),
        CurrencyUnit::Sat,
        Arc::new(store),
        seed,
        None,
    )
    .map_err(|error| WalletOpsError::Wallet(error.to_string()))
}

async fn poll_and_mint(wallet: &Wallet, quote_id: &str) -> Result<u64, WalletOpsError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let status = wallet
            .check_mint_quote(quote_id)
            .await
            .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
        match status.state {
            MintQuoteState::Paid | MintQuoteState::Issued => break,
            MintQuoteState::Unpaid => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(WalletOpsError::Wallet(format!(
                        "timed out waiting for mint quote {quote_id} to become paid (refusing phantom credit)"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    let proofs = wallet
        .mint(quote_id, Default::default(), None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let funded = proofs
        .iter()
        .map(|proof| proof.amount.to_u64())
        .fold(0u64, |acc, value| acc.saturating_add(value));
    if funded == 0 {
        return Err(WalletOpsError::Wallet(
            "mint completed but funded amount is 0 (refusing phantom credit)".into(),
        ));
    }
    Ok(funded)
}

/// Balance per configured mint (default + extras).
pub async fn balances_async(home: &MobeeHome) -> Result<Vec<MintBalance>, WalletOpsError> {
    let default = normalize_mint_url(&home.config.mint_url)?;
    let mut rows = Vec::new();
    for mint_url in configured_mints(home)? {
        let wallet = open_wallet_async(home, &mint_url).await?;
        let balance = wallet
            .total_balance()
            .await
            .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
            .to_u64();
        rows.push(MintBalance {
            is_default: mint_url == default,
            mint_url,
            balance_sats: balance,
        });
    }
    Ok(rows)
}

/// Flexible/repeatable mint-fund (no `already_funded` hard-block).
pub async fn mint_async(
    home: &MobeeHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintOutcome, WalletOpsError> {
    if amount_sats == 0 {
        return Err(WalletOpsError::Wallet("amount must be > 0".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    let wallet = open_wallet_async(home, &mint_url).await?;
    let amount = Amount::from(amount_sats);
    let quote = wallet
        .mint_quote(PaymentMethod::BOLT11, Some(amount), None, None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let invoice = quote.request.clone();
    let quote_id = quote.id.clone();
    let funded = poll_and_mint(&wallet, &quote_id).await?;
    let balance = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    Ok(MintOutcome {
        mint_url,
        invoice,
        funded_sats: funded,
        balance_sats: balance,
    })
}

/// Create a bolt11 invoice and mint once paid (testnut FakeWallet auto-pays).
pub async fn invoice_async(
    home: &MobeeHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<InvoiceOutcome, WalletOpsError> {
    let minted = mint_async(home, amount_sats, mint_override).await?;
    Ok(InvoiceOutcome {
        mint_url: minted.mint_url,
        invoice: minted.invoice,
        amount_sats,
        funded_sats: minted.funded_sats,
        balance_sats: minted.balance_sats,
    })
}

/// Create/print an unlocked cashu token (ecash out).
pub async fn send_async(
    home: &MobeeHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<SendOutcome, WalletOpsError> {
    if amount_sats == 0 {
        return Err(WalletOpsError::Wallet("amount must be > 0".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    let wallet = open_wallet_async(home, &mint_url).await?;
    let before = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    if before < amount_sats {
        return Err(WalletOpsError::Wallet(format!(
            "insufficient funds: balance={before} need={amount_sats}"
        )));
    }
    let prepared = wallet
        .prepare_send(Amount::from(amount_sats), SendOptions::default())
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let token = prepared
        .confirm(None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let balance = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    if balance >= before {
        return Err(WalletOpsError::Wallet(format!(
            "send did not move ecash: balance before={before} after={balance}"
        )));
    }
    Ok(SendOutcome {
        mint_url,
        sent_sats: amount_sats,
        balance_sats: balance,
        token: token.to_string(),
    })
}

/// Redeem a cashu token (ecash in). Mint must already be configured.
pub async fn receive_async(
    home: &MobeeHome,
    token: &str,
) -> Result<ReceiveOutcome, WalletOpsError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(WalletOpsError::Wallet("token is empty".into()));
    }
    let parsed = Token::from_str(token)
        .map_err(|error| WalletOpsError::Wallet(format!("invalid cashu token: {error}")))?;
    let mint_url = parsed
        .mint_url()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_string();
    let mint_url = mint_is_allowed(home, &mint_url)?;
    let wallet = open_wallet_async(home, &mint_url).await?;
    let before = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    let received = wallet
        .receive(token, ReceiveOptions::default())
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let received_sats = received.to_u64();
    if received_sats == 0 {
        return Err(WalletOpsError::Wallet(
            "receive credited 0 sats (refusing phantom credit)".into(),
        ));
    }
    let balance = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    if balance <= before {
        return Err(WalletOpsError::Wallet(format!(
            "receive did not increase balance: before={before} after={balance}"
        )));
    }
    Ok(ReceiveOutcome {
        mint_url,
        received_sats,
        balance_sats: balance,
    })
}

/// Pay a lightning invoice from ecash (fail-closed on insufficient / unpaid).
pub async fn melt_async(
    home: &MobeeHome,
    bolt11: &str,
    mint_override: Option<&str>,
) -> Result<MeltOutcome, WalletOpsError> {
    let bolt11 = bolt11.trim();
    if bolt11.is_empty() {
        return Err(WalletOpsError::Wallet("bolt11 invoice is empty".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    let wallet = open_wallet_async(home, &mint_url).await?;
    let quote = wallet
        .melt_quote(PaymentMethod::BOLT11, bolt11, None, None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let need = quote.amount.to_u64().saturating_add(quote.fee_reserve.to_u64());
    let balance = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    if balance < need {
        return Err(WalletOpsError::Wallet(format!(
            "insufficient funds for melt: balance={balance} need={need} (amount+fee_reserve)"
        )));
    }
    let prepared = wallet
        .prepare_melt(&quote.id, HashMap::new())
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let confirmed = prepared
        .confirm()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let paid_sats = confirmed.amount().to_u64();
    let fee_sats = confirmed.fee_paid().to_u64();
    let balance_sats = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    Ok(MeltOutcome {
        mint_url,
        paid_sats,
        fee_sats,
        balance_sats,
    })
}

/// List configured mints (default first).
pub fn list_mints(home: &MobeeHome) -> Result<Vec<MintBalance>, WalletOpsError> {
    let default = normalize_mint_url(&home.config.mint_url)?;
    Ok(configured_mints(home)?
        .into_iter()
        .map(|mint_url| MintBalance {
            is_default: mint_url == default,
            mint_url,
            balance_sats: 0,
        })
        .collect())
}

/// Opt-in add of an extra mint URL (does not invent balance).
pub fn add_mint(home: &mut MobeeHome, mint_url: &str) -> Result<String, WalletOpsError> {
    let normalized = normalize_mint_url(mint_url)?;
    let default = normalize_mint_url(&home.config.mint_url)?;
    if normalized == default {
        return Ok(normalized);
    }
    if home
        .config
        .extra_mints
        .iter()
        .any(|entry| normalize_mint_url(entry).ok().as_deref() == Some(normalized.as_str()))
    {
        return Ok(normalized);
    }
    home.config.extra_mints.push(normalized.clone());
    home::save_config(home)?;
    Ok(normalized)
}

/// Remove an opt-in extra mint. Default mint is pinned and cannot be removed.
pub fn remove_mint(home: &mut MobeeHome, mint_url: &str) -> Result<(), WalletOpsError> {
    let normalized = normalize_mint_url(mint_url)?;
    let default = normalize_mint_url(&home.config.mint_url)?;
    if normalized == default {
        return Err(WalletOpsError::MintPinnedDefault);
    }
    let before = home.config.extra_mints.len();
    home.config.extra_mints.retain(|entry| {
        normalize_mint_url(entry)
            .ok()
            .as_deref()
            != Some(normalized.as_str())
    });
    if home.config.extra_mints.len() == before {
        return Err(WalletOpsError::MintNotAllowed {
            mint_url: normalized,
        });
    }
    home::save_config(home)?;
    Ok(())
}

pub fn balances_blocking(home: &MobeeHome) -> Result<Vec<MintBalance>, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("balances_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(balances_async(home))
}

pub fn mint_blocking(
    home: &MobeeHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("mint_blocking").map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(mint_async(home, amount_sats, mint_override))
}

pub fn send_blocking(
    home: &MobeeHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<SendOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("send_blocking").map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(send_async(home, amount_sats, mint_override))
}

pub fn receive_blocking(
    home: &MobeeHome,
    token: &str,
) -> Result<ReceiveOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("receive_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(receive_async(home, token))
}

pub fn melt_blocking(
    home: &MobeeHome,
    bolt11: &str,
    mint_override: Option<&str>,
) -> Result<MeltOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("melt_blocking").map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(melt_async(home, bolt11, mint_override))
}

pub fn invoice_blocking(
    home: &MobeeHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<InvoiceOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("invoice_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(invoice_async(home, amount_sats, mint_override))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::bootstrap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-wallet-ops-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn extra_mint_add_remove_keeps_default_pinned() {
        let root = temp_home("mints");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        assert_eq!(home.config.mint_url, DEFAULT_MINT_URL);
        let listed = list_mints(&home).expect("list");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_default);

        let added = add_mint(&mut home, "https://example.mint.test").expect("add");
        assert_eq!(added, "https://example.mint.test");
        assert_eq!(list_mints(&home).expect("list2").len(), 2);

        let err = remove_mint(&mut home, DEFAULT_MINT_URL).expect_err("pin");
        assert!(matches!(err, WalletOpsError::MintPinnedDefault));

        remove_mint(&mut home, "https://example.mint.test").expect("remove");
        assert_eq!(list_mints(&home).expect("list3").len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_mint_refuses_inside_runtime() {
        let root = temp_home("nested");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = mint_blocking(&home, 1, None).expect_err("nested");
        assert!(err.to_string().contains("nested block_on refused"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_mint_refused_without_inventing_credit() {
        let root = temp_home("unknown");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = mint_blocking(&home, 1, Some("https://evil.example")).expect_err("deny");
        assert!(matches!(err, WalletOpsError::MintNotAllowed { .. }));
        let _ = std::fs::remove_dir_all(&root);
    }
}
