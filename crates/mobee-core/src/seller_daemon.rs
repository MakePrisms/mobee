//! Seller daemon state machine (freeze checklist).
//!
//! Loop:
//! 1. Subscribe **5109 + 1059 from START** (early pay buffered).
//! 2. On targeted offer passing B1 rate-gate → claim 7000 → journal claim (single-flight).
//! 3. Run agent (`--features acp` fail-closed) → git push (allowlist+scrub) → 6109.
//! 4. **Reconcile** buffered/already-received 1059 wraps against the new result.
//! 5. B2 bind job_id(+result_id) → `terms_for_offer` → `CdkSellerReceive::receive`
//!    (`Amount == offer.amount`) → journal receipt (`amount_received == offer.amount`).
//!
//! Never logs NIP-17 plaintext / tokens / key material. Observatory untouched.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(feature = "acp")]
use sha2::{Digest, Sha256};

use crate::buyer_fund::{self, FundError};
use crate::gateway::{
    self, claim_draft, error_draft, git_result_draft, parse_offer, EventDraft, ParsedOffer,
    JOB_OFFER_KIND,
};
use crate::home::{self, HomeError, MobeeHome, DEFAULT_MINT_URL};
use crate::job_lifecycle::{event_to_draft, job_hash_for_offer};
use crate::payment_send::ReceivedPayment;
use crate::payment_wallet::{CdkSellerReceive, PaymentPolicy, PaymentWalletError};
use crate::seller::{
    cashu_secret_from_nostr_hex, job_deadline_unix, rate_gate_allows, require_seller_config,
    sign_receipt_hash, unwrap_own_payment_gift_wrap, SellerError, SellerJournal,
};
use crate::seller_git::{self, SellerGitError};

/// In-flight single-flight lock for v1 (one job at a time per process).
static FLIGHT: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub enum DaemonError {
    Seller(SellerError),
    Home(HomeError),
    Fund(FundError),
    Wallet(PaymentWalletError),
    Git(SellerGitError),
    Relay(String),
    Agent(String),
    Policy(String),
    Config(String),
    AcpRequired,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seller(error) => write!(f, "{error}"),
            Self::Home(error) => write!(f, "{error}"),
            Self::Fund(error) => write!(f, "{error}"),
            Self::Wallet(error) => write!(f, "{error}"),
            Self::Git(error) => write!(f, "{error}"),
            Self::Relay(message) => write!(f, "seller relay error: {message}"),
            Self::Agent(message) => write!(f, "seller agent error: {message}"),
            Self::Policy(message) => write!(f, "seller policy: {message}"),
            Self::Config(message) => write!(f, "seller config: {message}"),
            Self::AcpRequired => write!(
                f,
                "seller agent-run requires rebuilding with the acp feature: \
                 cargo run -p mobee --features acp -- sell run"
            ),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<SellerError> for DaemonError {
    fn from(value: SellerError) -> Self {
        Self::Seller(value)
    }
}
impl From<HomeError> for DaemonError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}
impl From<FundError> for DaemonError {
    fn from(value: FundError) -> Self {
        Self::Fund(value)
    }
}
impl From<PaymentWalletError> for DaemonError {
    fn from(value: PaymentWalletError) -> Self {
        Self::Wallet(value)
    }
}
impl From<SellerGitError> for DaemonError {
    fn from(value: SellerGitError) -> Self {
        Self::Git(value)
    }
}

/// Active claimed job (single-flight slot).
#[derive(Debug, Clone)]
pub struct ActiveJob {
    pub job_id: String,
    pub buyer_pubkey: String,
    pub offer: ParsedOffer,
    pub claim_id: String,
    pub result_id: Option<String>,
    pub deadline_unix: u64,
    pub workdir: PathBuf,
}

/// Buffered early payment (received before/while result published).
struct BufferedPay {
    event_id: String,
    received: ReceivedPayment,
}

/// Seller daemon runtime state.
pub struct SellerDaemon {
    home: MobeeHome,
    keys: nostr_sdk::Keys,
    seller_pubkey: String,
    journal: SellerJournal,
    /// Early / unmatched 1059 payments awaiting reconcile (ids only logged).
    pay_buffer: VecDeque<BufferedPay>,
    active: Option<ActiveJob>,
}

impl SellerDaemon {
    pub fn open(home: MobeeHome) -> Result<Self, DaemonError> {
        require_seller_config(&home)?;
        if home.config.mint_url != DEFAULT_MINT_URL {
            return Err(DaemonError::Config(format!(
                "seller mint fail-closed: configured mint_url must be {DEFAULT_MINT_URL}, got {}",
                home.config.mint_url
            )));
        }
        let secret = home::read_secret_key_hex(&home)?;
        let keys = nostr_sdk::Keys::parse(&secret)
            .map_err(|error| DaemonError::Home(HomeError::Key(format!("parse: {error}"))))?;
        let seller_pubkey = keys.public_key().to_hex();
        let journal = SellerJournal::open(&home)?;
        Ok(Self {
            home,
            keys,
            seller_pubkey,
            journal,
            pay_buffer: VecDeque::new(),
            active: None,
        })
    }

    pub fn seller_pubkey(&self) -> &str {
        &self.seller_pubkey
    }

    pub fn home(&self) -> &MobeeHome {
        &self.home
    }

    /// Try to take the single-flight slot.
    pub fn try_begin_flight() -> bool {
        FLIGHT
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn end_flight() {
        FLIGHT.store(false, Ordering::SeqCst);
    }

    pub fn in_flight() -> bool {
        FLIGHT.load(Ordering::SeqCst)
    }

    /// Handle one kind-5109 offer event. Returns Ok(Some(active)) when claimed.
    pub async fn on_offer_event(
        &mut self,
        event: &nostr_sdk::Event,
    ) -> Result<Option<&ActiveJob>, DaemonError> {
        if Self::in_flight() || self.active.is_some() {
            return Ok(None); // single-flight: ignore while busy
        }
        if event.kind.as_u16() != JOB_OFFER_KIND {
            return Ok(None);
        }
        let draft = event_to_draft(event);
        let offer = match parse_offer(&draft) {
            Ok(offer) => offer,
            Err(_) => return Ok(None),
        };
        // Offer mint fail-closed to testnut (soft-skip so the daemon stays up).
        if offer.mint_url != DEFAULT_MINT_URL {
            eprintln!(
                "seller skip offer {}: non-testnut mint",
                event.id.to_hex()
            );
            return Ok(None);
        }
        let seller_cfg = require_seller_config(&self.home)?;
        if let Err(error) = rate_gate_allows(&offer, &self.seller_pubkey, seller_cfg.rate_sats) {
            // Soft skip (not our rate / not targeted) — not a hard daemon error.
            let _ = error;
            return Ok(None);
        }
        let job_id = event.id.to_hex();
        if self.journal.has_claim(&job_id)? {
            return Ok(None);
        }
        if !Self::try_begin_flight() {
            return Ok(None);
        }

        let buyer_pubkey = event.pubkey.to_hex();
        let claim = claim_draft(&job_id, &buyer_pubkey, &self.seller_pubkey);
        let claim_id = match publish_draft(&self.home, &self.keys, &claim).await {
            Ok(id) => id,
            Err(error) => {
                Self::end_flight();
                return Err(error);
            }
        };
        if let Err(error) = self.journal.append_claim(&job_id) {
            Self::end_flight();
            return Err(error.into());
        }

        let now = now_unix();
        let deadline = job_deadline_unix(&offer, seller_cfg, now);
        let workdir = job_workdir(&self.home, &job_id);
        if let Err(error) = std::fs::create_dir_all(&workdir) {
            Self::end_flight();
            return Err(DaemonError::Seller(SellerError::Io(error.to_string())));
        }

        self.active = Some(ActiveJob {
            job_id,
            buyer_pubkey,
            offer,
            claim_id,
            result_id: None,
            deadline_unix: deadline,
            workdir,
        });
        Ok(self.active.as_ref())
    }

    /// Buffer or attempt apply of one kind-1059 gift wrap (ONE decode site).
    /// Prefer [`ingest_gift_wrap`] which handles buffer-vs-apply correctly.
    pub async fn on_gift_wrap_event(
        &mut self,
        event: &nostr_sdk::Event,
    ) -> Result<Option<ReceiptOutcome>, DaemonError> {
        ingest_gift_wrap(self, event).await
    }

    /// After publishing 6109: reconcile buffered wraps so early pay still lands (B2).
    pub async fn reconcile_payments(&mut self) -> Result<Option<ReceiptOutcome>, DaemonError> {
        reconcile_after_result(self).await
    }

    fn buffer_payment(&mut self, event_id: String, received: ReceivedPayment) {
        if self.pay_buffer.iter().any(|entry| entry.event_id == event_id) {
            return;
        }
        if self.pay_buffer.len() >= 32 {
            self.pay_buffer.pop_front();
        }
        self.pay_buffer.push_back(BufferedPay { event_id, received });
    }

    async fn try_apply_payment(
        &mut self,
        _event_id: &str,
        received: ReceivedPayment,
    ) -> Result<Option<ReceiptOutcome>, DaemonError> {
        let Some(active) = self.active.as_ref() else {
            // No active job yet — caller should buffer.
            return Ok(None);
        };
        let Some(result_id) = active.result_id.as_deref() else {
            return Ok(None);
        };
        let local_job = active.job_id.clone();
        let local_result = result_id.to_owned();
        let expected_amount = active.offer.amount;
        let mint = active.offer.mint_url.clone();
        let offer = active.offer.clone();

        let payload_job = received.payload.job_id.clone();
        let payload_result = received.payload.result_id.clone();
        // B2: bind BEFORE journal — wrong-job refuse (no misattribution).
        if payload_job != local_job || payload_result != local_result {
            return Err(DaemonError::Policy(format!(
                "payment bind refused: payload job/result ({payload_job}/{payload_result}) != local ({local_job}/{local_result})"
            )));
        }

        let buyer = received.buyer_pubkey.to_hex();
        let policy = PaymentPolicy::new([mint_url(&mint)?]);
        let terms = policy.terms_for_offer(&offer, &self.seller_pubkey)?;
        // Amount in terms == offer.amount (NOT rate_sats).
        let secret = home::read_secret_key_hex(&self.home)?;
        let cashu_key = cashu_secret_from_nostr_hex(&secret)?;
        // Must await — seller loop already owns a tokio runtime; blocking open nests block_on → panic.
        let wallet = buyer_fund::open_testnut_wallet_async(&self.home).await?;
        let adapter = CdkSellerReceive::new(&wallet, cashu_key);
        let amount = adapter.receive(&received.payload.token, &terms).await?;
        let amount_received = amount.to_u64();

        self.journal.append_receipt(
            &local_job,
            &local_result,
            &payload_job,
            &payload_result,
            amount_received,
            expected_amount,
            &mint,
            &buyer,
            true,
        )?;

        let outcome = ReceiptOutcome {
            job_id: local_job,
            result_id: local_result,
            amount_received,
        };
        self.active = None;
        Self::end_flight();
        Ok(Some(outcome))
    }

    /// Run agent → push → publish 6109. On fail/timeout publish 7000 error and clear flight.
    pub async fn execute_active_job(&mut self) -> Result<String, DaemonError> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| DaemonError::Policy("no active job".into()))?
            .clone();
        if now_unix() > active.deadline_unix {
            self.fail_active("job deadline exceeded").await?;
            return Err(DaemonError::Policy("job deadline exceeded".into()));
        }

        let seller_cfg = require_seller_config(&self.home)?.clone();
        // Gate #10: capture HEAD before agent; deliver only if agent advanced it.
        let before_oid = seller_git::try_head_oid(&active.workdir, &self.home.root);
        let run_result = run_agent_job(&seller_cfg.agent_command, &active.offer.task, &active.workdir).await;
        if let Err(error) = run_result {
            self.fail_active(&error.to_string()).await?;
            return Err(error);
        }
        let after_oid = seller_git::try_head_oid(&active.workdir, &self.home.root);
        let _advanced = match seller_git::require_agent_advanced_head(
            before_oid.as_deref(),
            after_oid.as_deref(),
        ) {
            Ok(oid) => oid,
            Err(error) => {
                self.fail_active(&error.to_string()).await?;
                return Err(error.into());
            }
        };

        let branch = format!("mobee/{}", &active.job_id[..8.min(active.job_id.len())]);
        // Ensure we're on a branch named for the job (best-effort).
        let _ = std::process::Command::new("git")
            .args(["checkout", "-B", &branch])
            .current_dir(&active.workdir)
            .status();

        let commit = match seller_git::push_branch(
            &active.workdir,
            &seller_cfg.git_remote,
            &branch,
            &self.home.root,
        ) {
            Ok(oid) => oid,
            Err(error) => {
                self.fail_active(&error.to_string()).await?;
                return Err(error.into());
            }
        };

        let job_hash = job_hash_for_offer(&active.job_id, &active.offer.task, active.offer.amount);
        let seller_sig = sign_receipt_hash(&self.keys, &job_hash)?;
        let draft = git_result_draft(
            &active.job_id,
            &active.buyer_pubkey,
            &seller_cfg.git_remote,
            &branch,
            &commit,
            active.offer.amount,
            &job_hash,
            &seller_sig,
            format!("delivery commit {commit}"),
        );
        let result_id = match publish_draft(&self.home, &self.keys, &draft).await {
            Ok(id) => id,
            Err(error) => {
                self.fail_active(&error.to_string()).await?;
                return Err(error);
            }
        };
        if let Some(slot) = self.active.as_mut() {
            slot.result_id = Some(result_id.clone());
        }
        Ok(result_id)
    }

    async fn fail_active(&mut self, _reason: &str) -> Result<(), DaemonError> {
        if let Some(active) = self.active.take() {
            let draft = error_draft(&active.job_id, &active.buyer_pubkey, &self.seller_pubkey);
            let _ = publish_draft(&self.home, &self.keys, &draft).await;
        }
        Self::end_flight();
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptOutcome {
    pub job_id: String,
    pub result_id: String,
    pub amount_received: u64,
}

fn mint_url(raw: &str) -> Result<cashu::MintUrl, DaemonError> {
    use std::str::FromStr;
    cashu::MintUrl::from_str(raw)
        .map_err(|error| DaemonError::Policy(format!("invalid mint url: {error}")))
}

fn job_workdir(home: &MobeeHome, job_id: &str) -> PathBuf {
    home.root.join("seller-jobs").join(job_id)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn publish_draft(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    draft: &EventDraft,
) -> Result<String, DaemonError> {
    use nostr_sdk::prelude::{Client, Kind};

    let builder = gateway::nostr::event_builder(draft)
        .map_err(|error| DaemonError::Relay(format!("event builder: {error}")))?;
    let event = builder
        .sign_with_keys(keys)
        .map_err(|error| DaemonError::Relay(format!("sign: {error}")))?;
    let _ = Kind::Custom(draft.kind);

    let client = Client::new(keys.clone());
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| DaemonError::Relay(format!("add relay: {error}")))?;
    client.connect().await;
    let output = client
        .send_event_to([&home.config.relay_url], &event)
        .await;
    client.disconnect().await;
    let output = output.map_err(|error| DaemonError::Relay(format!("send: {error}")))?;
    if output.success.is_empty() {
        return Err(DaemonError::Relay("no relay accepted event".into()));
    }
    Ok(output.val.to_hex())
}

#[cfg(feature = "acp")]
async fn run_agent_job(
    agent_command: &[String],
    task: &str,
    workdir: &Path,
) -> Result<(), DaemonError> {
    use std::time::Duration;

    use crate::driver::{AcpDriver, AgentCommand, ContentBlock, PromptTurn, SessionConfig};
    use crate::engine::{RunParams, run_job};
    use crate::event::JobId;
    use crate::log::EventLog;

    if agent_command.is_empty() {
        return Err(DaemonError::Config("agent_command empty".into()));
    }
    let mut driver = AcpDriver::new(
        AgentCommand::new(agent_command[0].clone(), agent_command[1..].to_vec()),
        crate::driver::PermissionOutcome::Allow,
        Duration::from_secs(300),
    );
    let log_path = workdir.join("seller-run.jsonl");
    let mut log = EventLog::open(&log_path)
        .map_err(|error| DaemonError::Agent(error.to_string()))?;
    let params = RunParams {
        session_config: SessionConfig {
            cwd: workdir.to_path_buf(),
            mcp_servers: Vec::new(),
            env: Vec::new(),
        },
        prompt: PromptTurn {
            input: vec![ContentBlock::Text {
                text: task.to_owned(),
            }],
        },
    };
    let outcome = run_job(
        &mut driver,
        &mut log,
        &JobId(format!("seller-{}", short_hash(task))),
        params,
        &mut |_| {},
    )
    .await
    .map_err(|error| DaemonError::Agent(error.to_string()))?;
    match outcome.terminal {
        crate::event::JobExecutionStatus::Completed => Ok(()),
        other => Err(DaemonError::Agent(format!("agent terminal {other:?}"))),
    }
}

#[cfg(not(feature = "acp"))]
async fn run_agent_job(
    _agent_command: &[String],
    _task: &str,
    _workdir: &Path,
) -> Result<(), DaemonError> {
    Err(DaemonError::AcpRequired)
}

#[cfg(feature = "acp")]
fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..8])
}

/// Handle one gift-wrap: unwrap (one site), then apply or buffer.
pub async fn ingest_gift_wrap(
    daemon: &mut SellerDaemon,
    event: &nostr_sdk::Event,
) -> Result<Option<ReceiptOutcome>, DaemonError> {
    let Some(received) = unwrap_own_payment_gift_wrap(&daemon.keys, event).await? else {
        return Ok(None);
    };
    let event_id = event.id.to_hex();
    match try_apply_or_buffer(daemon, event_id, received).await? {
        ApplyResult::Applied(outcome) => Ok(Some(outcome)),
        ApplyResult::Buffered => Ok(None),
    }
}

enum ApplyResult {
    Applied(ReceiptOutcome),
    Buffered,
}

async fn try_apply_or_buffer(
    daemon: &mut SellerDaemon,
    event_id: String,
    received: ReceivedPayment,
) -> Result<ApplyResult, DaemonError> {
    let has_result = daemon
        .active
        .as_ref()
        .and_then(|job| job.result_id.as_ref())
        .is_some();
    if !has_result {
        daemon.buffer_payment(event_id, received);
        return Ok(ApplyResult::Buffered);
    }
    // Bind check first without consuming into receive on wrong-job:
    if let Some(active) = daemon.active.as_ref() {
        let local_job = &active.job_id;
        let local_result = active.result_id.as_deref().unwrap_or("");
        if received.payload.job_id != *local_job || received.payload.result_id != local_result {
            return Err(DaemonError::Policy(format!(
                "payment bind refused: payload job/result ({}/{}) != local ({local_job}/{local_result})",
                received.payload.job_id, received.payload.result_id
            )));
        }
    }
    match daemon.try_apply_payment(&event_id, received).await? {
        Some(outcome) => Ok(ApplyResult::Applied(outcome)),
        None => Ok(ApplyResult::Buffered),
    }
}

/// Reconcile buffered payments after 6109 publish (B2 early-pay).
pub async fn reconcile_after_result(
    daemon: &mut SellerDaemon,
) -> Result<Option<ReceiptOutcome>, DaemonError> {
    let mut pending = std::mem::take(&mut daemon.pay_buffer);
    let mut leftover = VecDeque::new();
    let mut done = None;
    while let Some(BufferedPay { event_id, received }) = pending.pop_front() {
        if done.is_some() {
            leftover.push_back(BufferedPay { event_id, received });
            continue;
        }
        match try_apply_or_buffer(daemon, event_id.clone(), received).await? {
            ApplyResult::Applied(outcome) => done = Some(outcome),
            ApplyResult::Buffered => {
                // re-buffered inside try_apply_or_buffer when no result — should not happen
                // after result publish; if bind not ready, leave in leftover via buffer.
                let _ = event_id;
            }
        }
    }
    // Drain anything re-buffered during loop.
    while let Some(item) = daemon.pay_buffer.pop_front() {
        leftover.push_back(item);
    }
    daemon.pay_buffer = leftover;
    Ok(done)
}

/// Blocking wrapper for [`run_forever`] (current-thread tokio runtime).
pub fn run_forever_blocking(daemon: SellerDaemon) -> Result<(), DaemonError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| DaemonError::Config(format!("tokio runtime: {error}")))?;
    runtime.block_on(run_forever(daemon))
}

/// Drain `notifications` until NIP-42 AUTH succeeds (or fail closed).
///
/// Caller must subscribe `relay.notifications()` **before** `connect` so the
/// `Authenticated` event cannot be missed.
///
/// mobee-relay p-gates kind-1059: unauthenticated `REQ kinds:[1059] #p:self` is
/// `CLOSED` with `restricted:` (not `auth-required:`). nostr-sdk 0.44 treats
/// `restricted:` as `Remove` — the sub is dropped, so the post-auth
/// `resubscribe()` never restores it. Auth **before** the 1059 subscribe is
/// therefore load-bearing for seller receive.
async fn wait_for_nip42_auth(
    notifications: &mut tokio::sync::broadcast::Receiver<nostr_sdk::pool::RelayNotification>,
    timeout: std::time::Duration,
) -> Result<(), DaemonError> {
    use nostr_sdk::pool::RelayNotification;

    tokio::time::timeout(timeout, async {
        loop {
            match notifications.recv().await {
                Ok(RelayNotification::Authenticated) => return Ok(()),
                Ok(RelayNotification::AuthenticationFailed) => {
                    return Err(DaemonError::Relay(
                        "NIP-42 authentication failed (required for kind-1059 p-gated receive)"
                            .into(),
                    ));
                }
                Ok(RelayNotification::Shutdown) => {
                    return Err(DaemonError::Relay(
                        "relay shutdown before NIP-42 authentication".into(),
                    ));
                }
                Ok(_) => {}
                Err(_) => {
                    return Err(DaemonError::Relay(
                        "relay notification channel closed before NIP-42 authentication".into(),
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| {
        DaemonError::Relay(
            "timed out waiting for NIP-42 authentication (required for kind-1059 receive)".into(),
        )
    })?
}

/// Long-running seller loop: NIP-42 AUTH, then subscribe 5109+1059 from START.
pub async fn run_forever(mut daemon: SellerDaemon) -> Result<(), DaemonError> {
    use std::time::Duration;
    use nostr_sdk::prelude::{Client, Filter, Kind, RelayPoolNotification, RelayUrl};

    let client = Client::new(daemon.keys.clone());
    // Default is true; set explicitly — seller receive depends on it.
    client.automatic_authentication(true);

    let relay_url_str = daemon.home.config.relay_url.clone();
    client
        .add_relay(&relay_url_str)
        .await
        .map_err(|error| DaemonError::Relay(format!("add relay: {error}")))?;

    let relay_url = RelayUrl::parse(&relay_url_str)
        .map_err(|error| DaemonError::Relay(format!("parse relay url: {error}")))?;
    let relay = client
        .relays()
        .await
        .get(&relay_url)
        .cloned()
        .ok_or_else(|| DaemonError::Relay("relay missing after add_relay".into()))?;

    // MUST subscribe before connect — Authenticated is not re-emitted.
    let mut relay_notifications = relay.notifications();
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(20)).await;
    wait_for_nip42_auth(&mut relay_notifications, Duration::from_secs(20)).await?;

    let offer_filter = Filter::new()
        .kind(Kind::Custom(JOB_OFFER_KIND))
        .pubkey(daemon.keys.public_key());
    let wrap_filter = Filter::new()
        .kind(Kind::GiftWrap)
        .pubkey(daemon.keys.public_key());

    client
        .subscribe(offer_filter, None)
        .await
        .map_err(|error| DaemonError::Relay(format!("subscribe 5109: {error}")))?;
    client
        .subscribe(wrap_filter, None)
        .await
        .map_err(|error| DaemonError::Relay(format!("subscribe 1059: {error}")))?;

    // Status line: never echo secrets.
    eprintln!(
        "seller daemon online pubkey={} relay={} mint={} nip42=authenticated",
        daemon.seller_pubkey(),
        daemon.home.config.relay_url,
        daemon.home.config.mint_url
    );

    let mut notifications = client.notifications();
    while let Ok(notification) = notifications.recv().await {
        match notification {
            RelayPoolNotification::Event { event, .. } => {
                if event.kind == Kind::GiftWrap {
                    match ingest_gift_wrap(&mut daemon, &event).await {
                        Ok(Some(receipt)) => {
                            eprintln!(
                                "seller receipt job_id={} result_id={} amount_received={}",
                                receipt.job_id, receipt.result_id, receipt.amount_received
                            );
                        }
                        Ok(None) => {}
                        Err(error) => eprintln!("seller pay path: {error}"),
                    }
                    continue;
                }
                if event.kind.as_u16() == JOB_OFFER_KIND {
                    match daemon.on_offer_event(&event).await {
                        Ok(Some(_)) => {
                            match daemon.execute_active_job().await {
                                Ok(result_id) => {
                                    eprintln!("seller published 6109 result_id={result_id}");
                                    match reconcile_after_result(&mut daemon).await {
                                        Ok(Some(receipt)) => eprintln!(
                                            "seller receipt (reconcile) job_id={} amount_received={}",
                                            receipt.job_id, receipt.amount_received
                                        ),
                                        Ok(None) => {}
                                        Err(error) => eprintln!("seller reconcile: {error}"),
                                    }
                                }
                                Err(error) => eprintln!("seller job failed: {error}"),
                            }
                        }
                        Ok(None) => {}
                        Err(error) => eprintln!("seller offer path: {error}"),
                    }
                }
            }
            RelayPoolNotification::Shutdown => break,
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::SellerConfig;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-seller-daemon-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn open_refuses_non_testnut_mint() {
        let root = temp("mint");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home.config.mint_url = "https://real-mint.example".into();
        home.config.seller = Some(SellerConfig {
            agent_command: vec!["echo".into()],
            rate_sats: 1,
            git_remote: "https://example.invalid/repo.git".into(),
            job_timeout_secs: None,
        });
        home::save_config(&home).expect("save");
        let err = match SellerDaemon::open(home) {
            Ok(_) => panic!("non-testnut must fail-closed"),
            Err(error) => error,
        };
        assert!(err.to_string().contains("fail-closed") || err.to_string().contains("testnut"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn open_requires_seller_section() {
        let root = temp("noseller");
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let err = match SellerDaemon::open(home) {
            Ok(_) => panic!("missing seller must refuse"),
            Err(error) => error,
        };
        assert!(err.to_string().contains("seller") || err.to_string().contains("agent_command"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn single_flight_mutex() {
        assert!(SellerDaemon::try_begin_flight());
        assert!(!SellerDaemon::try_begin_flight());
        SellerDaemon::end_flight();
        assert!(SellerDaemon::try_begin_flight());
        SellerDaemon::end_flight();
    }

    #[cfg(not(feature = "acp"))]
    #[tokio::test]
    async fn agent_run_fail_closed_without_acp_feature() {
        let err = run_agent_job(&["echo".into()], "task", Path::new(".")).await
            .expect_err("acp required");
        assert!(matches!(err, DaemonError::AcpRequired));
        assert!(err.to_string().contains("acp"));
    }
}
