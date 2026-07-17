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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(feature = "acp")]
use sha2::{Digest, Sha256};

use crate::buyer_fund::{self, FundError};
use crate::driver::UsageMetadata;
use crate::gateway::{
    self, claim_draft, error_draft, git_result_draft, parse_offer, EventDraft, ParsedOffer,
    TagSpec, JOB_OFFER_KIND,
};
use crate::home::{self, HomeError, MobeeHome, DEFAULT_MINT_URL};
use crate::job_lifecycle::{event_to_draft, job_hash_for_offer};
use crate::payment_send::ReceivedPayment;
use crate::payment_wallet::{CdkSellerReceive, PaymentPolicy, PaymentWalletError};
use crate::seller::{
    cashu_secret_from_nostr_hex, job_deadline_unix, plan_orphaned_claims, rate_gate_allows,
    require_seller_config, sign_receipt_hash, unwrap_own_payment_gift_wrap, ClaimLiveness,
    OrphanClaim, SellerError, SellerJournal,
};
use crate::seller_git::{self, SellerGitError};

/// In-flight single-flight lock for v1 (one job in the PROCESSING phase per process).
/// Held from claim through delivery (kind-6109), then released — a delivered-but-unpaid
/// job awaiting payment does NOT hold this lock (piece-11 #15 fix).
static FLIGHT: AtomicBool = AtomicBool::new(false);

/// Upper bound on delivered-but-unpaid jobs tracked concurrently (bounded memory).
/// Reaching it back-pressures new claims with a logged skip reason (never a silent drop).
const AWAITING_PAYMENT_CAP: usize = 16;

/// Item #16(c): bound on agent attempts per job. A transient agent error is retried up to
/// this many times, but only while the job deadline still has room — the retry loop never
/// outlives the deadline (see [`run_agent_with_retry`]).
const MAX_AGENT_ATTEMPTS: u32 = 3;

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

/// Whether a pay-path error is an EXPECTED idempotent re-see of an already-redeemed kind-1059
/// (the payment landed on an earlier delivery — a relay re-delivery of the gift-wrap, or a
/// restart). Two idempotent surfaces reach the pay-path log: the journal's pay-once guard
/// (`SellerError::Journal` "already receipted") and the mint reporting the proofs already spent
/// (cdk `TokenAlreadySpent`, surfaced as a `PaymentWalletError::Wallet` string). Both mean the
/// sats are already ours, so the re-see is logged at info, not error.
///
/// LOGGING classification ONLY — redeem, matching, reconcile and control flow are unchanged;
/// the error is still returned and handled identically, only the log line's severity differs.
fn is_idempotent_already_redeemed(error: &DaemonError) -> bool {
    // Journal pay-once guard: typed variant + our own stable message.
    if let DaemonError::Seller(SellerError::Journal(message)) = error {
        if message.to_ascii_lowercase().contains("already receipted") {
            return true;
        }
    }
    // Mint-level already-spent. cdk surfaces `TokenAlreadySpent` ("Token Already Spent") as a
    // string in `PaymentWalletError::Wallet` — there is no typed variant to match, so this
    // substring check is interim (TODO: expose a typed cdk-already-spent error to match on).
    let message = error.to_string().to_ascii_lowercase();
    message.contains("already spent") || message.contains("already redeemed")
}

/// (g) Collect-leg observability: the single, key-material-free line logged the moment a
/// kind-1059 payment is redeemed (proofs swapped at the mint), so the collect leg is
/// diagnosable in the daemon's stderr. NEVER includes the token or any secret.
fn collect_ok_log_line(
    job_id: &str,
    result_id: &str,
    amount_received: u64,
    expected: u64,
    mint: &str,
) -> String {
    format!(
        "seller collect ok: job_id={job_id} result_id={result_id} \
         amount_received={amount_received} expected={expected} mint={mint}"
    )
}

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

/// What [`SellerDaemon::classify_offer`] decided to do with an offer event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferDisposition {
    /// Admitted — claim the offer (single-flight reservation happens next).
    Claim(ClaimIntent),
    /// Not claimed — carries a named, loggable reason (never a silent drop).
    Skip(OfferSkip),
}

/// Everything needed to publish a claim + journal it, resolved without relay I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimIntent {
    pub job_id: String,
    pub buyer_pubkey: String,
    pub offer: ParsedOffer,
    pub deadline_unix: u64,
}

/// Enumerated reasons an offer is not claimed. Every variant maps to a logged reason —
/// there is no silent-drop path (piece-11 #15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferSkip {
    /// Event is not a kind-5109 offer.
    NotAnOffer { kind: u16 },
    /// Offer tags did not parse.
    Unparseable,
    /// Offer mint is not the fail-closed testnut mint.
    NonTestnutMint { mint_url: String },
    /// Rate-gate refused (not targeted to us / below rate / untargeted without opt-in).
    RateGate { reason: String },
    /// Journal already has a claim/receipt/release for this job (dedup).
    AlreadyProcessed,
    /// A job is already in the PROCESSING phase (single-flight). NOT triggered by
    /// delivered-but-unpaid jobs — those await payment without holding the slot.
    ProcessingBusy { job_id: String },
    /// Too many delivered-but-unpaid jobs pending payment (bounded-memory back-pressure).
    AwaitingPaymentFull { capacity: usize },
}

impl OfferSkip {
    /// Human-readable skip reason for logging (never empty).
    pub fn reason(&self) -> String {
        match self {
            Self::NotAnOffer { kind } => format!("not a kind-{JOB_OFFER_KIND} offer (kind {kind})"),
            Self::Unparseable => "offer tags did not parse".to_string(),
            Self::NonTestnutMint { mint_url } => format!("non-testnut mint {mint_url}"),
            Self::RateGate { reason } => format!("rate-gate: {reason}"),
            Self::AlreadyProcessed => "already claimed/receipted/released (journal dedup)".to_string(),
            Self::ProcessingBusy { job_id } => {
                format!("single-flight busy: job {job_id} is in the processing phase")
            }
            Self::AwaitingPaymentFull { capacity } => {
                format!("awaiting-payment backlog full (cap {capacity}); back-pressuring new claims")
            }
        }
    }
}

/// Reason string journaled + surfaced when a claim is released during restart-reconcile.
fn reconcile_reason(liveness: ClaimLiveness) -> &'static str {
    match liveness {
        ClaimLiveness::Expired => "claim expired before daemon restart (deadline passed, unpaid)",
        ClaimLiveness::Live => {
            "daemon restarted mid-execution; live claim released (v1 does not resume in-memory job state)"
        }
    }
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
    /// The PROCESSING-phase job (holds single-flight). `None` when idle or only awaiting pay.
    active: Option<ActiveJob>,
    /// DELIVERED-but-unpaid jobs (kind-6109 published, payment not yet redeemed). These do
    /// NOT hold single-flight, so new offers can still be claimed while payment is pending.
    awaiting_payment: Vec<ActiveJob>,
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
            awaiting_payment: Vec::new(),
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
    ///
    /// Skips are NEVER silent — every non-claim path is logged with its reason
    /// ([`OfferSkip::reason`]). A delivered-but-unpaid job does not block here (its
    /// binding lives in `awaiting_payment`, not the single-flight slot).
    pub async fn on_offer_event(
        &mut self,
        event: &nostr_sdk::Event,
    ) -> Result<Option<&ActiveJob>, DaemonError> {
        let now = now_unix();
        let intent = match self.classify_offer(event, now)? {
            OfferDisposition::Skip(skip) => {
                eprintln!("seller skip offer {}: {}", event.id.to_hex(), skip.reason());
                return Ok(None);
            }
            OfferDisposition::Claim(intent) => intent,
        };

        // Atomic reservation of the PROCESSING single-flight slot.
        if !Self::try_begin_flight() {
            eprintln!(
                "seller skip offer {}: single-flight busy (a job is already in the processing phase)",
                event.id.to_hex()
            );
            return Ok(None);
        }

        let claim = claim_draft(&intent.job_id, &intent.buyer_pubkey, &self.seller_pubkey);
        let claim_id = match publish_draft(&self.home, &self.keys, &claim).await {
            Ok(id) => id,
            Err(error) => {
                Self::end_flight();
                return Err(error);
            }
        };
        // Journal the CLAIMED transition WITH the deadline/claim_id/buyer so a restart can
        // reconcile this claim without the relay (piece-11 restart-reconcile).
        if let Err(error) = self.journal.append_claim(
            &intent.job_id,
            &claim_id,
            &intent.buyer_pubkey,
            intent.deadline_unix,
        ) {
            Self::end_flight();
            return Err(error.into());
        }

        let workdir = job_workdir(&self.home, &intent.job_id);
        if let Err(error) = std::fs::create_dir_all(&workdir) {
            Self::end_flight();
            return Err(DaemonError::Seller(SellerError::Io(error.to_string())));
        }

        self.active = Some(ActiveJob {
            job_id: intent.job_id,
            buyer_pubkey: intent.buyer_pubkey,
            offer: intent.offer,
            claim_id,
            result_id: None,
            deadline_unix: intent.deadline_unix,
            workdir,
        });
        Ok(self.active.as_ref())
    }

    /// Decide, WITHOUT any relay I/O, whether an offer event should be claimed.
    ///
    /// `now` is injected so the deadline is a pure function of inputs. Single-flight is
    /// enforced ONLY for the PROCESSING slot (`self.active`): a delivered-but-unpaid job
    /// in `awaiting_payment` does not block (piece-11 #15 silent-drop fix).
    fn classify_offer(
        &self,
        event: &nostr_sdk::Event,
        now: u64,
    ) -> Result<OfferDisposition, DaemonError> {
        if event.kind.as_u16() != JOB_OFFER_KIND {
            return Ok(OfferDisposition::Skip(OfferSkip::NotAnOffer {
                kind: event.kind.as_u16(),
            }));
        }
        let draft = event_to_draft(event);
        let offer = match parse_offer(&draft) {
            Ok(offer) => offer,
            Err(_) => return Ok(OfferDisposition::Skip(OfferSkip::Unparseable)),
        };
        // Offer mint fail-closed to testnut (soft-skip so the daemon stays up).
        if offer.mint_url != DEFAULT_MINT_URL {
            return Ok(OfferDisposition::Skip(OfferSkip::NonTestnutMint {
                mint_url: offer.mint_url.clone(),
            }));
        }
        let seller_cfg = require_seller_config(&self.home)?;
        if let Err(error) = rate_gate_allows(
            &offer,
            &self.seller_pubkey,
            seller_cfg.rate_sats,
            seller_cfg.claim_open_pool,
        ) {
            return Ok(OfferDisposition::Skip(OfferSkip::RateGate {
                reason: error.to_string(),
            }));
        }
        let job_id = event.id.to_hex();
        if self.journal.has_claim(&job_id)? {
            return Ok(OfferDisposition::Skip(OfferSkip::AlreadyProcessed));
        }
        // Single-flight is for PROCESSING only. Delivered-but-unpaid jobs live in
        // `awaiting_payment` and MUST NOT block a new claim.
        if let Some(active) = &self.active {
            return Ok(OfferDisposition::Skip(OfferSkip::ProcessingBusy {
                job_id: active.job_id.clone(),
            }));
        }
        if self.awaiting_payment.len() >= AWAITING_PAYMENT_CAP {
            return Ok(OfferDisposition::Skip(OfferSkip::AwaitingPaymentFull {
                capacity: AWAITING_PAYMENT_CAP,
            }));
        }
        let deadline_unix = job_deadline_unix(&offer, seller_cfg, now);
        Ok(OfferDisposition::Claim(ClaimIntent {
            job_id,
            buyer_pubkey: event.pubkey.to_hex(),
            offer,
            deadline_unix,
        }))
    }

    /// DELIVERED transition: move the PROCESSING job to `awaiting_payment` and free the
    /// single-flight slot so new offers can be claimed while payment is pending (#15).
    /// The payment binding is preserved (job_id + result_id) for [`try_apply_payment`].
    fn mark_delivered(&mut self) {
        if let Some(job) = self.active.take() {
            // `result_id` was set by `execute_active_job` on the successful publish path.
            self.awaiting_payment.push(job);
            while self.awaiting_payment.len() > AWAITING_PAYMENT_CAP {
                let dropped = self.awaiting_payment.remove(0);
                eprintln!(
                    "seller drop awaiting-payment job_id={} (backlog cap {AWAITING_PAYMENT_CAP})",
                    dropped.job_id
                );
            }
        }
        Self::end_flight();
    }

    /// Pure restart-reconcile plan: orphaned in-flight claims (journaled, no receipt, no
    /// release) classified Expired/Live by the injected `now`. No relay, no wall-clock.
    pub fn reconcile_plan(&self, now: u64) -> Result<Vec<OrphanClaim>, DaemonError> {
        Ok(plan_orphaned_claims(&self.journal.entries()?, now))
    }

    /// Durable restart-reconcile (NO relay): journal a terminal RELEASE for every orphaned
    /// in-flight claim so it can never read live again and is never re-claimed. Idempotent.
    /// Returns the plan that was acted on.
    pub fn reconcile_journal(&mut self, now: u64) -> Result<Vec<OrphanClaim>, DaemonError> {
        let plan = self.reconcile_plan(now)?;
        for orphan in &plan {
            self.journal
                .append_release(&orphan.job_id, reconcile_reason(orphan.liveness))?;
        }
        Ok(plan)
    }

    /// Full startup reconcile: durable journal release (above) + best-effort kind-7000
    /// error to surface the dead claim to the buyer. Publish failure is logged, not fatal —
    /// the journal release is the durable guarantee; the buyer view also derives expiry.
    pub async fn reconcile_on_startup(
        &mut self,
        now: u64,
    ) -> Result<Vec<OrphanClaim>, DaemonError> {
        let plan = self.reconcile_journal(now)?;
        for orphan in &plan {
            let reason = reconcile_reason(orphan.liveness);
            let draft = error_draft(&orphan.job_id, &orphan.buyer_pubkey, &self.seller_pubkey);
            match publish_draft(&self.home, &self.keys, &draft).await {
                Ok(id) => eprintln!(
                    "seller reconcile: released orphaned claim job_id={} liveness={:?} kind7000={id} reason={reason}",
                    orphan.job_id, orphan.liveness
                ),
                Err(error) => eprintln!(
                    "seller reconcile: released orphaned claim job_id={} liveness={:?} (kind-7000 publish deferred: {error}) reason={reason}",
                    orphan.job_id, orphan.liveness
                ),
            }
        }
        Ok(plan)
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
        received: ReceivedPayment,
    ) -> Result<Option<ReceiptOutcome>, DaemonError> {
        // Bind to the delivered-but-unpaid job this payment declares (exact job + result).
        // Never scans `active` — the processing slot is not a payment target.
        let Some(idx) = self.awaiting_payment.iter().position(|job| {
            job.job_id == received.payload.job_id
                && job.result_id.as_deref() == Some(received.payload.result_id.as_str())
        }) else {
            // No delivered job binds this payment yet — caller should buffer.
            return Ok(None);
        };
        let job = self.awaiting_payment[idx].clone();
        let local_job = job.job_id.clone();
        let local_result = job
            .result_id
            .clone()
            .expect("awaiting-payment job always carries a result_id");
        let expected_amount = job.offer.amount;
        let mint = job.offer.mint_url.clone();
        let offer = job.offer.clone();

        let payload_job = received.payload.job_id.clone();
        let payload_result = received.payload.result_id.clone();
        // B2: bind BEFORE journal — wrong-job refuse (no misattribution). Matched by
        // construction above; kept as a defensive guard.
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
        // (g) collect-leg observability: sats redeemed at the mint (no key/token material).
        eprintln!(
            "{}",
            collect_ok_log_line(
                &local_job,
                &local_result,
                amount_received,
                expected_amount,
                &mint
            )
        );

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
        // PAID: drop the delivered binding. Single-flight was already freed at delivery.
        self.awaiting_payment.remove(idx);
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
        // Gate #10 (empty-base): stamp delivery identity into a fresh git workdir (no
        // harness commit). Deliver only if every commit is agent-authored + non-empty tree.
        // Do NOT capture before-OID on empty / require advancement — dogfood is agent-from-empty.
        let identity = seller_git::DeliveryAgentIdentity::for_seller(&self.seller_pubkey);
        if let Err(error) =
            seller_git::init_empty_delivery_workdir(&active.workdir, &self.home.root, &identity)
        {
            self.fail_active(&error.to_string()).await?;
            return Err(error.into());
        }
        let run_started = std::time::Instant::now();
        // Item #16(e): the daemon OWNS delivery — append explicit, secret-free instructions so
        // the agent commits its deliverable to git (the daemon pushes it) instead of guessing.
        let prompt = compose_agent_prompt(&active.offer.task, &seller_cfg.git_remote);
        // Item #16(c): retry a transient agent error while the deadline still has room. The
        // kind-7000 error (fail_active, below) is published only after the attempt budget or
        // the deadline is spent — a transient failure never burns the claim early.
        let run_result = run_agent_with_retry(
            active.deadline_unix,
            MAX_AGENT_ATTEMPTS,
            now_unix,
            |_attempt| {
                // Item #16(b): each attempt runs under the job's *remaining* deadline, not a
                // hardcoded 300s.
                let job_timeout = unified_job_timeout(active.deadline_unix, now_unix());
                run_agent_job(
                    &seller_cfg.agent_command,
                    &prompt,
                    &active.workdir,
                    &identity,
                    job_timeout,
                )
            },
        )
        .await;
        // Wall-time is always measurable; token/model/cost ride out on `usage` only when the
        // ACP driver actually surfaced them (absent-stays-absent → `None`).
        let wall_time_ms = run_started.elapsed().as_millis() as u64;
        let usage = match run_result {
            Ok(usage) => usage,
            Err(error) => {
                self.fail_active(&error.to_string()).await?;
                return Err(error);
            }
        };
        let after_oid = seller_git::try_head_oid(&active.workdir, &self.home.root);
        let _advanced = match seller_git::require_agent_authored_delivery(
            &active.workdir,
            &self.home.root,
            &identity,
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

        // NIP-98: key from 0600 file → git child env only (never argv / never logged).
        let push_secret = home::read_secret_key_hex(&self.home)?;
        let push_auth = seller_git::PushAuth {
            secret_key_hex: push_secret,
        };
        let commit = match seller_git::push_branch_with_auth(
            &active.workdir,
            &seller_cfg.git_remote,
            &branch,
            &self.home.root,
            Some(&push_auth),
        ) {
            Ok(oid) => oid,
            Err(error) => {
                // Display path must not echo the secret (SellerGitError is scrubbed).
                self.fail_active(&error.to_string()).await?;
                return Err(error.into());
            }
        };
        drop(push_auth);

        let job_hash = job_hash_for_offer(&active.job_id, &active.offer.task, active.offer.amount);
        // Piece-9 Item 1: the seller signs the RECEIPT PREIMAGE (binds the trade + the
        // delivered git object, D4) — not the bare job-hash. The buyer reconstructs this
        // exact preimage and co-signs it. `exec_metadata_commitment` is the empty marker:
        // exec-metadata is NOT covered by the co-signature (Item 2, seller-claimed).
        let preimage = crate::receipt::ReceiptPreimage {
            job_hash: job_hash.clone(),
            offer_id: active.job_id.clone(),
            amount: active.offer.amount,
            unit: "sat".to_owned(),
            mint: active.offer.mint_url.clone(),
            buyer_pubkey: active.buyer_pubkey.clone(),
            seller_pubkey: self.seller_pubkey.clone(),
            delivery_integrity_hash: commit.clone(),
            delivery_kind: crate::receipt::DeliveryKind::Fork.as_str().to_owned(),
            exec_metadata_commitment: crate::receipt::EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
        };
        let seller_sig = sign_receipt_hash(&self.keys, &preimage.digest_hex())?;
        // Piece-9 Item 2: harness-generic PUBLIC seller-claimed usage block (opportunistic;
        // absent fields stay absent). `usage` carries the ACP-native token/model/cost the driver
        // surfaced this run — `None` when the harness exposed nothing.
        let exec_metadata = seller_exec_metadata(
            &seller_cfg.agent_command,
            seller_cfg.agent.as_deref(),
            wall_time_ms,
            usage.as_ref(),
        );
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
            &exec_metadata,
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

/// Build the piece-9 Item-2 seller-claimed PUBLIC usage block for a kind-6109 result.
///
/// Per gudnuf's Q2 ruling this block is PUBLIC and harness-generic. It is **opportunistic**:
/// emit only fields the seller can source. `harness` is resolved from the configured preset
/// label (else the agent command — USAGE-MATRIX checkpoint-b), `wall_time` is measured, and
/// `metadata_trust=seller-claimed` is required whenever any field is present (anchor rule).
///
/// `usage_transport` reflects **reality**: when the ACP driver actually captured usage this
/// run it is that surface (`acp-native`); otherwise the harness's declared axis.
///
/// Token / model / cost tags are appended **only where the driver surfaced them**
/// (absent-stays-absent, never zero-filled — a fabricated `0` is worse than a rendered dash).
/// `total` = `input + output + reasoning` (locked rule); cache siblings are evidence and are
/// NEVER summed into `total`. When `usage` is `None` the block is exactly the pre-plumbing
/// four tags — legacy/no-capture trades stay honestly dashed.
fn seller_exec_metadata(
    agent_command: &[String],
    agent_preset: Option<&str>,
    wall_time_ms: u64,
    usage: Option<&UsageMetadata>,
) -> Vec<TagSpec> {
    let (harness, static_transport) = harness_and_transport(agent_command, agent_preset);
    let transport = usage
        .and_then(|u| u.transport)
        .map(|t| t.as_str())
        .unwrap_or(static_transport);
    let wall = wall_time_ms.to_string();

    let mut tags = vec![
        TagSpec::new(["harness", harness.as_str()]),
        TagSpec::new(["usage_transport", transport]),
        TagSpec::new(["metadata_trust", "seller-claimed"]),
        TagSpec::new(["wall_time", wall.as_str(), "ms"]),
    ];

    if let Some(u) = usage {
        if let Some(model) = &u.model {
            tags.push(TagSpec::new(["model", model.as_str()]));
        }
        // Own the string renders so the borrows outlive each `TagSpec::new` call.
        let total = u.total_tokens().map(|n| n.to_string());
        let input = u.input_tokens.map(|n| n.to_string());
        let output = u.output_tokens.map(|n| n.to_string());
        let reasoning = u.reasoning_tokens.map(|n| n.to_string());
        let cache_read = u.cache_read_tokens.map(|n| n.to_string());
        let cache_write = u.cache_write_tokens.map(|n| n.to_string());
        if let Some(v) = &total {
            tags.push(TagSpec::new(["tokens", v.as_str(), "total"]));
        }
        if let Some(v) = &input {
            tags.push(TagSpec::new(["tokens", v.as_str(), "input"]));
        }
        if let Some(v) = &output {
            tags.push(TagSpec::new(["tokens", v.as_str(), "output"]));
        }
        if let Some(v) = &reasoning {
            tags.push(TagSpec::new(["tokens", v.as_str(), "reasoning"]));
        }
        if let Some(v) = &cache_read {
            tags.push(TagSpec::new(["tokens", v.as_str(), "cache_read"]));
        }
        if let Some(v) = &cache_write {
            tags.push(TagSpec::new(["tokens", v.as_str(), "cache_write"]));
        }
        if let Some(cost) = &u.cost {
            tags.push(TagSpec::new([
                "cost",
                cost.amount.as_str(),
                "usd",
                cost.basis.as_str(),
            ]));
        }
    }

    tags
}

/// Best-effort harness id + usage transport (USAGE-MATRIX checkpoint-b).
///
/// The configured **preset label** (`claude`|`cursor`|`codex`, [`SellerConfig::agent`]) is the
/// authoritative harness/adapter identity and is preferred over argv inspection: presets launch
/// the ACP adapter via `npx <adapter-package>` (argv0 = `npx`), so an argv0-naive id emitted
/// `npx` — which the observatory (`harnessFamilyFromId`) maps to `harness_family="other"`, hiding
/// real claude/codex/cursor jobs on the dashboard. When no preset label is present (raw
/// `--agent-argv` power-user hatch) fall back to scanning the FULL adapter argv (not just argv0):
/// the adapter package name (e.g. `@agentclientprotocol/claude-agent-acp`) still carries the
/// family. Unknown ⇒ the command basename + the conservative `side-channel`.
fn harness_and_transport(
    agent_command: &[String],
    agent_preset: Option<&str>,
) -> (String, &'static str) {
    // Preset label is authoritative — resolve from the adapter identity, never argv0.
    if let Some(preset) = agent_preset {
        match preset.trim().to_ascii_lowercase().as_str() {
            "claude" => return ("claude-agent-acp".to_owned(), "side-channel"),
            "codex" => return ("codex-acp-ng".to_owned(), "acp-native"),
            "cursor" => return ("cursor-agent".to_owned(), "side-channel"),
            _ => {}
        }
    }
    // Hatch fallback: scan the FULL argv (adapter identity), not just argv0.
    let joined = agent_command.join(" ").to_ascii_lowercase();
    if joined.contains("codex") {
        ("codex-acp-ng".to_owned(), "acp-native")
    } else if joined.contains("cursor") {
        ("cursor-agent".to_owned(), "side-channel")
    } else if joined.contains("claude") {
        ("claude-agent-acp".to_owned(), "side-channel")
    } else {
        let program = agent_command.first().map(String::as_str).unwrap_or("");
        let basename = program.rsplit('/').next().unwrap_or(program);
        let harness = if basename.is_empty() {
            "unknown".to_owned()
        } else {
            basename.to_owned()
        };
        (harness, "side-channel")
    }
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

/// Item #16(b): the ONE coherent job timeout. The ACP driver's idle/response timeout is
/// derived from the job's own deadline (`--job-timeout-secs` → offer deadline → default, via
/// [`job_deadline_unix`]) so a job has a single predictable deadline. Before this the driver
/// used a hardcoded 300s idle-timeout that silently conflicted with `--job-timeout-secs`
/// (a live codex seller hung ~300s on an ACP request while the job deadline said otherwise).
/// Saturating: a non-positive remaining window yields `Duration::ZERO`, which fails the run
/// cleanly at the deadline rather than hanging.
fn unified_job_timeout(deadline_unix: u64, now_unix: u64) -> Duration {
    Duration::from_secs(deadline_unix.saturating_sub(now_unix))
}

/// Item #16(c): run the agent with bounded retries that stay WITHIN the job deadline.
///
/// A transient agent error is retried until either the attempt budget (`max_attempts`) is
/// spent OR the deadline (`deadline_unix`, checked against injected `now`) passes. The error
/// is surfaced to the caller — which then publishes the kind-7000 error exactly once — ONLY
/// after one of those limits is reached. This stops a transient failure from immediately
/// burning the claim while the deadline still has room (job 0867a213 failed where 4d982c54
/// paid). `run` is invoked with the 1-based attempt number and awaited to completion before
/// any retry, so attempts never overlap.
async fn run_agent_with_retry<F, Fut>(
    deadline_unix: u64,
    max_attempts: u32,
    now: impl Fn() -> u64,
    mut run: F,
) -> Result<Option<UsageMetadata>, DaemonError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<Option<UsageMetadata>, DaemonError>>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match run(attempt).await {
            Ok(usage) => return Ok(usage),
            // Retry only while BOTH an attempt and the deadline remain; otherwise surface the
            // error so the caller publishes kind-7000 exactly once (past deadline / exhausted).
            Err(_) if attempt < max_attempts && now() < deadline_unix => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Item #16(e): daemon-owned delivery. The daemon appends explicit, secret-free delivery
/// instructions to the agent's task prompt so the agent delivers by committing its work to
/// the git repository in its working directory — rather than guessing a delivery channel.
/// The daemon performs the authenticated push of the committed branch to the bound remote
/// (NIP-98; the agent is never handed a key), so this text carries NO secret — it is public
/// prompt text built only from the task and the (public) remote URL.
fn compose_agent_prompt(task: &str, git_remote: &str) -> String {
    format!(
        "{task}\n\n\
         ---\n\
         DELIVERY (required). Deliver your work by committing it with git in your current \
         working directory:\n\
         - Make one or more non-empty commits authored by you. Do not leave the deliverable \
         uncommitted and do not only print it to the console.\n\
         - You do NOT need to push and you are NOT handed any credentials: the daemon pushes \
         your committed branch to the bound git remote ({git_remote}) on your behalf.\n\
         Anything not committed to git will not be delivered."
    )
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
    prompt: &str,
    workdir: &Path,
    identity: &seller_git::DeliveryAgentIdentity,
    timeout: Duration,
) -> Result<Option<UsageMetadata>, DaemonError> {
    use crate::driver::{AcpDriver, AgentCommand, ContentBlock, PromptTurn, SessionConfig};
    use crate::engine::{RunParams, run_job};
    use crate::event::JobId;
    use crate::log::EventLog;

    if agent_command.is_empty() {
        return Err(DaemonError::Config("agent_command empty".into()));
    }
    // Item #16(b): the ACP idle/response timeout IS the unified job timeout — never a
    // hardcoded 300s that could override or conflict with `--job-timeout-secs`.
    let mut driver = AcpDriver::new(
        AgentCommand::new(agent_command[0].clone(), agent_command[1..].to_vec()),
        crate::driver::PermissionOutcome::Allow,
        timeout,
    );
    let log_path = workdir.join("seller-run.jsonl");
    let mut log = EventLog::open(&log_path)
        .map_err(|error| DaemonError::Agent(error.to_string()))?;
    let params = RunParams {
        session_config: SessionConfig {
            cwd: workdir.to_path_buf(),
            mcp_servers: Vec::new(),
            env: identity.git_env(),
        },
        prompt: PromptTurn {
            input: vec![ContentBlock::Text {
                text: prompt.to_owned(),
            }],
        },
    };
    let outcome = run_job(
        &mut driver,
        &mut log,
        &JobId(format!("seller-{}", short_hash(prompt))),
        params,
        &mut |_| {},
    )
    .await
    .map_err(|error| DaemonError::Agent(error.to_string()))?;
    match outcome.terminal {
        crate::event::JobExecutionStatus::Completed => Ok(outcome.usage),
        other => Err(DaemonError::Agent(format!("agent terminal {other:?}"))),
    }
}

#[cfg(not(feature = "acp"))]
async fn run_agent_job(
    _agent_command: &[String],
    _prompt: &str,
    _workdir: &Path,
    _identity: &seller_git::DeliveryAgentIdentity,
    _timeout: Duration,
) -> Result<Option<UsageMetadata>, DaemonError> {
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
    // Does a delivered-but-unpaid job bind this payment (exact job + result)?
    let binds = daemon.awaiting_payment.iter().any(|job| {
        job.job_id == received.payload.job_id
            && job.result_id.as_deref() == Some(received.payload.result_id.as_str())
    });
    if !binds {
        // No delivered job matches yet — buffer it (early pay for a still-processing job, or
        // the wrap arrived before its delivery was recorded). Misattribution is impossible:
        // `try_apply_payment` only receives against an exact job+result match.
        daemon.buffer_payment(event_id, received);
        return Ok(ApplyResult::Buffered);
    }
    match daemon.try_apply_payment(received).await? {
        Some(outcome) => Ok(ApplyResult::Applied(outcome)),
        None => Ok(ApplyResult::Buffered),
    }
}

/// Reconcile buffered payments after 6109 publish (B2 early-pay). Applies every buffered
/// wrap that now binds a delivered job; leaves the rest buffered. Returns the last receipt.
pub async fn reconcile_after_result(
    daemon: &mut SellerDaemon,
) -> Result<Option<ReceiptOutcome>, DaemonError> {
    // Drain into a snapshot; unmatched wraps are re-buffered into the (now empty) pay_buffer.
    let mut pending = std::mem::take(&mut daemon.pay_buffer);
    let mut done = None;
    while let Some(BufferedPay { event_id, received }) = pending.pop_front() {
        match try_apply_or_buffer(daemon, event_id, received).await? {
            ApplyResult::Applied(outcome) => done = Some(outcome),
            ApplyResult::Buffered => {}
        }
    }
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

/// Build the seller's kind-5109 offer subscription filter(s).
///
/// Always includes the TARGETED filter (`#p` == seller pubkey). When `claim_open_pool` is set,
/// ALSO returns an UNtargeted filter (no pubkey pin): open-pool offers carry no `p` tag, so a
/// pubkey-pinned filter alone never delivers them and `--claim-open-pool` is DOA. A targeted
/// offer matches BOTH filters (deduped by event id at the call site); the downstream rate-gate
/// (`rate_gate_allows`) still decides whether an untargeted offer is actually claimed.
fn offer_subscription_filters(
    seller_pubkey: nostr_sdk::PublicKey,
    claim_open_pool: bool,
) -> Vec<nostr_sdk::Filter> {
    use nostr_sdk::prelude::{Filter, Kind};
    let mut filters = vec![Filter::new()
        .kind(Kind::Custom(JOB_OFFER_KIND))
        .pubkey(seller_pubkey)];
    if claim_open_pool {
        // Second, un-pinned filter: matches untargeted (open-pool) 5109 offers.
        filters.push(Filter::new().kind(Kind::Custom(JOB_OFFER_KIND)));
    }
    filters
}

/// The seller's LIVE offer subscription(s), grouped as they are registered on the relay.
///
/// Each element is ONE long-lived subscription — a single NIP-01 `REQ` whose filters the relay
/// OR-matches. The 5109 offer filters are grouped into ONE subscription: the pinned (`#p` ==
/// self) filter AND — under `claim_open_pool` — the un-pinned open-pool filter ride the SAME
/// `REQ`. This grouping is load-bearing: the earlier half-fix registered the un-pinned filter as
/// a SEPARATE second subscription, which delivered stored events (backfill) but no LIVE offers —
/// a running open-pool seller never reacted to a fresh untargeted offer, only claiming it after
/// a restart re-fetched it from stored events. Callers MUST subscribe each group as one `REQ`
/// (one `pool().subscribe(filters, ..)` call), never one subscription per filter.
fn offer_subscriptions(
    seller_pubkey: nostr_sdk::PublicKey,
    claim_open_pool: bool,
) -> Vec<Vec<nostr_sdk::Filter>> {
    vec![offer_subscription_filters(seller_pubkey, claim_open_pool)]
}

/// Whether a relay event in the seller loop should be handed to `on_offer_event`.
///
/// True iff the event is a kind-5109 offer not seen before. Routing is by KIND ONLY: a
/// non-p-tagged (open-pool) offer routes exactly like a targeted one, so the notification path
/// never drops untargeted offers. The event-id dedup makes a targeted offer that matched more
/// than one 5109 filter (or a reconnect re-delivery) reach `on_offer_event` at most once.
fn offer_event_should_process(
    event_kind: u16,
    event_id: nostr_sdk::EventId,
    seen_offers: &mut std::collections::HashSet<nostr_sdk::EventId>,
) -> bool {
    event_kind == JOB_OFFER_KIND && seen_offers.insert(event_id)
}

/// Long-running seller loop: NIP-42 AUTH, then subscribe 5109+1059 from START.
pub async fn run_forever(mut daemon: SellerDaemon) -> Result<(), DaemonError> {
    use std::time::Duration;
    use nostr_sdk::prelude::{
        Client, Filter, Kind, RelayPoolNotification, RelayUrl, SubscribeOptions,
    };

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

    // Restart-reconcile: release any orphaned in-flight claims from a prior run BEFORE
    // serving new offers, so a claim left live by a crash never reads "processing" forever
    // (evidence job 0867a213). Durable via journal; kind-7000 surface is best-effort.
    match daemon.reconcile_on_startup(now_unix()).await {
        Ok(plan) if !plan.is_empty() => {
            eprintln!(
                "seller reconcile: released {} orphaned claim(s) on startup",
                plan.len()
            );
        }
        Ok(_) => {}
        Err(error) => eprintln!("seller reconcile failed on startup (continuing): {error}"),
    }

    // Offer subscription: always the TARGETED filter (p-tag == seller). When the seller opts
    // into the open pool, the offer subscription ALSO carries an UNtargeted 5109 filter —
    // open-pool offers carry no p-tag and would otherwise never reach `on_offer_event`, so
    // `--claim-open-pool` was DOA. BOTH filters ride ONE long-lived subscription (a single REQ,
    // OR-matched per NIP-01): registered as a SEPARATE second subscription (the earlier
    // half-fix) the un-pinned filter delivered stored events (backfill) but never LIVE offers,
    // so a running open-pool seller ignored fresh untargeted offers. `offer_subscriptions`
    // groups them into that single subscription; subscribe each group as ONE REQ via
    // `pool().subscribe` (`Client::subscribe` takes a single filter — one REQ per filter is the
    // bug). The event-id dedup in the loop below still processes each offer exactly once.
    let claim_open_pool = require_seller_config(&daemon.home)
        .map(|cfg| cfg.claim_open_pool)
        .unwrap_or(false);
    for filters in offer_subscriptions(daemon.keys.public_key(), claim_open_pool) {
        client
            .pool()
            .subscribe(filters, SubscribeOptions::default())
            .await
            .map_err(|error| DaemonError::Relay(format!("subscribe 5109: {error}")))?;
    }
    let wrap_filter = Filter::new()
        .kind(Kind::GiftWrap)
        .pubkey(daemon.keys.public_key());
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

    // Both 5109 filters ride ONE subscription, so the relay delivers each offer once even when
    // a targeted offer matches both filters. Keep an event-id dedup as defense-in-depth (e.g.
    // reconnect re-delivery) so each offer id reaches `on_offer_event` at most once.
    let mut seen_offers: std::collections::HashSet<nostr_sdk::EventId> =
        std::collections::HashSet::new();
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
                        // Idempotent re-see (info, not error): the sats already landed.
                        Err(error) if is_idempotent_already_redeemed(&error) => eprintln!(
                            "seller pay: kind-1059 already redeemed (idempotent re-see, no action): {error}"
                        ),
                        Err(error) => eprintln!("seller pay path: {error}"),
                    }
                    continue;
                }
                // An offer (kind-5109) routes to `on_offer_event` REGARDLESS of p-tag — open-pool
                // offers carry none, so a p-tag gate here would silently drop them. Deduped by
                // event id so each offer is processed once (see `offer_event_should_process`).
                if offer_event_should_process(event.kind.as_u16(), event.id, &mut seen_offers) {
                    match daemon.on_offer_event(&event).await {
                        Ok(Some(_)) => {
                            match daemon.execute_active_job().await {
                                Ok(result_id) => {
                                    eprintln!("seller published 6109 result_id={result_id}");
                                    // DELIVERED: free the single-flight slot so new offers can
                                    // be claimed while this job awaits payment (#15).
                                    daemon.mark_delivered();
                                    match reconcile_after_result(&mut daemon).await {
                                        Ok(Some(receipt)) => eprintln!(
                                            "seller receipt (reconcile) job_id={} amount_received={}",
                                            receipt.job_id, receipt.amount_received
                                        ),
                                        Ok(None) => {}
                                        // Idempotent re-see (info, not error): sats already landed.
                                        Err(error) if is_idempotent_already_redeemed(&error) => eprintln!(
                                            "seller reconcile: kind-1059 already redeemed (idempotent re-see, no action): {error}"
                                        ),
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
            agent: None,
            claim_open_pool: false,
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

    // Item #16(b): the ACP timeout is unified with `--job-timeout-secs` — one deadline.
    #[test]
    fn unified_job_timeout_is_the_remaining_deadline_not_a_hardcoded_constant() {
        // The effective timeout is strictly the remaining window to the job's deadline.
        assert_eq!(unified_job_timeout(1_000, 940), Duration::from_secs(60));
        assert_eq!(unified_job_timeout(1_000, 100), Duration::from_secs(900));
        // Two different deadlines ⇒ two different timeouts — proves it is DERIVED from the
        // deadline, not a fixed 300s that could override or conflict with `--job-timeout-secs`.
        assert_ne!(
            unified_job_timeout(1_000, 940),
            unified_job_timeout(1_000, 100)
        );
        assert_ne!(unified_job_timeout(1_000, 940), Duration::from_secs(300));
        // At/past the deadline ⇒ ZERO (fail cleanly at the deadline, never hang, never wrap).
        assert_eq!(unified_job_timeout(1_000, 1_000), Duration::ZERO);
        assert_eq!(unified_job_timeout(1_000, 5_000), Duration::ZERO);
    }

    // Item #16(c): a transient agent error is retried WITHIN the deadline; kind-7000 is
    // published only after the attempt budget or the deadline is spent.
    #[tokio::test]
    async fn retry_recovers_from_a_transient_error_within_the_deadline() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        // Deadline far away ⇒ never the limiter; a transient first error must be retried,
        // NOT burn the claim (publish 7000) while the deadline still has room.
        let out = run_agent_with_retry(u64::MAX, 3, || 0, |attempt| {
            attempts.set(attempt);
            async move {
                if attempt < 2 {
                    Err(DaemonError::Agent("transient".into()))
                } else {
                    Ok::<Option<UsageMetadata>, DaemonError>(None)
                }
            }
        })
        .await;
        assert!(out.is_ok(), "transient error retried within deadline, not fatal: {out:?}");
        assert_eq!(attempts.get(), 2, "retried once, then succeeded");
    }

    #[tokio::test]
    async fn retry_exhausts_bounded_attempts_then_surfaces_the_error() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        // Deadline never the limiter (u64::MAX) — only the attempt budget stops the loop.
        let out = run_agent_with_retry(u64::MAX, 3, || 0, |attempt| {
            attempts.set(attempt);
            async move {
                Err::<Option<UsageMetadata>, DaemonError>(DaemonError::Agent("always".into()))
            }
        })
        .await;
        assert!(out.is_err(), "exhausted retries ⇒ error so caller publishes kind-7000");
        assert_eq!(attempts.get(), 3, "bounded to the attempt budget");
    }

    #[tokio::test]
    async fn retry_past_deadline_makes_one_attempt_then_surfaces_the_error() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        // `now` (5_000) is already past the deadline (1_000) ⇒ no retry budget at all: one
        // attempt, then the error surfaces so the caller publishes kind-7000.
        let out = run_agent_with_retry(1_000, 3, || 5_000, |attempt| {
            attempts.set(attempt);
            async move {
                Err::<Option<UsageMetadata>, DaemonError>(DaemonError::Agent("late".into()))
            }
        })
        .await;
        assert!(out.is_err(), "past deadline ⇒ error (caller publishes kind-7000)");
        assert_eq!(attempts.get(), 1, "no retry once the deadline has passed");
    }

    // Item #16(e): the daemon appends explicit, secret-free delivery instructions.
    #[test]
    fn composed_prompt_carries_task_and_daemon_owned_delivery_instructions() {
        let remote = "https://relay.example/git/abc.git";
        let prompt = compose_agent_prompt("build a widget", remote);
        // The original task stays up front.
        assert!(prompt.starts_with("build a widget"), "task preserved: {prompt}");
        // Explicit, daemon-owned delivery instructions are appended.
        assert!(prompt.contains("DELIVERY"), "has a delivery section: {prompt}");
        assert!(
            prompt.contains("commit") || prompt.contains("Commit"),
            "tells the agent to commit: {prompt}"
        );
        assert!(prompt.contains("git"), "delivery is via git: {prompt}");
        assert!(
            prompt.contains(remote),
            "names the bound remote so delivery is not guessed: {prompt}"
        );
        // Public prompt text — never embeds a secret.
        let lower = prompt.to_lowercase();
        assert!(!prompt.contains("nsec"), "no nostr secret key");
        assert!(!lower.contains("private key"), "no private key");
        assert!(!lower.contains("secret"), "no secret material");
    }

    #[test]
    fn seller_exec_metadata_is_harness_generic_public_and_absent_stays_absent() {
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        // claude ⇒ side-channel; codex ⇒ acp-native; unknown ⇒ basename + side-channel.
        // `None` usage: the pre-capture block — token/model/cost stay absent.
        let claude = seller_exec_metadata(&["claude".into(), "--print".into()], None, 1234, None);
        assert_eq!(value(&claude, "harness").as_deref(), Some("claude-agent-acp"));
        assert_eq!(value(&claude, "usage_transport").as_deref(), Some("side-channel"));
        // Anchor rule: metadata_trust present whenever any field is present.
        assert_eq!(value(&claude, "metadata_trust").as_deref(), Some("seller-claimed"));
        assert_eq!(value(&claude, "wall_time").as_deref(), Some("1234"));
        // Absent-stays-absent: no zero-filled token/model/cost fields (not sourced this run).
        assert!(value(&claude, "tokens").is_none());
        assert!(value(&claude, "model").is_none());
        assert!(value(&claude, "cost").is_none());

        let codex = seller_exec_metadata(&["/nix/store/x/bin/codex-acp".into()], None, 5, None);
        assert_eq!(value(&codex, "harness").as_deref(), Some("codex-acp-ng"));
        assert_eq!(value(&codex, "usage_transport").as_deref(), Some("acp-native"));

        let unknown = seller_exec_metadata(&["/opt/tools/mytool".into()], None, 5, None);
        assert_eq!(value(&unknown, "harness").as_deref(), Some("mytool"));
        assert_eq!(value(&unknown, "usage_transport").as_deref(), Some("side-channel"));
    }

    #[test]
    fn claude_preset_resolves_harness_family_claude_despite_npx_argv0() {
        // Mirror the observatory reader (web/network/js/parse.js `harnessFamilyFromId`):
        // a family substring wins; present-but-unrecognized (e.g. "npx") → "other".
        fn harness_family(id: &str) -> &'static str {
            let s = id.to_ascii_lowercase();
            if s.contains("claude") {
                "claude"
            } else if s.contains("cursor") {
                "cursor"
            } else if s.contains("codex") {
                "codex"
            } else {
                "other"
            }
        }
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        // The `claude` preset launches the ACP adapter via `npx` (argv0 = "npx"). An argv0-naive
        // id emits "npx" → harness_family "other" (the gudnuf-visible dashboard bug). The preset
        // label must drive resolution to "claude-agent-acp" → family "claude".
        let npx_claude = vec![
            "/usr/bin/npx".to_string(),
            "-y".to_string(),
            "@agentclientprotocol/claude-agent-acp".to_string(),
        ];
        let tags = seller_exec_metadata(&npx_claude, Some("claude"), 100, None);
        let harness = value(&tags, "harness").expect("harness tag");
        assert_eq!(harness, "claude-agent-acp");
        assert_eq!(
            harness_family(&harness),
            "claude",
            "claude preset must map to harness_family 'claude', not 'other'"
        );

        // Preset label is authoritative even when the argv carries no family hint at all.
        let opaque = vec![
            "/usr/bin/npx".to_string(),
            "-y".to_string(),
            "@acp/opaque-adapter".to_string(),
        ];
        let opaque_tags = seller_exec_metadata(&opaque, Some("claude"), 100, None);
        assert_eq!(
            harness_family(&value(&opaque_tags, "harness").expect("harness")),
            "claude"
        );

        // Regression guard: bare argv0 = "npx" with NO preset label used to yield "other";
        // the full-argv fallback now recovers "claude" from the adapter package name.
        let hatch = seller_exec_metadata(&npx_claude, None, 100, None);
        assert_eq!(
            harness_family(&value(&hatch, "harness").expect("harness")),
            "claude"
        );
    }

    #[test]
    fn open_pool_filter_lands_untargeted_offer_only_when_enabled() {
        use nostr_sdk::prelude::{EventBuilder, Filter, Keys, Kind, MatchEventOptions, Tag};

        let seller = Keys::generate();
        let buyer = Keys::generate();

        // A TARGETED offer carries a p-tag == seller; an UNtargeted (open-pool) offer has none.
        let targeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(Tag::public_key(seller.public_key()))
            .sign_with_keys(&buyer)
            .expect("sign targeted offer");
        let untargeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .sign_with_keys(&buyer)
            .expect("sign untargeted offer");

        let matches_any = |filters: &[Filter], event: &nostr_sdk::Event| {
            filters
                .iter()
                .any(|filter| filter.match_event(event, MatchEventOptions::new()))
        };

        // Targeted-only (claim_open_pool = false): the targeted offer matches, the untargeted
        // (open-pool) offer does NOT — this is exactly why --claim-open-pool was DOA.
        let targeted_only = offer_subscription_filters(seller.public_key(), false);
        assert!(
            matches_any(&targeted_only, &targeted),
            "targeted offer must match the pinned filter"
        );
        assert!(
            !matches_any(&targeted_only, &untargeted),
            "untargeted offer must NOT match without open-pool"
        );

        // Open-pool (claim_open_pool = true): the 2nd un-pinned filter lands the untargeted
        // offer. RED-ON-REVERT: drop the `filters.push(...)` in offer_subscription_filters and
        // this final assert fails — the untargeted offer no longer matches, so no claim fires.
        let open_pool = offer_subscription_filters(seller.public_key(), true);
        assert!(
            matches_any(&open_pool, &targeted),
            "targeted offer still matches under open-pool"
        );
        assert!(
            matches_any(&open_pool, &untargeted),
            "untargeted offer MUST match under open-pool (the fix)"
        );
    }

    // The half-fix above proved the un-pinned filter is CONSTRUCTED, but not that it is
    // registered on the LIVE subscription — the un-pinned filter had been subscribed as a
    // SEPARATE second subscription, which the relay tore down after its stored-events flush, so
    // it streamed backfill offers but never LIVE ones. These two tests cover the actual
    // delivery seam: (1) both 5109 filters ride ONE live subscription; (2) a non-p-tagged 5109
    // routes to `on_offer_event` and is deduped. The full live proof (untargeted offer to a
    // RUNNING seller → claim without restart) is exercised end-to-end against a real relay.
    #[test]
    fn open_pool_offer_rides_a_single_live_subscription() {
        use nostr_sdk::prelude::{EventBuilder, Filter, Keys, Kind, MatchEventOptions, Tag};

        let seller = Keys::generate();
        let buyer = Keys::generate();

        let targeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(Tag::public_key(seller.public_key()))
            .sign_with_keys(&buyer)
            .expect("sign targeted offer");
        let untargeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .sign_with_keys(&buyer)
            .expect("sign untargeted offer");

        let matches_any = |filters: &[Filter], event: &nostr_sdk::Event| {
            filters
                .iter()
                .any(|filter| filter.match_event(event, MatchEventOptions::new()))
        };

        // Open-pool: the offer filters are registered as EXACTLY ONE live subscription (a single
        // REQ) carrying BOTH the pinned and the un-pinned filter, so the un-pinned filter rides
        // the same long-lived subscription and streams LIVE offers. The half-fix registered the
        // un-pinned filter as a SEPARATE second subscription (two subscriptions), which the relay
        // dropped after backfill. RED-ON-REVERT: return one subscription per filter (as the
        // half-fix did) and this `len() == 1` assertion fails.
        let subs = offer_subscriptions(seller.public_key(), true);
        assert_eq!(
            subs.len(),
            1,
            "open-pool offers must ride ONE live subscription, not a separate un-pinned one"
        );
        let live = &subs[0];
        assert!(
            matches_any(live, &targeted),
            "the single live subscription must match targeted offers"
        );
        assert!(
            matches_any(live, &untargeted),
            "the single live subscription must ALSO match untargeted (open-pool) offers"
        );

        // Targeted-only: still one subscription; matches targeted, not untargeted.
        let subs = offer_subscriptions(seller.public_key(), false);
        assert_eq!(subs.len(), 1, "one offer subscription when not open-pool");
        assert!(matches_any(&subs[0], &targeted));
        assert!(
            !matches_any(&subs[0], &untargeted),
            "untargeted offers must not match without open-pool"
        );
    }

    #[test]
    fn untargeted_offer_routes_to_on_offer_event_and_dedups() {
        use nostr_sdk::prelude::{EventBuilder, Keys, Kind};
        use std::collections::HashSet;

        let buyer = Keys::generate();
        // An UNtargeted (open-pool) 5109 offer carries no p-tag.
        let untargeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .sign_with_keys(&buyer)
            .expect("sign untargeted offer");

        let mut seen: HashSet<nostr_sdk::EventId> = HashSet::new();

        // A non-p-tagged 5109 offer MUST route to `on_offer_event` on the LIVE push — this rules
        // out the "notification path drops non-p-tagged 5109" failure mode. RED-ON-REVERT: gate
        // routing on a p-tag and this first assertion fails for the untargeted offer.
        assert!(
            offer_event_should_process(untargeted.kind.as_u16(), untargeted.id, &mut seen),
            "an untargeted 5109 offer must route to on_offer_event"
        );
        // Dedup by event id: a re-delivered offer id is processed at most once.
        assert!(
            !offer_event_should_process(untargeted.kind.as_u16(), untargeted.id, &mut seen),
            "a re-delivered offer id must be deduped, not double-processed"
        );
        // A non-offer kind (e.g. gift-wrap 1059) does not route as an offer.
        assert!(
            !offer_event_should_process(Kind::GiftWrap.as_u16(), untargeted.id, &mut seen),
            "non-5109 events must not route to on_offer_event"
        );
    }

    #[test]
    fn already_redeemed_1059_classified_info_not_error() {
        // Journal pay-once guard = idempotent already-redeemed re-see → info, not error.
        let journal_dup = DaemonError::Seller(SellerError::Journal(
            "job abcd already receipted (pay-once)".into(),
        ));
        assert!(
            is_idempotent_already_redeemed(&journal_dup),
            "journal pay-once re-see is idempotent (logged info)"
        );

        // Mint says the proofs are already spent (cdk TokenAlreadySpent) → info, not error.
        let mint_spent =
            DaemonError::Wallet(PaymentWalletError::Wallet("Token Already Spent".into()));
        assert!(
            is_idempotent_already_redeemed(&mint_spent),
            "mint already-spent re-see is idempotent (logged info)"
        );

        // Genuine failures are NOT downgraded — they stay on the error channel.
        assert!(!is_idempotent_already_redeemed(&DaemonError::Relay(
            "connection refused".into()
        )));
        assert!(!is_idempotent_already_redeemed(&DaemonError::Policy(
            "payment bind refused: payload job/result mismatch".into()
        )));
    }

    #[test]
    fn collect_ok_log_line_carries_amount_and_no_key_material() {
        let line = collect_ok_log_line("job1", "res1", 5, 5, "https://testnut.example");
        assert!(
            line.contains("amount_received=5"),
            "collect log must surface the collected amount"
        );
        assert!(line.contains("job_id=job1") && line.contains("result_id=res1"));
        assert!(line.contains("mint=https://testnut.example"));
        // No key/token material ever in the collect log.
        let lower = line.to_ascii_lowercase();
        assert!(
            !lower.contains("token") && !lower.contains("secret") && !lower.contains("nsec"),
            "collect log must never carry token/key material"
        );
    }

    #[test]
    fn seller_exec_metadata_emits_captured_usage_into_result_tags() {
        use crate::driver::{UsageCost, UsageMetadata, UsageTransport};

        // A tag qualified by cell index 1 (value) + cell 2 (qualifier), e.g. ["tokens","140","total"].
        let qualified = |tags: &[TagSpec], name: &str, qualifier: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name) && tag.0.get(2).map(String::as_str) == Some(qualifier))
                .and_then(|tag| tag.value().map(str::to_owned))
        };
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        let usage = UsageMetadata {
            model: Some("claude-opus-4-8".into()),
            input_tokens: Some(100),
            output_tokens: Some(40),
            reasoning_tokens: None,
            cache_read_tokens: Some(4096),
            cache_write_tokens: Some(512),
            cost: Some(UsageCost {
                amount: "0.0123".into(),
                basis: "harness-reported-usd".into(),
            }),
            transport: Some(UsageTransport::AcpNative),
        };
        // claude command would statically declare side-channel; a REAL acp-native capture wins.
        let tags = seller_exec_metadata(&["claude".into()], None, 4321, Some(&usage));

        assert_eq!(value(&tags, "usage_transport").as_deref(), Some("acp-native"));
        assert_eq!(value(&tags, "model").as_deref(), Some("claude-opus-4-8"));
        // total = input + output (reasoning absent = unknown, not zero); cache NOT folded in.
        assert_eq!(qualified(&tags, "tokens", "total").as_deref(), Some("140"));
        assert_eq!(qualified(&tags, "tokens", "input").as_deref(), Some("100"));
        assert_eq!(qualified(&tags, "tokens", "output").as_deref(), Some("40"));
        assert_eq!(qualified(&tags, "tokens", "reasoning"), None);
        assert_eq!(qualified(&tags, "tokens", "cache_read").as_deref(), Some("4096"));
        assert_eq!(qualified(&tags, "tokens", "cache_write").as_deref(), Some("512"));
        // cost tag: ["cost","<amount>","usd","<basis>"].
        let cost = tags
            .iter()
            .find(|t| t.first() == Some("cost"))
            .expect("cost tag");
        assert_eq!(cost.0, vec!["cost", "0.0123", "usd", "harness-reported-usd"]);

        // Partial capture (output only) → NO total tag (a partial never masquerades as complete).
        let partial = UsageMetadata {
            output_tokens: Some(40),
            transport: Some(UsageTransport::AcpNative),
            ..UsageMetadata::default()
        };
        let partial_tags = seller_exec_metadata(&["claude".into()], None, 1, Some(&partial));
        assert_eq!(qualified(&partial_tags, "tokens", "total"), None);
        assert_eq!(qualified(&partial_tags, "tokens", "output").as_deref(), Some("40"));
    }

    fn sample_offer(amount: u64, seller: &str) -> ParsedOffer {
        ParsedOffer {
            task: "task".into(),
            output: "text/plain".into(),
            amount,
            unit: "sat".into(),
            deadline_unix: 2_000_000_000,
            mint_url: DEFAULT_MINT_URL.into(),
            seller_pubkey: Some(seller.to_owned()),
        }
    }

    fn active_job(
        job_id: &str,
        seller: &str,
        result_id: Option<&str>,
        deadline: u64,
        root: &Path,
    ) -> ActiveJob {
        ActiveJob {
            job_id: job_id.into(),
            buyer_pubkey: "bb".repeat(32),
            offer: sample_offer(5, seller),
            claim_id: format!("claim-{job_id}"),
            result_id: result_id.map(str::to_owned),
            deadline_unix: deadline,
            workdir: root.join(job_id),
        }
    }

    fn test_daemon(label: &str) -> (PathBuf, SellerDaemon) {
        let root = temp(label);
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home.config.mint_url = DEFAULT_MINT_URL.into();
        home.config.seller = Some(SellerConfig {
            agent_command: vec!["echo".into()],
            rate_sats: 1,
            git_remote: "https://example.invalid/repo.git".into(),
            job_timeout_secs: None,
            agent: None,
            claim_open_pool: false,
        });
        home::save_config(&home).expect("save");
        let keys = nostr_sdk::Keys::generate();
        let seller_pubkey = keys.public_key().to_hex();
        let journal = SellerJournal::open(&home).expect("journal");
        let daemon = SellerDaemon {
            home,
            keys,
            seller_pubkey,
            journal,
            pay_buffer: VecDeque::new(),
            active: None,
            awaiting_payment: Vec::new(),
        };
        (root, daemon)
    }

    fn offer_event(
        buyer: &nostr_sdk::Keys,
        seller_pubkey: &str,
        amount: u64,
        deadline: u64,
    ) -> nostr_sdk::Event {
        let offer = crate::gateway::OfferDraft::new(
            "do a task",
            "text/plain",
            amount,
            deadline,
            DEFAULT_MINT_URL,
            seller_pubkey,
        );
        let draft = offer.to_event_draft();
        let builder = gateway::nostr::event_builder(&draft).expect("event builder");
        builder.sign_with_keys(buyer).expect("sign offer")
    }

    // Behavior 1 (#15): a delivered-but-unpaid job MUST NOT block claiming a new offer;
    // only a PROCESSING job holds single-flight, and any skip is a NAMED reason.
    #[test]
    fn delivered_unpaid_does_not_block_new_offer_but_processing_does() {
        let (root, mut daemon) = test_daemon("admit");
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;

        // Idle slot ⇒ offer is admitted (Claim).
        let ev1 = offer_event(&buyer, &seller_pk, 5, now + 3600);
        match daemon.classify_offer(&ev1, now).expect("classify idle") {
            OfferDisposition::Claim(intent) => assert_eq!(intent.job_id, ev1.id.to_hex()),
            other => panic!("idle daemon must admit an offer, got {other:?}"),
        }

        // A DELIVERED-but-unpaid job awaiting payment ⇒ STILL admits a new offer (the fix).
        daemon.awaiting_payment.push(active_job(
            "delivered-prev",
            &seller_pk,
            Some("result-prev"),
            now + 3600,
            &root,
        ));
        assert!(daemon.active.is_none(), "delivered job must not hold the slot");
        let ev2 = offer_event(&buyer, &seller_pk, 5, now + 3600);
        match daemon.classify_offer(&ev2, now).expect("classify while awaiting-pay") {
            OfferDisposition::Claim(_) => {}
            other => panic!("delivered-but-unpaid must NOT block a new claim, got {other:?}"),
        }

        // A PROCESSING job (holds the slot) ⇒ skip, but with an explicit, non-empty reason.
        daemon.active = Some(active_job("processing-now", &seller_pk, None, now + 3600, &root));
        let ev3 = offer_event(&buyer, &seller_pk, 5, now + 3600);
        match daemon.classify_offer(&ev3, now).expect("classify while processing") {
            OfferDisposition::Skip(skip) => {
                assert!(matches!(skip, OfferSkip::ProcessingBusy { .. }), "got {skip:?}");
                let reason = skip.reason();
                assert!(!reason.is_empty(), "skip reason must never be empty (never silent)");
                assert!(
                    reason.contains("processing"),
                    "reason must name the processing single-flight: {reason}"
                );
            }
            other => panic!("processing job must block with a reason, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // Behavior 2: restart-reconcile over a REAL orphaned-claim fixture (journaled in-flight
    // claim + past deadline). The orphan is released (durable, no relay) and never re-fired.
    #[test]
    fn reconcile_journal_releases_real_orphaned_claim_and_is_idempotent() {
        let (root, mut daemon) = test_daemon("reconcile");
        let buyer = "cc".repeat(32);

        // A real journaled in-flight claim with a PAST deadline, no receipt, no release —
        // exactly the orphaned live claim a crashed daemon leaves behind (job 0867a213).
        daemon
            .journal
            .append_claim("orphan-job", "orphan-claim", &buyer, 1_000_000_000)
            .expect("journal orphaned claim");

        // Before reconcile, it reads as an in-flight orphan (would show "processing").
        let pre = daemon.reconcile_plan(2_000_000_000).expect("plan");
        assert_eq!(pre.len(), 1, "one orphaned claim in flight: {pre:?}");
        assert_eq!(pre[0].job_id, "orphan-job");
        assert_eq!(pre[0].buyer_pubkey, buyer);
        assert_eq!(pre[0].liveness, ClaimLiveness::Expired, "past deadline ⇒ EXPIRED");

        // Reconcile releases it durably (journal RELEASE) with no relay.
        let released = daemon.reconcile_journal(2_000_000_000).expect("reconcile");
        assert_eq!(released.len(), 1);
        assert!(
            daemon.journal.has_release("orphan-job").expect("has_release"),
            "orphan must be journaled as released — never left silently live"
        );

        // Idempotent: a second restart finds nothing to release.
        let again = daemon.reconcile_journal(2_000_000_000).expect("reconcile again");
        assert!(again.is_empty(), "released orphan is terminal: {again:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(not(feature = "acp"))]
    #[tokio::test]
    async fn agent_run_fail_closed_without_acp_feature() {
        let identity = seller_git::DeliveryAgentIdentity::for_seller(&"aa".repeat(32));
        let err = run_agent_job(
            &["echo".into()],
            "task",
            Path::new("."),
            &identity,
            Duration::from_secs(1),
        )
        .await
        .expect_err("acp required");
        assert!(matches!(err, DaemonError::AcpRequired));
        assert!(err.to_string().contains("acp"));
    }
}
