//! Seller daemon primitives: rate-gate, journal (pay-once), ★1059 unwrap, pay bind.
//!
//! Shape-locked (Scribe SoT + Temper B1/B2):
//! - `rate_sats` is CLAIM-FLOOR only (`offer.amount ≥ rate_sats`)
//! - receive asserts `Amount == offer.amount` via lifted `terms_for_offer`
//! - bind `job_id`(+`result_id`) before journaling; wrong-job → refuse
//! - ★1059: unwrap ONLY `p-tag==self`, exactly ONE decode site, never log plaintext

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::gateway::ParsedOffer;
use crate::home::{HomeError, MobeeHome, SellerConfig};

const JOURNAL_FILE: &str = "seller-journal.jsonl";
pub const DEFAULT_JOB_TIMEOUT_SECS: u64 = 600;

#[derive(Debug)]
pub enum SellerError {
    Config(String),
    Io(String),
    Journal(String),
    Policy(String),
    Payment(String),
    Home(HomeError),
}

impl std::fmt::Display for SellerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(message) => write!(f, "seller config error: {message}"),
            Self::Io(message) => write!(f, "seller io error: {message}"),
            Self::Journal(message) => write!(f, "seller journal error: {message}"),
            Self::Policy(message) => write!(f, "seller policy refused: {message}"),
            Self::Payment(message) => write!(f, "seller payment error: {message}"),
            Self::Home(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SellerError {}

impl From<HomeError> for SellerError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}

/// Require the three locked `[seller]` fields (named fail-closed).
pub fn require_seller_config(home: &MobeeHome) -> Result<&SellerConfig, SellerError> {
    let seller = home.config.seller.as_ref().ok_or_else(|| {
        SellerError::Config("missing required [seller] section (agent_command, rate_sats, git_remote)".into())
    })?;
    if seller.agent_command.is_empty() {
        return Err(SellerError::Config(
            "missing required field agent_command (argv array)".into(),
        ));
    }
    if seller.agent_command.iter().any(|part| part.is_empty()) {
        return Err(SellerError::Config(
            "agent_command argv entries must be non-empty".into(),
        ));
    }
    if seller.git_remote.trim().is_empty() {
        return Err(SellerError::Config(
            "missing required field git_remote".into(),
        ));
    }
    // rate_sats: 0 is allowed (accept any amount ≥ 0) but field must be present (serde).
    Ok(seller)
}

/// B1 claim-floor: targeted-to-self AND `offer.amount ≥ rate_sats`.
///
/// Untargeted/open offers refuse by default. Pass `claim_open_pool = true` (explicit
/// seller opt-in) to allow untargeted offers that still clear the rate floor.
pub fn rate_gate_allows(
    offer: &ParsedOffer,
    seller_pubkey: &str,
    rate_sats: u64,
    claim_open_pool: bool,
) -> Result<(), SellerError> {
    match offer.seller_pubkey.as_deref() {
        None if claim_open_pool => {}
        None => {
            return Err(SellerError::Policy(
                "untargeted offer refused (seller claims only p-tag==self; set claim_open_pool=true to opt in)".into(),
            ));
        }
        Some(target) if target != seller_pubkey => {
            return Err(SellerError::Policy(format!(
                "offer targets seller {target}, not {seller_pubkey}"
            )));
        }
        Some(_) => {}
    }
    if offer.amount < rate_sats {
        return Err(SellerError::Policy(format!(
            "offer amount {} sat below seller rate_sats {rate_sats}",
            offer.amount
        )));
    }
    Ok(())
}

/// Job deadline: config override, else offer deadline, else now+DEFAULT.
pub fn job_deadline_unix(offer: &ParsedOffer, seller: &SellerConfig, now_unix: u64) -> u64 {
    if let Some(secs) = seller.job_timeout_secs {
        return now_unix.saturating_add(secs);
    }
    if offer.deadline_unix > now_unix {
        return offer.deadline_unix;
    }
    now_unix.saturating_add(DEFAULT_JOB_TIMEOUT_SECS)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalEntry {
    Claim {
        job_id: String,
        ts: u64,
    },
    Receipt {
        job_id: String,
        result_id: String,
        amount_received: u64,
        mint: String,
        buyer: String,
        swap_ok: bool,
        ts: u64,
    },
}

/// Append-only seller journal — claim dedup + pay-once receipt evidence.
pub struct SellerJournal {
    path: PathBuf,
}

impl SellerJournal {
    pub fn open(home: &MobeeHome) -> Result<Self, SellerError> {
        let path = home.root.join(JOURNAL_FILE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| SellerError::Io(error.to_string()))?;
        }
        if !path.exists() {
            File::create(&path).map_err(|error| SellerError::Io(error.to_string()))?;
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> Result<Vec<JournalEntry>, SellerError> {
        let file = File::open(&self.path).map_err(|error| SellerError::Io(error.to_string()))?;
        let mut out = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|error| SellerError::Io(error.to_string()))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let entry: JournalEntry = serde_json::from_str(trimmed)
                .map_err(|error| SellerError::Journal(format!("corrupt journal line: {error}")))?;
            out.push(entry);
        }
        Ok(out)
    }

    pub fn has_claim(&self, job_id: &str) -> Result<bool, SellerError> {
        Ok(self.entries()?.into_iter().any(|entry| match entry {
            JournalEntry::Claim { job_id: id, .. } => id == job_id,
            JournalEntry::Receipt { job_id: id, .. } => id == job_id,
        }))
    }

    pub fn has_receipt(&self, job_id: &str) -> Result<bool, SellerError> {
        Ok(self.entries()?.into_iter().any(|entry| matches!(
            entry,
            JournalEntry::Receipt { job_id: id, .. } if id == job_id
        )))
    }

    pub fn append_claim(&self, job_id: &str) -> Result<(), SellerError> {
        if self.has_claim(job_id)? {
            return Err(SellerError::Journal(format!(
                "job {job_id} already claimed (dedup)"
            )));
        }
        self.append(JournalEntry::Claim {
            job_id: job_id.to_owned(),
            ts: now_unix(),
        })
    }

    /// B2: bind payload job_id(+result_id) to the local claim/result BEFORE journaling.
    pub fn append_receipt(
        &self,
        local_job_id: &str,
        local_result_id: &str,
        payload_job_id: &str,
        payload_result_id: &str,
        amount_received: u64,
        expected_amount: u64,
        mint: &str,
        buyer: &str,
        swap_ok: bool,
    ) -> Result<(), SellerError> {
        if payload_job_id != local_job_id || payload_result_id != local_result_id {
            return Err(SellerError::Policy(format!(
                "payment bind refused: payload job/result ({payload_job_id}/{payload_result_id}) != local ({local_job_id}/{local_result_id})"
            )));
        }
        if amount_received != expected_amount {
            return Err(SellerError::Policy(format!(
                "receipt amount_received {amount_received} != offer.amount {expected_amount}"
            )));
        }
        if self.has_receipt(local_job_id)? {
            return Err(SellerError::Journal(format!(
                "job {local_job_id} already receipted (pay-once)"
            )));
        }
        self.append(JournalEntry::Receipt {
            job_id: local_job_id.to_owned(),
            result_id: local_result_id.to_owned(),
            amount_received,
            mint: mint.to_owned(),
            buyer: buyer.to_owned(),
            swap_ok,
            ts: now_unix(),
        })
    }

    fn append(&self, entry: JournalEntry) -> Result<(), SellerError> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| SellerError::Io(error.to_string()))?;
        let line = serde_json::to_string(&entry)
            .map_err(|error| SellerError::Journal(error.to_string()))?;
        // Journal lines carry ids/amounts/mint/buyer/swap_ok/ts only — never token material.
        writeln!(file, "{line}").map_err(|error| SellerError::Io(error.to_string()))?;
        file.sync_all()
            .map_err(|error| SellerError::Io(error.to_string()))?;
        Ok(())
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Sign a receipt/job hash with the seller nostr key (schnorr hex).
#[cfg(feature = "gateway")]
pub fn sign_receipt_hash(
    keys: &nostr_sdk::prelude::Keys,
    job_hash: &str,
) -> Result<String, SellerError> {
    use nostr_sdk::secp256k1::Message;

    let bytes: [u8; 32] = hex::decode(job_hash)
        .map_err(|error| SellerError::Payment(format!("invalid receipt hash bytes: {error}")))?
        .try_into()
        .map_err(|_| SellerError::Payment("invalid receipt hash length".into()))?;
    Ok(keys
        .sign_schnorr(&Message::from_digest(bytes))
        .to_string())
}

/// ★1059 ONE decode site: unwrap gift-wrap ONLY when `p-tag==self`.
///
/// Returns `Ok(None)` when the wrap is not addressed to us or fails decrypt (not an error —
/// other parties' envelopes stay dark). Decrypted contents are NEVER logged.
#[cfg(all(feature = "gateway", feature = "wallet"))]
pub async fn unwrap_own_payment_gift_wrap(
    keys: &nostr_sdk::prelude::Keys,
    event: &nostr_sdk::prelude::Event,
) -> Result<Option<crate::payment_send::ReceivedPayment>, SellerError> {
    use nostr_sdk::prelude::{Kind, UnwrappedGift};

    if event.kind != Kind::GiftWrap {
        return Ok(None);
    }
    let self_pk = keys.public_key();
    // ★1059 condition: p-tag == self only (exact ONE decode site below).
    let addressed_to_self = event.tags.iter().any(|tag| {
        let slice = tag.as_slice();
        slice.first().map(String::as_str) == Some("p")
            && slice
                .get(1)
                .map(|value| value == &self_pk.to_hex() || value == &self_pk.to_string())
                .unwrap_or(false)
    });
    if !addressed_to_self {
        return Ok(None);
    }

    let unwrapped = match UnwrappedGift::from_gift_wrap(keys, event).await {
        Ok(unwrapped) => unwrapped,
        Err(_) => return Ok(None),
    };
    // Do not log unwrapped.rumor.content — token material lives there.
    match crate::payment_send::parse_nip17_payment_payload(&unwrapped.rumor.content, unwrapped.sender)
    {
        Ok(received) => Ok(Some(received)),
        Err(_) => Ok(None),
    }
}

/// Derive the cashu P2PK signing key from the packaged nostr secret hex.
///
/// Buyer locks to BIP-340 even-y (`02||xonly`) because nostr only carries the
/// 32-byte x-only coordinate. A raw secp secret may derive an odd-y (`03`)
/// compressed pubkey — that key cannot witness a `02`-locked P2PK proof
/// (Inputs:0). Normalize here: if the derived pubkey is odd-parity, negate the
/// scalar (`d' = n − d`) so the signing keypair is even-y and matches the lock.
#[cfg(feature = "wallet")]
pub fn cashu_secret_from_nostr_hex(secret_hex: &str) -> Result<cashu::SecretKey, SellerError> {
    use std::ops::Deref;

    let bytes = hex::decode(secret_hex.trim())
        .map_err(|error| SellerError::Payment(format!("secret hex decode: {error}")))?;
    let key = cashu::SecretKey::from_slice(&bytes)
        .map_err(|error| SellerError::Payment(format!("cashu secret key: {error}")))?;
    // Compressed form: 0x02 = even-y, 0x03 = odd-y (BIP-340 / BIP-340-compatible).
    if key.public_key().to_bytes()[0] == 0x03 {
        Ok(cashu::SecretKey::from((*key.deref()).negate()))
    } else {
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ParsedOffer;
    use crate::home::{self, MobeeConfig, SellerConfig};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-seller-{label}-{}-{id}",
            std::process::id()
        ))
    }

    fn offer(amount: u64, seller: Option<&str>) -> ParsedOffer {
        ParsedOffer {
            task: "task".into(),
            output: "text/plain".into(),
            amount,
            unit: "sat".into(),
            deadline_unix: 2_000_000_000,
            mint_url: crate::home::DEFAULT_MINT_URL.into(),
            seller_pubkey: seller.map(str::to_owned),
        }
    }

    #[test]
    fn rate_gate_floor_allows_above_rate_refuses_below_and_untargeted() {
        let seller = "aa".repeat(32);
        rate_gate_allows(&offer(5, Some(&seller)), &seller, 3, false).expect("above floor");
        rate_gate_allows(&offer(3, Some(&seller)), &seller, 3, false).expect("equal floor");
        assert!(rate_gate_allows(&offer(2, Some(&seller)), &seller, 3, false).is_err());
        assert!(rate_gate_allows(&offer(9, None), &seller, 1, false).is_err());
        rate_gate_allows(&offer(9, None), &seller, 1, true).expect("open-pool opt-in");
        assert!(rate_gate_allows(&offer(0, None), &seller, 1, true).is_err());
    }

    #[test]
    fn agent_command_string_refused_at_config_parse() {
        let raw = r#"
relay_url = "wss://example.invalid"
mint_url = "https://testnut.cashudevkit.org"
per_job_budget_sats = 21
total_budget_sats = 100

[seller]
agent_command = "claude --print"
rate_sats = 1
git_remote = "https://example.invalid/repo.git"
"#;
        let err = toml::from_str::<MobeeConfig>(raw).expect_err("string argv must refuse");
        assert!(
            err.to_string().contains("argv array") || err.to_string().contains("agent_command"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn agent_command_argv_array_parses() {
        let raw = r#"
relay_url = "wss://example.invalid"
mint_url = "https://testnut.cashudevkit.org"
per_job_budget_sats = 21
total_budget_sats = 100

[seller]
agent_command = ["claude", "--print"]
rate_sats = 7
git_remote = "https://example.invalid/repo.git"
"#;
        let config: MobeeConfig = toml::from_str(raw).expect("parse");
        let seller = config.seller.expect("seller");
        assert_eq!(seller.agent_command, vec!["claude", "--print"]);
        assert_eq!(seller.rate_sats, 7);
    }

    #[test]
    fn journal_pay_once_and_bind_refuse_wrong_job() {
        let root = temp_home("journal");
        let _ = fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home.config.seller = Some(SellerConfig {
            agent_command: vec!["echo".into()],
            rate_sats: 1,
            git_remote: "https://example.invalid/repo.git".into(),
            job_timeout_secs: None,
            agent: None,
            claim_open_pool: false,
        });
        home::save_config(&home).expect("save");

        let journal = SellerJournal::open(&home).expect("journal");
        journal.append_claim("job-a").expect("claim");
        assert!(journal.append_claim("job-a").is_err());

        let refused = journal.append_receipt(
            "job-a",
            "result-a",
            "job-OTHER",
            "result-a",
            7,
            7,
            "https://testnut.cashudevkit.org",
            "buyer",
            true,
        );
        assert!(refused.is_err(), "wrong-job must refuse");

        journal
            .append_receipt(
                "job-a",
                "result-a",
                "job-a",
                "result-a",
                7,
                7,
                "https://testnut.cashudevkit.org",
                "buyer",
                true,
            )
            .expect("receipt");
        assert!(
            journal
                .append_receipt(
                    "job-a",
                    "result-a",
                    "job-a",
                    "result-a",
                    7,
                    7,
                    "https://testnut.cashudevkit.org",
                    "buyer",
                    true,
                )
                .is_err(),
            "pay-once"
        );

        let raw = fs::read_to_string(journal.path()).expect("read");
        // Mint host may contain "cashu" (testnut.cashudevkit.org); refuse token/payload fields.
        assert!(
            !raw.contains("cashuA") && !raw.contains("\"token\"") && !raw.contains("nsec"),
            "journal must never hold token/key material: {raw}"
        );
        assert!(raw.contains("amount_received"));
    }

    #[test]
    fn receipt_amount_must_equal_offer_amount_not_rate() {
        let root = temp_home("amount");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let journal = SellerJournal::open(&home).expect("journal");
        journal.append_claim("job-b").expect("claim");
        // offer.amount=21, rate would have been 1 — journal expects offer.amount
        let err = journal
            .append_receipt(
                "job-b",
                "result-b",
                "job-b",
                "result-b",
                1,
                21,
                "https://testnut.cashudevkit.org",
                "buyer",
                true,
            )
            .expect_err("amount_received must == offer.amount");
        assert!(err.to_string().contains("offer.amount"));
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn cashu_secret_from_nostr_hex_normalizes_odd_y_to_even() {
        // Find a raw secret whose uncompressed/compressed pubkey is odd-y (03…).
        let odd_raw = (1u8..=u8::MAX)
            .map(|byte| [byte; 32])
            .find(|bytes| {
                cashu::SecretKey::from_slice(bytes)
                    .map(|key| key.public_key().to_bytes()[0] == 0x03)
                    .unwrap_or(false)
            })
            .expect("an odd-parity secp secret exists");
        let raw_key = cashu::SecretKey::from_slice(&odd_raw).expect("parse");
        assert_eq!(raw_key.public_key().to_bytes()[0], 0x03, "precondition: odd-y");
        let xonly_before = raw_key.public_key().x_only_public_key().to_string();

        let normalized =
            cashu_secret_from_nostr_hex(&hex::encode(odd_raw)).expect("normalize");
        assert_eq!(
            normalized.public_key().to_bytes()[0],
            0x02,
            "must force even-y compressed pubkey"
        );
        assert_eq!(
            normalized.public_key().x_only_public_key().to_string(),
            xonly_before,
            "x-only coordinate must be unchanged by BIP-340 negation"
        );
        // Matches buyer lock shape: 02||xonly.
        assert_eq!(
            normalized.public_key().to_hex(),
            format!("02{xonly_before}")
        );
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn cashu_secret_from_nostr_hex_leaves_even_y_unchanged() {
        let even_raw = (1u8..=u8::MAX)
            .map(|byte| [byte; 32])
            .find(|bytes| {
                cashu::SecretKey::from_slice(bytes)
                    .map(|key| key.public_key().to_bytes()[0] == 0x02)
                    .unwrap_or(false)
            })
            .expect("an even-parity secp secret exists");
        let raw_key = cashu::SecretKey::from_slice(&even_raw).expect("parse");
        let before = raw_key.public_key().to_hex();
        let normalized =
            cashu_secret_from_nostr_hex(&hex::encode(even_raw)).expect("normalize");
        assert_eq!(normalized.public_key().to_hex(), before);
        assert_eq!(normalized.public_key().to_bytes()[0], 0x02);
    }
}
