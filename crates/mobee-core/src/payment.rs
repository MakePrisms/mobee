//! Durable trade-payment state and orchestration contracts.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cashu::{Amount, CurrencyUnit, MintUrl, PublicKey, Token};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::payment_send::PaymentSent;
use crate::wallet::VerifiedPayment;

const ATTEMPT_DOMAIN: &[u8] = b"mobee/v1/payment-attempt";

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Validated market job identifier.
pub struct JobId(String);

impl JobId {
    /// Creates a non-empty job identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, PaymentError> {
        nonempty(value.into(), "job id").map(Self)
    }

    /// Returns the identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Validated result identifier.
pub struct ResultId(String);

impl ResultId {
    /// Creates a non-empty result identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, PaymentError> {
        nonempty(value.into(), "result id").map(Self)
    }

    /// Returns the identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Validated streamed result-content digest.
pub struct ContentHash(String);

impl ContentHash {
    /// Parses a lowercase SHA-256 digest.
    pub fn from_hex(value: impl Into<String>) -> Result<Self, PaymentError> {
        digest_hex(value.into(), "content hash").map(Self)
    }

    /// Returns the digest hex.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Validated streamed job digest.
pub struct JobHash(String);

impl JobHash {
    /// Parses a lowercase SHA-256 digest.
    pub fn from_hex(value: impl Into<String>) -> Result<Self, PaymentError> {
        digest_hex(value.into(), "job hash").map(Self)
    }

    /// Returns the digest hex.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Stable wallet reconciliation identifier for one trade payment.
pub struct AttemptId(String);

impl AttemptId {
    /// Returns the attempt identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn for_key(key: &PaymentKey) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(ATTEMPT_DOMAIN);
        hash_field(&mut hasher, key.job_id.as_str());
        hash_field(&mut hasher, key.result_id.as_str());
        hash_field(&mut hasher, key.content_hash.as_str());
        hash_field(&mut hasher, key.job_hash.as_str());
        hash_field(&mut hasher, &key.seller_pubkey.to_string());
        hash_field(&mut hasher, &key.amount.to_string());
        hash_field(&mut hasher, &key.unit.to_string());
        hash_field(&mut hasher, &key.mint.to_string());
        Self(hex::encode(hasher.finalize()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Typed payment facts shared by lock, verify, send, and journal policy.
pub struct PaymentTerms {
    pub mint: MintUrl,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub seller_pubkey: PublicKey,
}

impl PaymentTerms {
    /// Constructs typed payment terms.
    pub fn new(
        mint: MintUrl,
        amount: Amount,
        unit: CurrencyUnit,
        seller_pubkey: PublicKey,
    ) -> Self {
        Self {
            mint,
            amount,
            unit,
            seller_pubkey,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Canonical eight-field trade-payment identity.
pub struct PaymentKey {
    pub job_id: JobId,
    pub result_id: ResultId,
    pub content_hash: ContentHash,
    pub job_hash: JobHash,
    pub seller_pubkey: PublicKey,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub mint: MintUrl,
}

impl PaymentKey {
    /// Builds a key from trade identity and typed payment terms.
    pub fn new(
        job_id: JobId,
        result_id: ResultId,
        content_hash: ContentHash,
        job_hash: JobHash,
        terms: &PaymentTerms,
    ) -> Self {
        Self {
            job_id,
            result_id,
            content_hash,
            job_hash,
            seller_pubkey: terms.seller_pubkey,
            amount: terms.amount,
            unit: terms.unit.clone(),
            mint: terms.mint.clone(),
        }
    }

    /// Derives the stable reconciliation identifier.
    pub fn attempt_id(&self) -> AttemptId {
        AttemptId::for_key(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Durable metadata for a published receipt.
pub struct ReceiptRecord {
    pub receipt_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
/// Durable five-state trade-payment spine.
pub enum PaymentState {
    Intent {
        attempt_id: AttemptId,
    },
    Locked {
        attempt_id: AttemptId,
    },
    Sent {
        attempt_id: AttemptId,
        payment: PaymentSent,
    },
    ReceiptPublished {
        attempt_id: AttemptId,
        receipt: ReceiptRecord,
    },
    Closed {
        attempt_id: AttemptId,
        receipt: ReceiptRecord,
    },
}

impl PaymentState {
    /// Returns the reconciliation identifier carried by this state.
    pub fn attempt_id(&self) -> &AttemptId {
        match self {
            Self::Intent { attempt_id }
            | Self::Locked { attempt_id }
            | Self::Sent { attempt_id, .. }
            | Self::ReceiptPublished { attempt_id, .. }
            | Self::Closed { attempt_id, .. } => attempt_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// One append-only state record for a payment key.
pub struct PaymentRecord {
    pub key: PaymentKey,
    pub value: PaymentState,
}

/// Pure payment transition reducer.
pub struct PaymentMachine;

impl PaymentMachine {
    /// Folds ordered records into the current state.
    pub fn fold(
        key: &PaymentKey,
        records: &[PaymentRecord],
    ) -> Result<Option<PaymentState>, PaymentError> {
        let mut current = None;
        for record in records {
            if &record.key != key {
                return Err(PaymentError::Refused(
                    "journal guard returned a record for another payment key".into(),
                ));
            }
            current = Some(Self::decide(current.as_ref(), &record.value)?);
        }
        Ok(current)
    }

    /// Validates and applies one state transition.
    pub fn decide(
        current: Option<&PaymentState>,
        next: &PaymentState,
    ) -> Result<PaymentState, PaymentError> {
        let legal = match (current, next) {
            (None, PaymentState::Intent { .. }) => true,
            (
                Some(PaymentState::Intent { attempt_id }),
                PaymentState::Locked { attempt_id: next },
            )
            | (
                Some(PaymentState::Locked { attempt_id }),
                PaymentState::Sent {
                    attempt_id: next, ..
                },
            )
            | (
                Some(PaymentState::Sent { attempt_id, .. }),
                PaymentState::ReceiptPublished {
                    attempt_id: next, ..
                },
            )
            | (
                Some(PaymentState::ReceiptPublished { attempt_id, .. }),
                PaymentState::Closed {
                    attempt_id: next, ..
                },
            ) => attempt_id == next,
            _ => false,
        };
        if !legal {
            return Err(PaymentError::IllegalTransition {
                from: current.map(state_name).unwrap_or("none"),
                to: state_name(next),
            });
        }
        Ok(next.clone())
    }
}

/// Exclusive durable journal access for one payment key.
pub trait PaymentJournalGuard {
    /// Replays all records for the locked key.
    fn replay(&mut self) -> Result<Vec<PaymentRecord>, JournalError>;
    /// Durably syncs replayed records before an effect trusts them.
    fn sync_replay(&mut self) -> Result<(), JournalError>;
    /// Appends and durably syncs one record.
    fn append_sync(&mut self, record: &PaymentRecord) -> Result<(), JournalError>;

    /// Folds the locked key's records into its current state.
    fn current(&mut self) -> Result<Option<PaymentState>, PaymentError> {
        let records = self.replay()?;
        let key = records.first().map(|record| &record.key);
        match key {
            Some(key) => PaymentMachine::fold(key, &records),
            None => Ok(None),
        }
    }
}

/// Journal capable of locking one payment key across an effect.
pub trait PaymentJournal {
    /// Exclusive guard returned for the journal's lifetime.
    type Guard<'a>: PaymentJournalGuard
    where
        Self: 'a;

    /// Acquires exclusive journal access for a payment key.
    fn lock<'a>(&'a self, key: &PaymentKey) -> Result<Self::Guard<'a>, JournalError>;
}

#[derive(Clone, Debug)]
/// Append-only JSONL payment journal.
pub struct FsPaymentJournal {
    path: PathBuf,
    #[cfg(test)]
    parent_sync_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FsPaymentJournal {
    /// Opens a journal at the given path when first locked.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            #[cfg(test)]
            parent_sync_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Returns the journal path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    fn parent_sync_count(&self) -> usize {
        self.parent_sync_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Exclusive file-backed payment journal guard.
pub struct FsPaymentJournalGuard {
    file: File,
    key: PaymentKey,
}

impl PaymentJournal for FsPaymentJournal {
    type Guard<'a> = FsPaymentJournalGuard;

    fn lock<'a>(&'a self, key: &PaymentKey) -> Result<Self::Guard<'a>, JournalError> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.path)?;
        file.lock()?;
        sync_parent_directory(&self.path)?;
        #[cfg(test)]
        self.parent_sync_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(FsPaymentJournalGuard {
            file,
            key: key.clone(),
        })
    }
}

fn sync_parent_directory(path: &Path) -> Result<(), JournalError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()?;
    Ok(())
}

impl PaymentJournalGuard for FsPaymentJournalGuard {
    fn replay(&mut self) -> Result<Vec<PaymentRecord>, JournalError> {
        let mut input = self.file.try_clone()?;
        input.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        input.read_to_end(&mut bytes)?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(JournalError::Corrupt {
                line: bytes.iter().filter(|byte| **byte == b'\n').count() + 1,
                detail: "record is missing its commit newline".into(),
            });
        }
        let reader = BufReader::new(bytes.as_slice());
        let mut records = Vec::new();
        for (index, line) in reader.lines().enumerate() {
            let line = line?;
            let record = serde_json::from_str::<PaymentRecord>(&line).map_err(|error| {
                JournalError::Corrupt {
                    line: index + 1,
                    detail: error.to_string(),
                }
            })?;
            if record.key == self.key {
                records.push(record);
            }
        }
        Ok(records)
    }

    fn sync_replay(&mut self) -> Result<(), JournalError> {
        self.file.sync_all()?;
        Ok(())
    }

    fn append_sync(&mut self, record: &PaymentRecord) -> Result<(), JournalError> {
        if record.key != self.key {
            return Err(JournalError::KeyMismatch);
        }
        serde_json::to_writer(&mut self.file, record)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Receipt authorship and cryptographically validated signer evidence.
pub struct ReceiptEvidence {
    pub receipt_id: String,
    pub author: PublicKey,
    pub valid_signers: Vec<PublicKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Required buyer author and dual receipt signers.
pub struct ReceiptAuthority {
    pub buyer: PublicKey,
    pub seller: PublicKey,
}

impl ReceiptAuthority {
    fn verify(&self, evidence: ReceiptEvidence) -> Result<ReceiptRecord, PaymentError> {
        let buyer_signed = evidence.valid_signers.contains(&self.buyer);
        let seller_signed = evidence.valid_signers.contains(&self.seller);
        if evidence.author != self.buyer || !buyer_signed || !seller_signed {
            return Err(PaymentError::ForgedReceipt);
        }
        Ok(ReceiptRecord {
            receipt_id: evidence.receipt_id,
        })
    }
}

/// Ephemeral wallet lock result; never persisted by the trade journal.
pub struct LockedPayment {
    token: Token,
}

impl LockedPayment {
    /// Wraps the token returned by the wallet reconciliation edge.
    pub fn new(token: Token) -> Self {
        Self { token }
    }

    /// Returns the locked token for verify and send adapters.
    pub fn token(&self) -> &Token {
        &self.token
    }
}

/// Injected wallet, verifier, send, and receipt effects.
pub trait PaymentEffects {
    /// Creates or reconciles the wallet lock for an attempt.
    fn lock_or_reconcile(
        &mut self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
    ) -> Result<LockedPayment, EffectError>;

    /// Produces a fresh typed payment verification.
    fn verify_payment(
        &mut self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
        locked: &LockedPayment,
    ) -> Result<VerifiedPayment, EffectError>;

    /// Sends one newly locked and verified payment.
    fn send_payment(
        &mut self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
        locked: &LockedPayment,
        verified: &VerifiedPayment,
    ) -> Result<PaymentSent, EffectError>;

    /// Publishes or recovers the receipt for a sent payment.
    fn publish_receipt(
        &mut self,
        key: &PaymentKey,
        payment: &PaymentSent,
    ) -> Result<ReceiptEvidence, EffectError>;
}

/// Guarded payment workflow orchestrator.
pub struct PaymentService<'a, J> {
    journal: &'a J,
}

impl<'a, J: PaymentJournal> PaymentService<'a, J> {
    /// Creates a service over a trade journal.
    pub fn new(journal: &'a J) -> Self {
        Self { journal }
    }

    /// Advances a payment as far as its current durable state permits.
    pub fn run<E: PaymentEffects>(
        &self,
        key: &PaymentKey,
        terms: &PaymentTerms,
        authority: &ReceiptAuthority,
        effects: &mut E,
    ) -> Result<PaymentState, PaymentError> {
        require_key_matches_terms(key, terms)?;
        let mut guard = self.journal.lock(key)?;
        let records = guard.replay()?;
        guard.sync_replay()?;
        let mut state = PaymentMachine::fold(key, &records)?;
        let recovered_locked = matches!(state, Some(PaymentState::Locked { .. }));
        let mut locked_payment = None;

        if state.is_none() {
            let intent = PaymentState::Intent {
                attempt_id: key.attempt_id(),
            };
            append_transition(&mut guard, key, state.as_ref(), &intent)?;
            state = Some(intent);
        }

        if let Some(PaymentState::Intent { attempt_id }) = state.clone() {
            locked_payment = Some(effects.lock_or_reconcile(&attempt_id, terms)?);
            let locked = PaymentState::Locked { attempt_id };
            append_transition(&mut guard, key, state.as_ref(), &locked)?;
            state = Some(locked);
        }

        if matches!(state, Some(PaymentState::Locked { .. })) {
            if recovered_locked {
                return Err(PaymentError::AmbiguousSendRefused);
            }
            let attempt_id = state
                .as_ref()
                .expect("locked state exists")
                .attempt_id()
                .clone();
            let locked = locked_payment.as_ref().ok_or_else(|| {
                PaymentError::Refused("locked token is unavailable after reconciliation".into())
            })?;
            require_locked_matches_terms(locked, terms)?;
            let verified = effects.verify_payment(&attempt_id, terms, locked)?;
            require_verified_matches_terms(&verified, terms)?;
            let payment = effects.send_payment(&attempt_id, terms, locked, &verified)?;
            if payment.relay_success.is_empty() {
                return Err(PaymentError::NoRelayAccepted);
            }
            let sent = PaymentState::Sent {
                attempt_id,
                payment,
            };
            append_transition(&mut guard, key, state.as_ref(), &sent)?;
            state = Some(sent);
        }

        if let Some(PaymentState::Sent {
            attempt_id,
            payment,
        }) = state.clone()
        {
            let receipt = authority.verify(effects.publish_receipt(key, &payment)?)?;
            let published = PaymentState::ReceiptPublished {
                attempt_id,
                receipt,
            };
            append_transition(&mut guard, key, state.as_ref(), &published)?;
            state = Some(published);
        }

        if let Some(PaymentState::ReceiptPublished {
            attempt_id,
            receipt,
        }) = state.clone()
        {
            let closed = PaymentState::Closed {
                attempt_id,
                receipt,
            };
            append_transition(&mut guard, key, state.as_ref(), &closed)?;
            state = Some(closed);
        }

        state.ok_or_else(|| PaymentError::Refused("payment state is absent".into()))
    }
}

fn append_transition<G: PaymentJournalGuard>(
    guard: &mut G,
    key: &PaymentKey,
    current: Option<&PaymentState>,
    next: &PaymentState,
) -> Result<(), PaymentError> {
    PaymentMachine::decide(current, next)?;
    guard.append_sync(&PaymentRecord {
        key: key.clone(),
        value: next.clone(),
    })?;
    Ok(())
}

fn require_key_matches_terms(key: &PaymentKey, terms: &PaymentTerms) -> Result<(), PaymentError> {
    if key.mint != terms.mint
        || key.amount != terms.amount
        || key.unit != terms.unit
        || key.seller_pubkey != terms.seller_pubkey
    {
        return Err(PaymentError::Refused(
            "payment key does not match typed payment terms".into(),
        ));
    }
    Ok(())
}

fn require_verified_matches_terms(
    verified: &VerifiedPayment,
    terms: &PaymentTerms,
) -> Result<(), PaymentError> {
    if verified.mint != terms.mint || verified.amount != terms.amount || verified.unit != terms.unit
    {
        return Err(PaymentError::Refused(
            "verified payment does not match typed payment terms".into(),
        ));
    }
    Ok(())
}

fn require_locked_matches_terms(
    locked: &LockedPayment,
    terms: &PaymentTerms,
) -> Result<(), PaymentError> {
    let token = locked.token();
    let amount = token
        .value()
        .map_err(|error| PaymentError::Refused(format!("invalid locked token: {error}")))?;
    let mint = token
        .mint_url()
        .map_err(|error| PaymentError::Refused(format!("invalid locked token: {error}")))?;
    if amount != terms.amount || mint != terms.mint || token.unit().as_ref() != Some(&terms.unit) {
        return Err(PaymentError::Refused(
            "locked token does not match typed payment terms".into(),
        ));
    }
    Ok(())
}

fn state_name(state: &PaymentState) -> &'static str {
    match state {
        PaymentState::Intent { .. } => "Intent",
        PaymentState::Locked { .. } => "Locked",
        PaymentState::Sent { .. } => "Sent",
        PaymentState::ReceiptPublished { .. } => "ReceiptPublished",
        PaymentState::Closed { .. } => "Closed",
    }
}

fn nonempty(value: String, name: &str) -> Result<String, PaymentError> {
    if value.is_empty() {
        Err(PaymentError::InvalidInput(format!("{name} is empty")))
    } else {
        Ok(value)
    }
}

fn digest_hex(value: String, name: &str) -> Result<String, PaymentError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(value)
    } else {
        Err(PaymentError::InvalidInput(format!(
            "{name} must be 32-byte lowercase hex"
        )))
    }
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value.as_bytes());
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Failure returned by an injected payment effect.
pub struct EffectError(String);

impl EffectError {
    /// Creates an effect failure with a stable message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for EffectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for EffectError {}

#[derive(Debug)]
/// Durable journal failure.
pub enum JournalError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Corrupt { line: usize, detail: String },
    KeyMismatch,
}

impl fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "payment journal I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "payment journal encoding failed: {error}"),
            Self::Corrupt { line, detail } => {
                write!(
                    formatter,
                    "payment journal line {line} is corrupt: {detail}"
                )
            }
            Self::KeyMismatch => formatter.write_str("payment journal guard key mismatch"),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<std::io::Error> for JournalError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for JournalError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Debug)]
/// Payment policy, transition, journal, or effect failure.
pub enum PaymentError {
    InvalidInput(String),
    IllegalTransition {
        from: &'static str,
        to: &'static str,
    },
    Journal(JournalError),
    Effect(EffectError),
    NoRelayAccepted,
    AmbiguousSendRefused,
    ForgedReceipt,
    Refused(String),
}

impl fmt::Display for PaymentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(formatter, "invalid payment input: {message}"),
            Self::IllegalTransition { from, to } => {
                write!(formatter, "illegal payment transition: {from} -> {to}")
            }
            Self::Journal(error) => error.fmt(formatter),
            Self::Effect(error) => write!(formatter, "payment effect failed: {error}"),
            Self::NoRelayAccepted => formatter.write_str("no relay accepted the payment"),
            Self::AmbiguousSendRefused => {
                formatter.write_str("payment send state is ambiguous; refusing automatic resend")
            }
            Self::ForgedReceipt => formatter.write_str("receipt author or signatures are invalid"),
            Self::Refused(message) => write!(formatter, "payment refused: {message}"),
        }
    }
}

impl std::error::Error for PaymentError {}

impl From<JournalError> for PaymentError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

impl From<EffectError> for PaymentError {
    fn from(error: EffectError) -> Self {
        Self::Effect(error)
    }
}

#[cfg(any(test, feature = "test-support"))]
mod memory {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};

    use super::*;

    #[derive(Clone, Default)]
    /// Mutex-backed journal for hermetic payment tests.
    pub struct MemoryPaymentJournal {
        records: Arc<Mutex<Vec<PaymentRecord>>>,
        sync_count: Arc<AtomicUsize>,
        replay_sync_count: Arc<AtomicUsize>,
    }

    impl MemoryPaymentJournal {
        /// Returns a snapshot of all recorded transitions.
        pub fn records(&self) -> Vec<PaymentRecord> {
            self.records
                .lock()
                .expect("memory journal poisoned")
                .clone()
        }

        /// Returns the number of durable append operations.
        pub fn sync_count(&self) -> usize {
            self.sync_count.load(Ordering::SeqCst)
        }

        /// Returns the number of replay durability syncs.
        pub fn replay_sync_count(&self) -> usize {
            self.replay_sync_count.load(Ordering::SeqCst)
        }
    }

    /// Exclusive mutex-backed journal guard.
    pub struct MemoryPaymentJournalGuard<'a> {
        records: MutexGuard<'a, Vec<PaymentRecord>>,
        key: PaymentKey,
        sync_count: &'a AtomicUsize,
        replay_sync_count: &'a AtomicUsize,
    }

    impl PaymentJournal for MemoryPaymentJournal {
        type Guard<'a> = MemoryPaymentJournalGuard<'a>;

        fn lock<'a>(&'a self, key: &PaymentKey) -> Result<Self::Guard<'a>, JournalError> {
            Ok(MemoryPaymentJournalGuard {
                records: self.records.lock().map_err(|_| {
                    JournalError::Io(std::io::Error::other("memory journal poisoned"))
                })?,
                key: key.clone(),
                sync_count: &self.sync_count,
                replay_sync_count: &self.replay_sync_count,
            })
        }
    }

    impl PaymentJournalGuard for MemoryPaymentJournalGuard<'_> {
        fn replay(&mut self) -> Result<Vec<PaymentRecord>, JournalError> {
            Ok(self
                .records
                .iter()
                .filter(|record| record.key == self.key)
                .cloned()
                .collect())
        }

        fn sync_replay(&mut self) -> Result<(), JournalError> {
            self.replay_sync_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn append_sync(&mut self, record: &PaymentRecord) -> Result<(), JournalError> {
            if record.key != self.key {
                return Err(JournalError::KeyMismatch);
            }
            self.records.push(record.clone());
            self.sync_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use memory::MemoryPaymentJournal;

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use cashu::secret::Secret;
    use cashu::{Id, Proof, SecretKey};

    use super::*;
    use crate::payment_send::PaymentRelayFailure;

    const MINT: &str = "https://testnut.cashu.space";

    #[test]
    fn machine_rejects_skipped_and_repeated_states() {
        let key = key();
        let attempt_id = key.attempt_id();
        assert!(matches!(
            PaymentMachine::decide(
                None,
                &PaymentState::Locked {
                    attempt_id: attempt_id.clone()
                }
            ),
            Err(PaymentError::IllegalTransition { .. })
        ));
        assert!(matches!(
            PaymentMachine::decide(
                Some(&PaymentState::Intent {
                    attempt_id: attempt_id.clone()
                }),
                &PaymentState::Intent { attempt_id }
            ),
            Err(PaymentError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn attempt_id_is_stable_and_unit_is_part_of_the_key() {
        let sat = key();
        let mut msat = sat.clone();
        msat.unit = CurrencyUnit::Msat;

        assert_eq!(sat.attempt_id(), sat.attempt_id());
        assert_ne!(sat.attempt_id(), msat.attempt_id());
    }

    #[test]
    fn service_mints_at_most_once_across_retry_and_concurrency() {
        let journal = Arc::new(MemoryPaymentJournal::default());
        let shared = FakeShared::default();
        let key = Arc::new(key());
        let terms = Arc::new(terms());
        let authority = Arc::new(authority());

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let journal = Arc::clone(&journal);
                let shared = shared.clone();
                let key = Arc::clone(&key);
                let terms = Arc::clone(&terms);
                let authority = Arc::clone(&authority);
                thread::spawn(move || {
                    let mut effects = FakeEffects::new(shared);
                    PaymentService::new(journal.as_ref()).run(
                        &key,
                        &terms,
                        &authority,
                        &mut effects,
                    )
                })
            })
            .collect();

        for handle in handles {
            let result = handle.join().unwrap();
            assert!(matches!(result, Ok(PaymentState::Closed { .. })));
        }
        assert_eq!(shared.mint_count.load(Ordering::SeqCst), 1);
        assert_eq!(shared.send_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn crash_after_lock_effect_reconciles_without_reminting() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut crashing = FakeEffects::new(shared.clone());
        crashing.crash_after_lock = true;

        let first =
            PaymentService::new(&journal).run(&key(), &terms(), &authority(), &mut crashing);
        assert!(matches!(first, Err(PaymentError::Effect(_))));
        assert!(matches!(
            journal.records().last().map(|record| &record.value),
            Some(PaymentState::Intent { .. })
        ));

        let mut recovered = FakeEffects::new(shared.clone());
        let result = PaymentService::new(&journal)
            .run(&key(), &terms(), &authority(), &mut recovered)
            .unwrap();

        assert!(matches!(result, PaymentState::Closed { .. }));
        assert_eq!(shared.mint_count.load(Ordering::SeqCst), 1);
        assert_eq!(shared.lock_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn blind_remint_negative_control_violates_the_pay_once_counter() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut crashing = FakeEffects::new(shared.clone());
        crashing.blind_lock = true;
        crashing.crash_after_lock = true;

        assert!(
            PaymentService::new(&journal)
                .run(&key(), &terms(), &authority(), &mut crashing)
                .is_err()
        );

        let mut recovered = FakeEffects::new(shared.clone());
        recovered.blind_lock = true;
        assert!(
            PaymentService::new(&journal)
                .run(&key(), &terms(), &authority(), &mut recovered)
                .is_ok()
        );

        assert_eq!(shared.mint_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn every_effect_observes_its_write_ahead_sync() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared);
        effects.ordering_journal = Some(journal.clone());

        assert!(
            PaymentService::new(&journal)
                .run(&key(), &terms(), &authority(), &mut effects)
                .is_ok()
        );
        assert_eq!(journal.sync_count(), 5);
    }

    #[test]
    fn replay_is_synced_before_the_first_effect() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared);
        effects.replay_sync_journal = Some(journal.clone());

        assert!(PaymentService::new(&journal)
            .run(&key(), &terms(), &authority(), &mut effects)
            .is_ok());
    }

    #[test]
    fn mismatched_locked_token_refuses_before_verify_or_send() {
        let expected = terms();
        let mut wrong_unit = expected.clone();
        wrong_unit.unit = CurrencyUnit::Msat;
        let mut wrong_amount = expected.clone();
        wrong_amount.amount = Amount::from(8);
        let mut wrong_mint = expected.clone();
        wrong_mint.mint = MintUrl::from_str("https://other-mint.example").unwrap();

        for locked_terms in [wrong_unit, wrong_amount, wrong_mint] {
            let journal = MemoryPaymentJournal::default();
            let shared = FakeShared::default();
            let mut effects = FakeEffects::new(shared.clone());
            effects.locked_terms = Some(locked_terms);

            let error = PaymentService::new(&journal)
                .run(&key(), &expected, &authority(), &mut effects)
                .unwrap_err();

            assert!(
                matches!(error, PaymentError::Refused(message) if message.contains("locked token"))
            );
            assert!(matches!(
                journal.records().last().map(|record| &record.value),
                Some(PaymentState::Locked { .. })
            ));
            assert_eq!(shared.verify_count.load(Ordering::SeqCst), 0);
            assert_eq!(shared.send_count.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn empty_relay_success_stays_locked_and_never_auto_resends() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared.clone());
        effects.empty_send = true;

        let first = PaymentService::new(&journal).run(&key(), &terms(), &authority(), &mut effects);
        assert!(matches!(first, Err(PaymentError::NoRelayAccepted)));
        assert!(matches!(
            journal.records().last().map(|record| &record.value),
            Some(PaymentState::Locked { .. })
        ));

        let mut retry = FakeEffects::new(shared.clone());
        assert!(matches!(
            PaymentService::new(&journal).run(&key(), &terms(), &authority(), &mut retry),
            Err(PaymentError::AmbiguousSendRefused)
        ));
        assert_eq!(shared.send_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sent_recovery_retries_receipt_only() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared.clone());
        effects.fail_receipt = true;

        assert!(matches!(
            PaymentService::new(&journal).run(&key(), &terms(), &authority(), &mut effects),
            Err(PaymentError::Effect(_))
        ));
        assert!(matches!(
            journal.records().last().map(|record| &record.value),
            Some(PaymentState::Sent { .. })
        ));

        let mut retry = FakeEffects::new(shared.clone());
        assert!(matches!(
            PaymentService::new(&journal)
                .run(&key(), &terms(), &authority(), &mut retry)
                .unwrap(),
            PaymentState::Closed { .. }
        ));
        assert_eq!(shared.mint_count.load(Ordering::SeqCst), 1);
        assert_eq!(shared.send_count.load(Ordering::SeqCst), 1);
        assert_eq!(shared.receipt_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn receipt_published_recovery_closes_without_any_effect() {
        let journal = MemoryPaymentJournal::default();
        let payment_key = key();
        let attempt_id = payment_key.attempt_id();
        let states = [
            PaymentState::Intent {
                attempt_id: attempt_id.clone(),
            },
            PaymentState::Locked {
                attempt_id: attempt_id.clone(),
            },
            PaymentState::Sent {
                attempt_id: attempt_id.clone(),
                payment: sent(),
            },
            PaymentState::ReceiptPublished {
                attempt_id,
                receipt: ReceiptRecord {
                    receipt_id: "receipt".into(),
                },
            },
        ];
        {
            let mut guard = journal.lock(&payment_key).unwrap();
            for state in states {
                guard
                    .append_sync(&PaymentRecord {
                        key: payment_key.clone(),
                        value: state,
                    })
                    .unwrap();
            }
        }
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared.clone());

        assert!(matches!(
            PaymentService::new(&journal)
                .run(&payment_key, &terms(), &authority(), &mut effects)
                .unwrap(),
            PaymentState::Closed { .. }
        ));
        assert_eq!(shared.lock_calls.load(Ordering::SeqCst), 0);
        assert_eq!(shared.send_count.load(Ordering::SeqCst), 0);
        assert_eq!(shared.receipt_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn forged_receipt_author_or_missing_signature_is_rejected() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared);
        effects.forged_receipt = true;

        assert!(matches!(
            PaymentService::new(&journal).run(&key(), &terms(), &authority(), &mut effects),
            Err(PaymentError::ForgedReceipt)
        ));
        assert!(matches!(
            journal.records().last().map(|record| &record.value),
            Some(PaymentState::Sent { .. })
        ));
    }

    #[test]
    fn fs_journal_rejects_torn_tail() {
        let path = std::env::temp_dir().join(format!(
            "mobee-payment-journal-{}-{}.jsonl",
            std::process::id(),
            key().attempt_id().as_str()
        ));
        let _ = std::fs::remove_file(&path);
        let journal = FsPaymentJournal::new(&path);
        {
            let mut guard = journal.lock(&key()).unwrap();
            guard
                .append_sync(&PaymentRecord {
                    key: key(),
                    value: PaymentState::Intent {
                        attempt_id: key().attempt_id(),
                    },
                })
                .unwrap();
        }
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"key":"torn"#)
            .unwrap();

        let mut guard = journal.lock(&key()).unwrap();
        assert!(matches!(
            guard.replay(),
            Err(JournalError::Corrupt { line: 2, .. })
        ));
        drop(guard);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn fs_journal_rejects_complete_json_without_newline() {
        let path = std::env::temp_dir().join(format!(
            "mobee-payment-journal-complete-tail-{}-{}.jsonl",
            std::process::id(),
            key().attempt_id().as_str()
        ));
        let _ = std::fs::remove_file(&path);
        let record = PaymentRecord {
            key: key(),
            value: PaymentState::Intent {
                attempt_id: key().attempt_id(),
            },
        };
        serde_json::to_writer(File::create(&path).unwrap(), &record).unwrap();

        let journal = FsPaymentJournal::new(&path);
        let mut guard = journal.lock(&key()).unwrap();
        assert!(matches!(
            guard.replay(),
            Err(JournalError::Corrupt { line: 1, .. })
        ));
        drop(guard);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn fs_journal_syncs_parent_before_the_first_effect() {
        let directory = std::env::temp_dir().join(format!(
            "mobee-payment-journal-dir-{}-{}",
            std::process::id(),
            key().attempt_id().as_str()
        ));
        let path = directory.join("payments.jsonl");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let journal = FsPaymentJournal::new(&path);

        let guard = journal.lock(&key()).unwrap();
        assert_eq!(journal.parent_sync_count(), 1);
        drop(guard);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[derive(Clone, Default)]
    struct FakeShared {
        attempts: Arc<Mutex<Vec<AttemptId>>>,
        mint_count: Arc<AtomicUsize>,
        lock_calls: Arc<AtomicUsize>,
        verify_count: Arc<AtomicUsize>,
        send_count: Arc<AtomicUsize>,
        receipt_count: Arc<AtomicUsize>,
    }

    struct FakeEffects {
        shared: FakeShared,
        blind_lock: bool,
        crash_after_lock: bool,
        locked_terms: Option<PaymentTerms>,
        empty_send: bool,
        fail_receipt: bool,
        forged_receipt: bool,
        ordering_journal: Option<MemoryPaymentJournal>,
        replay_sync_journal: Option<MemoryPaymentJournal>,
    }

    impl FakeEffects {
        fn new(shared: FakeShared) -> Self {
            Self {
                shared,
                blind_lock: false,
                crash_after_lock: false,
                locked_terms: None,
                empty_send: false,
                fail_receipt: false,
                forged_receipt: false,
                ordering_journal: None,
                replay_sync_journal: None,
            }
        }
    }

    impl PaymentEffects for FakeEffects {
        fn lock_or_reconcile(
            &mut self,
            attempt_id: &AttemptId,
            terms: &PaymentTerms,
        ) -> Result<LockedPayment, EffectError> {
            if let Some(journal) = &self.replay_sync_journal {
                assert_eq!(
                    journal.replay_sync_count(),
                    1,
                    "replay must sync before lock"
                );
            }
            if let Some(journal) = &self.ordering_journal {
                assert_eq!(journal.sync_count(), 1, "Intent must sync before lock");
            }
            self.shared.lock_calls.fetch_add(1, Ordering::SeqCst);
            let mut attempts = self.shared.attempts.lock().unwrap();
            if self.blind_lock || !attempts.contains(attempt_id) {
                attempts.push(attempt_id.clone());
                self.shared.mint_count.fetch_add(1, Ordering::SeqCst);
            }
            drop(attempts);
            if self.crash_after_lock {
                self.crash_after_lock = false;
                return Err(EffectError::new("simulated crash after lock effect"));
            }
            Ok(locked_payment(self.locked_terms.as_ref().unwrap_or(terms)))
        }

        fn verify_payment(
            &mut self,
            _attempt_id: &AttemptId,
            terms: &PaymentTerms,
            _locked: &LockedPayment,
        ) -> Result<VerifiedPayment, EffectError> {
            if let Some(journal) = &self.ordering_journal {
                assert_eq!(journal.sync_count(), 2, "Locked must sync before verify");
            }
            self.shared.verify_count.fetch_add(1, Ordering::SeqCst);
            Ok(VerifiedPayment {
                mint: terms.mint.clone(),
                amount: terms.amount,
                unit: terms.unit.clone(),
                proof_ys: Vec::new(),
            })
        }

        fn send_payment(
            &mut self,
            _attempt_id: &AttemptId,
            _terms: &PaymentTerms,
            _locked: &LockedPayment,
            _verified: &VerifiedPayment,
        ) -> Result<PaymentSent, EffectError> {
            if let Some(journal) = &self.ordering_journal {
                assert_eq!(journal.sync_count(), 2, "Locked must sync before send");
            }
            self.shared.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(PaymentSent {
                payment_id: "payment".into(),
                relay_success: if self.empty_send {
                    Vec::new()
                } else {
                    vec!["memory://relay".into()]
                },
                relay_failed: if self.empty_send {
                    vec![PaymentRelayFailure {
                        relay: "memory://relay".into(),
                        error: "rejected".into(),
                    }]
                } else {
                    Vec::new()
                },
            })
        }

        fn publish_receipt(
            &mut self,
            _key: &PaymentKey,
            _payment: &PaymentSent,
        ) -> Result<ReceiptEvidence, EffectError> {
            if let Some(journal) = &self.ordering_journal {
                assert_eq!(journal.sync_count(), 3, "Sent must sync before receipt");
            }
            self.shared.receipt_count.fetch_add(1, Ordering::SeqCst);
            if self.fail_receipt {
                return Err(EffectError::new("receipt relay unavailable"));
            }
            let authority = authority();
            Ok(ReceiptEvidence {
                receipt_id: "receipt".into(),
                author: if self.forged_receipt {
                    public_key(9)
                } else {
                    authority.buyer
                },
                valid_signers: if self.forged_receipt {
                    vec![authority.seller]
                } else {
                    vec![authority.buyer, authority.seller]
                },
            })
        }
    }

    fn terms() -> PaymentTerms {
        PaymentTerms::new(
            MintUrl::from_str(MINT).unwrap(),
            Amount::from(7),
            CurrencyUnit::Sat,
            public_key(2),
        )
    }

    fn key() -> PaymentKey {
        PaymentKey::new(
            JobId::new("job").unwrap(),
            ResultId::new("result").unwrap(),
            ContentHash::from_hex("11".repeat(32)).unwrap(),
            JobHash::from_hex("22".repeat(32)).unwrap(),
            &terms(),
        )
    }

    fn authority() -> ReceiptAuthority {
        ReceiptAuthority {
            buyer: public_key(1),
            seller: public_key(2),
        }
    }

    fn sent() -> PaymentSent {
        PaymentSent {
            payment_id: "payment".into(),
            relay_success: vec!["memory://relay".into()],
            relay_failed: Vec::new(),
        }
    }

    fn public_key(byte: u8) -> PublicKey {
        SecretKey::from_slice(&[byte; 32]).unwrap().public_key()
    }

    fn locked_payment(terms: &PaymentTerms) -> LockedPayment {
        let proof = Proof::new(
            terms.amount,
            Id::from_str("010000000000000000000000000000000000000000000000000000000000000000")
                .unwrap(),
            Secret::new("payment-state-machine-fixture"),
            public_key(42),
        );
        LockedPayment::new(Token::new(
            terms.mint.clone(),
            vec![proof],
            None,
            terms.unit.clone(),
        ))
    }
}
