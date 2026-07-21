//! Seller daemon state machine (freeze checklist).
//!
//! Loop:
//! 1. Subscribe **offers + gift-wraps from START** (early pay buffered).
//! 2. On targeted offer passing B1 rate-gate → claim feedback → journal claim (single-flight).
//! 3. Run agent (`--features acp` fail-closed) → git push (allowlist+scrub) → result.
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
use crate::contribution::ContributionOffer;
use crate::driver::UsageMetadata;
use crate::episode::{Episode, EpisodeKind, EpisodeLog, EpisodeOutcome, UsageRecord};
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

/// In-flight single-flight lock (one job in the PROCESSING phase per process).
/// Held from claim through delivery (result-kind), then released — a delivered-but-unpaid
/// job awaiting payment does NOT hold this lock.
static FLIGHT: AtomicBool = AtomicBool::new(false);

/// Upper bound on delivered-but-unpaid jobs tracked concurrently (bounded memory).
/// Reaching it back-pressures new claims with a logged skip reason (never a silent drop).
const AWAITING_PAYMENT_CAP: usize = 16;

/// Item #16(c): bound on agent attempts per job. A transient agent error is retried up to
/// this many times, but only while the job deadline still has room — the retry loop never
/// outlives the deadline (see [`run_agent_with_retry`]).
const MAX_AGENT_ATTEMPTS: u32 = 3;

/// Relay-read timeout for the backfill pre-claim money-safety check ([`SellerDaemon::
/// backfill_offer_blocked`]). Matches the job-lifecycle fetch budget: fetches terminate on the
/// relay's EOSE, so this is an upper bound, not a fixed wait. On a slow/unreachable relay the
/// check fails CLOSED (skip), so a small budget is safe.
const BACKFILL_CHECK_TIMEOUT_SECS: u64 = 5;

/// Cadence (seconds) of the periodic seller wrap backfill (#57). A running daemon re-runs the
/// SAME stored-wrap backfill the boot path uses every this-many seconds, so an AGED relay
/// subscription that has silently stopped delivering kind-1059 payment gift-wraps still recovers
/// WITHOUT a restart. Field-observed: fresh subscriptions deliver a wrap within ~1 min, but a
/// subscription ~10+ min old was seen to go deaf and never deliver again — a payment then sat
/// unredeemed until the daemon was manually restarted (which re-ran the boot backfill). This is a
/// FIXED constant, NOT a user config knob (charter: no new config); the env override below exists
/// only as a test seam (mirrors the heartbeat cadence).
const WRAP_BACKFILL_INTERVAL_SECS: u64 = 300;

/// Test-only env seam overriding [`WRAP_BACKFILL_INTERVAL_SECS`] (NOT a user config knob; mirrors
/// `MOBEE_HEARTBEAT_INTERVAL_SECS`). A `0` or unparseable value is ignored. No production path
/// sets it — the periodic cadence is the fixed constant in production.
const WRAP_BACKFILL_INTERVAL_ENV: &str = "MOBEE_WRAP_BACKFILL_INTERVAL_SECS";

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
    /// The parsed contribution offer (target pin + base + accepts) when this is a
    /// contribution job; `None` ⇒ from-scratch (empty-base delivery).
    pub contribution: Option<ContributionOffer>,
    /// Delivery facts captured at successful result-kind publish, carried through
    /// `mark_delivered` so the terminal episode (paid at receipt, or unpaid at eviction) is a
    /// single complete append. `None` until the job delivers. Diagnostic only — never money state.
    pub delivery: Option<DeliveryRecord>,
}

/// The delivery-time facts an [`Episode`] needs, captured once at result-kind publish.
/// Stashed on the [`ActiveJob`] so the (possibly later) paid/unpaid terminal writes one complete
/// episode without re-deriving anything on the money path.
#[derive(Debug, Clone)]
pub struct DeliveryRecord {
    pub result_id: String,
    pub commit_oid: String,
    pub git_remote: String,
    pub branch: String,
    pub delivery_kind: String,
    pub harness: String,
    pub wall_time_ms: u64,
    pub usage: Option<UsageMetadata>,
    pub transcript_ref: String,
    pub deliver_ts: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorReasonCode {
    AgentSpawnFailed,
    AgentRunFailed,
    AgentTimeout,
    GitForkFailed,
    GitPushFailed,
    ContributionUnsupported,
    ContributionMalformed,
    ClaimReleased,
    Internal,
}

impl ErrorReasonCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::AgentSpawnFailed => "agent_spawn_failed",
            Self::AgentRunFailed => "agent_run_failed",
            Self::AgentTimeout => "agent_timeout",
            Self::GitForkFailed => "git_fork_failed",
            Self::GitPushFailed => "git_push_failed",
            Self::ContributionUnsupported => "contribution_unsupported",
            Self::ContributionMalformed => "contribution_malformed",
            Self::ClaimReleased => "claim_released",
            Self::Internal => "internal",
        }
    }
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
    /// Parsed contribution offer, threaded into the active job.
    pub contribution: Option<ContributionOffer>,
}

/// Enumerated reasons an offer is not claimed. Every variant maps to a logged reason —
/// there is no silent-drop path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferSkip {
    /// Event is not a offer-kind offer.
    NotAnOffer { kind: u16 },
    /// Offer tags did not parse.
    Unparseable,
    /// Offer's own deadline (`param deadline`, absolute unix) has already passed at `now`.
    /// Money-safety: a lapsed offer is REFUSED so a backfilled (stored) offer can never be
    /// resurrected with a fresh `now + timeout` deadline (the pre-backfill hazard in
    /// `job_deadline_unix`). A pure function of the offer event + `now`, so it holds for a
    /// live offer too — an offer whose deadline already passed is dead regardless of delivery.
    DeadlineExpired { deadline_unix: u64, now: u64 },
    /// Rate-gate refused (not targeted to us / below rate / untargeted without opt-in).
    RateGate { reason: String },
    /// Journal already has a claim/receipt/release for this job (dedup).
    AlreadyProcessed,
    /// A job is already in the PROCESSING phase (single-flight). NOT triggered by
    /// delivered-but-unpaid jobs — those await payment without holding the slot.
    ProcessingBusy { job_id: String },
    /// Too many delivered-but-unpaid jobs pending payment (bounded-memory back-pressure).
    AwaitingPaymentFull { capacity: usize },
    /// A `job-class=contribution` offer arrived but this seller has contribution support
    /// disabled (`contribution_enabled=false`). Emits a feedback-kind error (interop courtesy) instead
    /// of running it as from-scratch.
    ContributionUnsupported,
    /// A `job-class=contribution` offer whose target-repo/base pins are malformed —
    /// refused (fail-closed; never run as from-scratch). Emits a feedback-kind error.
    ContributionMalformed { reason: String },
}

impl OfferSkip {
    /// Machine-mappable variant name (`refusal_reason_code`). Stable enumerated set.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAnOffer { .. } => "NotAnOffer",
            Self::Unparseable => "Unparseable",
            Self::DeadlineExpired { .. } => "DeadlineExpired",
            Self::RateGate { .. } => "RateGate",
            Self::AlreadyProcessed => "AlreadyProcessed",
            Self::ProcessingBusy { .. } => "ProcessingBusy",
            Self::AwaitingPaymentFull { .. } => "AwaitingPaymentFull",
            Self::ContributionUnsupported => "ContributionUnsupported",
            Self::ContributionMalformed { .. } => "ContributionMalformed",
        }
    }

    /// Human-readable skip reason for logging (never empty).
    pub fn reason(&self) -> String {
        match self {
            Self::NotAnOffer { kind } => format!("not a kind-{JOB_OFFER_KIND} offer (kind {kind})"),
            Self::Unparseable => "offer tags did not parse".to_string(),
            Self::DeadlineExpired { deadline_unix, now } => format!(
                "offer deadline {deadline_unix} already passed at now={now} (expired; refused — a lapsed offer is never claimed or resurrected)"
            ),
            Self::RateGate { reason } => format!("rate-gate: {reason}"),
            Self::AlreadyProcessed => "already claimed/receipted/released (journal dedup)".to_string(),
            Self::ProcessingBusy { job_id } => {
                format!("single-flight busy: job {job_id} is in the processing phase")
            }
            Self::AwaitingPaymentFull { capacity } => {
                format!("awaiting-payment backlog full (cap {capacity}); back-pressuring new claims")
            }
            Self::ContributionUnsupported => {
                "contribution offer refused: seller contribution support is disabled (feedback-kind interop courtesy)".to_string()
            }
            Self::ContributionMalformed { reason } => {
                format!("contribution offer refused (malformed pins, fail-closed): {reason}")
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
    /// DELIVERED-but-unpaid jobs (result-kind published, payment not yet redeemed). These do
    /// NOT hold single-flight, so new offers can still be claimed while payment is pending.
    awaiting_payment: Vec<ActiveJob>,
}

impl SellerDaemon {
    pub fn open(home: MobeeHome) -> Result<Self, DaemonError> {
        require_seller_config(&home)?;
        if home.config.accepted_mints.is_empty() {
            return Err(DaemonError::Config(
                "seller accepted_mints must be non-empty".to_owned(),
            ));
        }
        // Real-mint fence (issue #49): with `allow_real_mints=false` (default), only the
        // testnut/dev allow-list is admissible; set `allow_real_mints=true` to admit any
        // well-formed https mint (the deliberate real-money switch).
        for mint in &home.config.accepted_mints {
            if !crate::home::mint_allowed(mint, home.config.allow_real_mints) {
                return Err(DaemonError::Config(format!(
                    "seller mint fail-closed: accepted_mints entry {mint} is not an allow-listed \
                     testnut/dev mint ({DEFAULT_MINT_URL}); set allow_real_mints=true to admit a \
                     real https mint"
                )));
            }
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

    /// Build the seller heartbeat for the current daemon state, or `None` when there is no
    /// `[seller]` config (no advertised rate to publish). `accepting` is `n` while the
    /// single-flight slot is held (the seller is busy on a job); `queue_depth` is that in-flight
    /// count. Reads state only — never publishes.
    fn heartbeat_draft(&self) -> Option<crate::heartbeat::HeartbeatDraft> {
        let cfg = require_seller_config(&self.home).ok()?;
        Some(crate::heartbeat::heartbeat_for_state(
            self.active.is_some(),
            cfg.rate_sats,
        ))
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

    /// Handle one offer-kind offer event. Returns Ok(Some(active)) when claimed.
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
                // Durable refusal capture (previously eprintln-only). Best-effort —
                // an episode write never affects the skip decision or the money path.
                self.record_refused_episode(event, &skip);
                // Announce the refusal (with its machine-readable reason code). Skip the same two
                // non-freshly-classified cases the episode capture skips — a non-offer and a dedup
                // re-see are noise, not a seller decision. `amount` rides along when the offer
                // parsed (unparseable ⇒ None).
                if !matches!(skip, OfferSkip::NotAnOffer { .. } | OfferSkip::AlreadyProcessed) {
                    let amount = parse_offer(&event_to_draft(event)).ok().map(|o| o.amount);
                    self.announce(crate::announce::AnnounceEvent::refused(
                        now_unix(),
                        &self.seller_pubkey,
                        &event.id.to_hex(),
                        skip.code(),
                        &skip.reason(),
                        amount,
                    ));
                }
                // Bundled buyer-visibility: a TARGETED-to-self under-rate refusal also emits a
                // feedback-kind `status=error` so the buyer learns why. Open-pool under-rate stays
                // log-only (spam guard). No-op for every other skip reason. Content carries the
                // same machine-readable rate-gate reason already logged here (buyers distinguish
                // rate-refusal from a crash).
                if let OfferSkip::RateGate { reason } = &skip {
                    self.publish_under_rate_error_if_targeted(event, reason).await;
                }
                // Interop courtesy: a seller that cannot/​will not handle a contribution
                // offer emits a feedback-kind `status=error` so the buyer does not wait on a delivery
                // that will never come. NOT a security control — buyer refusal is the boundary.
                if matches!(
                    skip,
                    OfferSkip::ContributionUnsupported | OfferSkip::ContributionMalformed { .. }
                ) {
                    let content = contribution_refusal_error_content(&skip);
                    let draft = error_draft(
                        &event.id.to_hex(),
                        &event.pubkey.to_hex(),
                        &self.seller_pubkey,
                        content,
                    );
                    let _ = publish_draft(&self.home, &self.keys, &draft).await;
                }
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

        // The seller authors the claim's payment terms as a NUT-18 payment
        // request (`creq…`) — accepted mints from its OWN config (not the offer), amount/unit
        // copied from the offer, single-use, addressed to the seller's key. The claim is the
        // invoice; the buyer satisfies this `creq`.
        let creq = match gateway::creq::build_seller_creq(
            &intent.job_id,
            intent.offer.amount,
            &intent.offer.unit,
            &self.home.config.accepted_mints,
            &self.seller_pubkey,
        ) {
            Ok(creq) => creq,
            Err(error) => {
                Self::end_flight();
                return Err(DaemonError::Seller(SellerError::Io(error.to_string())));
            }
        };
        let claim = claim_draft(
            &intent.job_id,
            &intent.buyer_pubkey,
            &self.seller_pubkey,
            &creq,
        );
        let claim_id = match publish_draft(&self.home, &self.keys, &claim).await {
            Ok(id) => id,
            Err(error) => {
                Self::end_flight();
                return Err(error);
            }
        };
        // Journal the CLAIMED transition WITH the deadline/claim_id/buyer so a restart can
        // reconcile this claim without the relay (restart-reconcile).
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

        // CLAIMED visibility: before this feature the daemon claimed an offer SILENTLY (the claim
        // was journaled but emitted no log line and no observable event — the sidecar's earliest
        // positive signal was `delivered`). Emit the first-ever claim signal on BOTH surfaces: a
        // stderr line (for the log-tailing sidecar) and the structured announce event.
        eprintln!(
            "seller claimed offer job_id={} claim_id={claim_id} buyer={} amount={} deadline={}",
            intent.job_id, intent.buyer_pubkey, intent.offer.amount, intent.deadline_unix
        );
        self.announce(crate::announce::AnnounceEvent::claimed(
            now_unix(),
            &self.seller_pubkey,
            &intent.job_id,
            &intent.buyer_pubkey,
            intent.offer.amount,
            intent.deadline_unix,
        ));

        self.active = Some(ActiveJob {
            job_id: intent.job_id,
            buyer_pubkey: intent.buyer_pubkey,
            offer: intent.offer,
            claim_id,
            result_id: None,
            deadline_unix: intent.deadline_unix,
            workdir,
            contribution: intent.contribution,
            delivery: None,
        });
        Ok(self.active.as_ref())
    }

    /// Decide, WITHOUT any relay I/O, whether an offer event should be claimed.
    ///
    /// `now` is injected so the deadline is a pure function of inputs. Single-flight is
    /// enforced ONLY for the PROCESSING slot (`self.active`): a delivered-but-unpaid job
    /// in `awaiting_payment` does not block (silent-drop fix).
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
        // The offer no longer names a mint that the seller must match — the
        // seller's `accepted_mints` are asserted later against the *paid* token (redeem guard),
        // so there is nothing to gate here. (`offer.mint_url` stays present and readable until
        // Jobs D/E re-point the remaining reads.)
        // Offer-freshness gate (money-safety, backfill guard #a) — a deliberate ALWAYS-ON
        // refusal on EVERY offer path (live, open-pool backfill, AND the targeted filter's
        // full-history backfill; classify-level, so no filter shape can bypass it). The offer
        // carries its own absolute deadline (`param deadline`, always present — `parse_offer`
        // requires it). If it has already passed we REFUSE here, BEFORE `job_deadline_unix` —
        // which would otherwise hand a stale offer a fresh `now + timeout` deadline and
        // resurrect a lapsed job (the pre-existing hazard: stored targeted offers were
        // re-deliverable on every restart and got fresh deadlines). `offer.deadline_unix > now`
        // is exactly the still-usable branch of `job_deadline_unix`, so this gate is its safe
        // complement. Pure over (offer, now).
        if offer.deadline_unix <= now {
            return Ok(OfferDisposition::Skip(OfferSkip::DeadlineExpired {
                deadline_unix: offer.deadline_unix,
                now,
            }));
        }
        let seller_cfg = require_seller_config(&self.home)?;
        // Parse the contribution class. A malformed contribution offer is REFUSED
        // (fail-closed, never run from-scratch); a contribution offer to a seller with support
        // disabled is refused as an interop courtesy. Both emit a feedback-kind in `on_offer_event`.
        let contribution = match crate::contribution::parse_contribution_offer(&draft.tags) {
            Ok(value) => value,
            Err(error) => {
                return Ok(OfferDisposition::Skip(OfferSkip::ContributionMalformed {
                    reason: error.to_string(),
                }));
            }
        };
        if contribution.is_some() && !seller_cfg.contribution_enabled {
            return Ok(OfferDisposition::Skip(OfferSkip::ContributionUnsupported));
        }
        if let Err(error) = rate_gate_allows(
            &offer,
            &self.seller_pubkey,
            seller_cfg.rate_sats,
            seller_cfg.claim_open_pool,
        ) {
            // Prefer the raw policy message (e.g. "offer amount 3 sat below seller rate_sats 5")
            // over Display's "seller policy refused: …" prefix — that string is logged and
            // plumbed into the feedback-kind content for targeted under-rate refusals.
            let reason = match error {
                SellerError::Policy(message) => message,
                other => other.to_string(),
            };
            return Ok(OfferDisposition::Skip(OfferSkip::RateGate { reason }));
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
            contribution,
        }))
    }

    /// Backfill pre-claim money-safety check (guards #c/#d) for a BACKFILLED offer — one posted
    /// before the daemon came online. Reads the job's current relay state (offer + feedback-kind
    /// claims + result-kind results, with claim liveness derived against `now`) and returns
    /// `Some(reason)` if the offer must be SKIPPED, `None` to proceed to the normal claim path.
    ///
    /// This is the guard that stops a stored offer from stomping a trade that already moved on
    /// while the daemon was offline:
    ///  * **#c** any result-kind result exists (from ANY seller) ⇒ already delivered/settled;
    ///  * **#d** a LIVE claim (feedback-kind `processing`, not past the offer deadline) is held by
    ///    ANOTHER seller ⇒ an in-flight trade — do not stomp it.
    ///
    /// FAIL-CLOSED: if the relay read errors (cannot determine current state) the offer is
    /// SKIPPED with a logged reason rather than optimistically claimed. Never runs on the
    /// live path (see the caller's `created_at < daemon_start_unix` gate), so it adds no relay
    /// round-trip to offers posted while the daemon runs.
    async fn backfill_offer_blocked(&self, event: &nostr_sdk::Event) -> Option<String> {
        let job_id = event.id.to_hex();
        let now = now_unix();
        let view = match crate::job_lifecycle::fetch_job_view_async(
            &self.home,
            &self.keys,
            &job_id,
            Duration::from_secs(BACKFILL_CHECK_TIMEOUT_SECS),
            now,
        )
        .await
        {
            Ok(view) => view,
            Err(error) => {
                return Some(format!(
                    "backfill relay pre-claim check failed (fail-closed skip): {error}"
                ));
            }
        };
        // #c — already delivered/settled by ANY seller (a result-kind result exists).
        if let Some(result) = view.results.first() {
            return Some(format!(
                "already delivered: {} result-kind result(s) on relay (newest {} by {}); not re-claiming a settled job",
                view.results.len(),
                result.result_id,
                result.seller_pubkey
            ));
        }
        // #d — a LIVE claim held by ANOTHER seller (don't stomp an in-flight trade). Expired
        // claims are already excluded from `live_claim_id` (liveness derived vs the offer
        // deadline); our OWN prior claim is caught by the durable journal in `classify_offer`.
        if let Some(live_id) = &view.live_claim_id {
            if let Some(claim) = view.claims.iter().find(|c| &c.claim_id == live_id) {
                if claim.seller_pubkey != self.seller_pubkey {
                    return Some(format!(
                        "live claim {live_id} held by another seller {} (offer in-flight); not stomping",
                        claim.seller_pubkey
                    ));
                }
            }
        }
        None
    }

    /// Bundled buyer-visibility (step 5): when a **targeted-to-self** offer is refused only for
    /// being below the seller's rate floor, publish a feedback-kind `status=error` so the buyer
    /// learns WHY (in addition to the local skip log). OPEN-POOL / untargeted under-rate refusals
    /// stay LOG-ONLY (a fleet of rate-N sellers each feedback-ing every cheap open offer would spam
    /// the relay). `reason` is the machine-readable rate-gate string (same as the skip log) and
    /// is written into the event content. Publish failure is logged, never fatal. Called only on
    /// a `RateGate` skip.
    async fn publish_under_rate_error_if_targeted(&self, event: &nostr_sdk::Event, reason: &str) {
        let draft = event_to_draft(event);
        let Ok(offer) = parse_offer(&draft) else {
            return;
        };
        let Ok(seller_cfg) = require_seller_config(&self.home) else {
            return;
        };
        let targeted_to_self = offer.seller_pubkey.as_deref() == Some(self.seller_pubkey.as_str());
        if !(targeted_to_self && offer.amount < seller_cfg.rate_sats) {
            return; // open-pool / wrong-target / not-under-rate ⇒ log-only (handled by caller).
        }
        let error = under_rate_error_draft(
            &event.id.to_hex(),
            &event.pubkey.to_hex(),
            &self.seller_pubkey,
            reason,
        );
        match publish_draft(&self.home, &self.keys, &error).await {
            Ok(id) => eprintln!(
                "seller under-rate refusal surfaced: feedback-kind error={id} offer={} (amount {} < rate_sats {}) reason={reason}",
                event.id.to_hex(),
                offer.amount,
                seller_cfg.rate_sats
            ),
            Err(error) => eprintln!(
                "seller WARN: under-rate refusal feedback-kind publish failed offer={}: {error}",
                event.id.to_hex()
            ),
        }
    }

    /// DELIVERED transition: move the PROCESSING job to `awaiting_payment` and free the
    /// single-flight slot so new offers can be claimed while payment is pending (#15).
    /// The payment binding is preserved (job_id + result_id) for [`try_apply_payment`].
    fn mark_delivered(&mut self) {
        if let Some(job) = self.active.take() {
            // `result_id` was set by `execute_active_job` on the successful publish path.
            // #57: durably journal the delivered-but-unpaid transition so a restart rebuilds this
            // job into awaiting_payment and a stored/buffered wrap can bind + redeem. Best-effort
            // but LOUD — a failed write risks re-stranding the payment across a restart.
            if let Some(result_id) = job.result_id.as_deref() {
                if let Err(error) = self.journal.append_delivery(
                    &job.job_id,
                    result_id,
                    job.offer.amount,
                    &job.offer.unit,
                    &job.buyer_pubkey,
                ) {
                    eprintln!(
                        "seller WARN: failed to journal delivery for job_id={} (payment recovery on restart may be degraded): {error}",
                        job.job_id
                    );
                }
            }
            self.awaiting_payment.push(job);
            while self.awaiting_payment.len() > AWAITING_PAYMENT_CAP {
                let dropped = self.awaiting_payment.remove(0);
                eprintln!(
                    "seller drop awaiting-payment job_id={} (backlog cap {AWAITING_PAYMENT_CAP})",
                    dropped.job_id
                );
                // Delivered-but-UNPAID terminal (backpressure eviction). Best-effort.
                self.record_delivered_unpaid_episode(&dropped);
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

    /// #57: rebuild delivered-but-unpaid jobs (journaled `Delivery`, no `Receipt`/`Release`) into
    /// `awaiting_payment` at boot, so a backfilled or buffered payment wrap can bind and redeem.
    /// Without this, a job the offer-scan classifies "already delivered" is never re-added and its
    /// wrap buffers forever. Idempotent: skips jobs already present; respects the backlog cap. The
    /// rebuilt offer carries only the money-critical fields the redeem path reads (amount/unit);
    /// `seller_pubkey=None` so `assert_seller_matches` passes.
    pub fn restore_delivered_unpaid(&mut self) -> Result<usize, DaemonError> {
        let unpaid = self.journal.deliveries_awaiting_receipt()?;
        let mut restored = 0usize;
        for d in unpaid {
            if self
                .awaiting_payment
                .iter()
                .any(|job| job.job_id == d.job_id)
            {
                continue;
            }
            if self.awaiting_payment.len() >= AWAITING_PAYMENT_CAP {
                break;
            }
            let unit = if d.unit.is_empty() {
                "sat".to_string()
            } else {
                d.unit
            };
            self.awaiting_payment.push(ActiveJob {
                job_id: d.job_id.clone(),
                buyer_pubkey: d.buyer_pubkey,
                offer: ParsedOffer {
                    task: String::new(),
                    output: String::new(),
                    amount: d.amount,
                    unit,
                    deadline_unix: 0,
                    seller_pubkey: None,
                },
                claim_id: String::new(),
                result_id: Some(d.result_id),
                deadline_unix: 0,
                workdir: job_workdir(&self.home, &d.job_id),
                contribution: None,
                delivery: None,
            });
            restored += 1;
        }
        Ok(restored)
    }

    /// Reconstruct a delivered-job bind for a buffered/backfilled payment wrap from ON-RELAY
    /// ground truth, for the recovery case [`restore_delivered_unpaid`] cannot cover: a seat that
    /// delivered under an older build (or whose journal is lost/incomplete) has NO local `Delivery`
    /// entry, so its wrap references a job that never enters `awaiting_payment` and buffers forever.
    ///
    /// Pure over the fetched [`JobView`] + the received payment (the async caller does the relay
    /// I/O and the `awaiting_payment` push), so every fail-closed branch is unit-testable without a
    /// relay. Returns `Ok(ActiveJob)` only when EVERY money-critical fact verifies against events
    /// this seller itself authored; otherwise `Err(reason)` and the caller leaves the wrap buffered.
    ///
    /// Guards (all fail-closed — a partial reconstruction never redeems):
    ///  * the result (kind [`JOB_RESULT_KIND`]) for `J` MUST be authored by self (a foreign author
    ///    is refused — money only ever binds to this seller's own delivery);
    ///  * the claim (kind 3402) for `J` MUST be authored by self AND carry a parseable `creq`;
    ///  * that `creq` MUST bind the payload: same job id, matching unit, the payload's realized
    ///    mint listed in the `creq`, and the `creq` amount equal to the on-relay offer amount;
    ///  * the payload's realized mint MUST be in the seller's `accepted_mints` (the SAME allow-list
    ///    the redeem guard enforces — checked here so a foreign mint leaves the wrap buffered
    ///    rather than erroring the ingest);
    ///  * the on-relay offer MUST be present (its amount/unit are the redeem terms).
    ///
    /// The rebuilt job carries `seller_pubkey=None` (so `assert_seller_matches` passes) and the
    /// money-critical amount/unit from the offer; the EXISTING redeem path re-checks every guard
    /// (`assert_redeem_mint`, terms, amount) before any funds move.
    fn reconstruct_delivered_bind(
        &self,
        view: &crate::job_lifecycle::JobView,
        received: &ReceivedPayment,
    ) -> Result<ActiveJob, String> {
        let job_id = received.payload.job_id();
        // Own result (kind 3403, author == self). A result authored by ANOTHER seller is a foreign
        // delivery — never bind money to it.
        let own_result = view
            .results
            .iter()
            .find(|result| result.seller_pubkey == self.seller_pubkey)
            .ok_or_else(|| {
                format!(
                    "no self-authored result on relay for job_id={job_id} (results={}); \
                     refusing reconstruction",
                    view.results.len()
                )
            })?;
        // Own claim (kind 3402, author == self) carrying the seller-authored NUT-18 `creq`.
        let own_claim = view
            .claims
            .iter()
            .find(|claim| claim.seller_pubkey == self.seller_pubkey)
            .ok_or_else(|| {
                format!("no self-authored claim on relay for job_id={job_id}; refusing reconstruction")
            })?;
        let creq = own_claim.creq.as_deref().ok_or_else(|| {
            format!(
                "self claim for job_id={job_id} carries no creq (cannot verify payment terms); \
                 refusing reconstruction"
            )
        })?;
        // Bind the reconstruction to the seller-authored request FIELD BY FIELD: the checks below
        // verify the fetched claim's creq against the payload (payment id == job id, realized mint
        // listed, unit match) and against the on-relay offer (amount). The seller side stores no
        // creq hash to compare a recomputed one against (the accept-bind's `creq_hash` is a buyer
        // artifact), so the parsed fields ARE the binding.
        let request = gateway::creq::parse_creq(creq).map_err(|error| {
            format!("self claim creq for job_id={job_id} unparseable: {error}; refusing reconstruction")
        })?;
        if request.payment_id.as_deref() != Some(job_id) {
            return Err(format!(
                "creq mismatch: creq payment id {:?} != job_id={job_id}; refusing reconstruction",
                request.payment_id
            ));
        }
        let payload_mint = &received.payload.payload.mint;
        if !request.mints.contains(payload_mint) {
            return Err(format!(
                "creq mismatch: payload mint {payload_mint} not authored in self claim creq for \
                 job_id={job_id}; refusing reconstruction"
            ));
        }
        if request.unit.as_ref() != Some(&received.payload.payload.unit) {
            return Err(format!(
                "creq mismatch: payload unit {:?} != creq unit {:?} for job_id={job_id}; \
                 refusing reconstruction",
                received.payload.payload.unit,
                request.unit
            ));
        }
        // On-relay offer supplies the redeem terms (amount/unit) the seller-authored creq copied.
        let offer = view.offer.as_ref().ok_or_else(|| {
            format!("no offer on relay for job_id={job_id}; refusing reconstruction")
        })?;
        if request.amount != Some(cashu::Amount::from(offer.amount_sats)) {
            return Err(format!(
                "creq mismatch: creq amount {:?} != offer amount {} for job_id={job_id}; \
                 refusing reconstruction",
                request.amount, offer.amount_sats
            ));
        }
        // The payload's realized mint MUST be one this seller advertised — the SAME allow-list the
        // redeem guard (`assert_redeem_mint`) enforces, applied here so an unlisted mint leaves the
        // wrap buffered instead of erroring the ingest. Never relaxes the allow-list.
        let accepted = self
            .home
            .config
            .accepted_mints
            .iter()
            .map(|entry| mint_url(entry))
            .collect::<Result<std::collections::HashSet<_>, _>>()
            .map_err(|error| format!("accepted_mints parse for job_id={job_id}: {error}"))?;
        if !accepted.contains(payload_mint) {
            return Err(format!(
                "payload mint {payload_mint} not in seller accepted_mints for job_id={job_id}; \
                 refusing reconstruction"
            ));
        }
        Ok(ActiveJob {
            job_id: job_id.to_owned(),
            buyer_pubkey: offer.author_pubkey.clone(),
            offer: ParsedOffer {
                task: String::new(),
                output: String::new(),
                amount: offer.amount_sats,
                unit: received.payload.payload.unit.to_string(),
                deadline_unix: 0,
                seller_pubkey: None,
            },
            claim_id: own_claim.claim_id.clone(),
            result_id: Some(own_result.result_id.clone()),
            deadline_unix: 0,
            workdir: job_workdir(&self.home, job_id),
            contribution: None,
            delivery: None,
        })
    }

    /// Fetch on-relay ground truth for a buffered wrap whose delivered-job bind is missing from the
    /// local journal, reconstruct the bind fail-closed ([`reconstruct_delivered_bind`]), and
    /// restore it into `awaiting_payment` so the EXISTING redeem path runs. Returns `true` iff a
    /// bind was restored. Any relay-fetch or verify failure logs a clear reason and returns `false`
    /// (the caller leaves the wrap buffered). Never redeems on a partial reconstruction.
    async fn reconstruct_bind_from_relay(&mut self, received: &ReceivedPayment) -> bool {
        let job_id = received.payload.job_id().to_owned();
        if self.awaiting_payment.len() >= AWAITING_PAYMENT_CAP {
            eprintln!(
                "seller wrap reconstruct job_id={job_id}: awaiting_payment full (cap \
                 {AWAITING_PAYMENT_CAP}); leaving buffered"
            );
            return false;
        }
        let view = match crate::job_lifecycle::fetch_job_view_async(
            &self.home,
            &self.keys,
            &job_id,
            Duration::from_secs(BACKFILL_CHECK_TIMEOUT_SECS),
            now_unix(),
        )
        .await
        {
            Ok(view) => view,
            Err(error) => {
                eprintln!(
                    "seller wrap reconstruct job_id={job_id}: relay fetch failed (fail-closed, \
                     leaving buffered): {error}"
                );
                return false;
            }
        };
        match self.reconstruct_delivered_bind(&view, received) {
            Ok(job) => {
                eprintln!(
                    "seller wrap reconstruct job_id={job_id}: bind reconstructed from on-relay \
                     result+claim (result_id={} claim_id={}); restoring into awaiting_payment",
                    job.result_id.as_deref().unwrap_or_default(),
                    job.claim_id
                );
                self.awaiting_payment.push(job);
                true
            }
            Err(reason) => {
                eprintln!("seller wrap reconstruct job_id={job_id}: {reason}");
                false
            }
        }
    }

    /// Full startup reconcile: durable journal release (above) + best-effort feedback-kind
    /// error to surface the dead claim to the buyer. Publish failure is logged, not fatal —
    /// the journal release is the durable guarantee; the buyer view also derives expiry.
    pub async fn reconcile_on_startup(
        &mut self,
        now: u64,
    ) -> Result<Vec<OrphanClaim>, DaemonError> {
        let plan = self.reconcile_journal(now)?;
        for orphan in &plan {
            let reason = reconcile_reason(orphan.liveness);
            // RECONCILE-RELEASED announce: an orphaned in-flight claim released on startup.
            self.announce(crate::announce::AnnounceEvent::reconcile_released(
                now_unix(),
                &self.seller_pubkey,
                &orphan.job_id,
                &format!("{:?}", orphan.liveness),
                reason,
            ));
            let content = reconcile_error_content(orphan.liveness);
            let draft = error_draft(
                &orphan.job_id,
                &orphan.buyer_pubkey,
                &self.seller_pubkey,
                content,
            );
            match publish_draft(&self.home, &self.keys, &draft).await {
                Ok(id) => eprintln!(
                    "seller reconcile: released orphaned claim job_id={} liveness={:?} kind7000={id} reason={reason}",
                    orphan.job_id, orphan.liveness
                ),
                Err(error) => eprintln!(
                    "seller reconcile: released orphaned claim job_id={} liveness={:?} (feedback-kind publish deferred: {error}) reason={reason}",
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

    /// After publishing result: reconcile buffered wraps so early pay still lands (B2).
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
        // Bind to the delivered-but-unpaid job this payment declares (by job id — the NUT-18
        // payload's `id`). `result_id` is resolved LOCALLY from the bound job: NUT-18 carries no
        // result id (only `id` == the job id), and a job id is unique in `awaiting_payment`.
        // Never scans `active` — the processing slot is not a payment target.
        let payload_job = received.payload.job_id().to_owned();
        let Some(idx) = self
            .awaiting_payment
            .iter()
            .position(|job| job.job_id.as_str() == payload_job)
        else {
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
        let offer = job.offer.clone();
        // The redeem terms + all money-path records (redeem log / journal receipt /
        // announce) key off the REALIZED mint the buyer actually paid at (the NUT-18 payload's
        // `mint`), not the seller default. The redeem guard below refuses unless that mint is one
        // the seller advertised in its creq.
        let payload_mint = received.payload.payload.mint.clone();
        let mint = payload_mint.to_string();
        let token = received.payload.to_token();

        // B2: bind BEFORE journal — wrong-job refuse (no misattribution). Matched by
        // construction above; kept as a defensive guard.
        if payload_job != local_job {
            return Err(DaemonError::Policy(format!(
                "payment bind refused: payload job ({payload_job}) != local ({local_job})"
            )));
        }

        let buyer = received.buyer_pubkey.to_hex();
        // Redeem guard: the paid token's mint must be one the seller advertised
        // (`∈ accepted_mints`) AND equal the payload's declared mint. Build the accepted set from
        // the seller's OWN config (the same list authored into the creq `m`), pin the terms to the
        // realized mint, and hand both to the receive guard.
        let accepted_mints = self
            .home
            .config
            .accepted_mints
            .iter()
            .map(|entry| mint_url(entry))
            .collect::<Result<std::collections::HashSet<_>, _>>()?;
        let policy = PaymentPolicy::new(accepted_mints.iter().cloned());
        let terms = policy.terms_for_offer(payload_mint.clone(), &offer, &self.seller_pubkey)?;
        // Amount in terms == offer.amount (NOT rate_sats).
        let secret = home::read_secret_key_hex(&self.home)?;
        let cashu_key = cashu_secret_from_nostr_hex(&secret)?;
        // Must await — seller loop already owns a tokio runtime; blocking open nests block_on → panic.
        let wallet = buyer_fund::open_testnut_wallet_async(&self.home).await?;
        let adapter = CdkSellerReceive::new(&wallet, cashu_key);
        let amount = adapter
            .receive(&token, &terms, &accepted_mints, &payload_mint)
            .await?;
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
            // NUT-18 carries no result id; the bound job's result id is authoritative.
            &local_result,
            amount_received,
            expected_amount,
            &mint,
            &buyer,
            true,
        )?;

        // Capture the delivered-PAID terminal episode. Best-effort — written AFTER the
        // authoritative receipt above and can never fail or alter it. Seeds the retro (§ Retro).
        self.record_paid_episode(&job, amount_received, expected_amount);
        // COLLECTED announce: sats redeemed at the mint + receipt journaled. Emitted AFTER the
        // authoritative receipt; a sink failure can never affect the money that already landed.
        self.announce(crate::announce::AnnounceEvent::collected(
            now_unix(),
            &self.seller_pubkey,
            &local_job,
            &local_result,
            amount_received,
            expected_amount,
            &mint,
        ));

        let outcome = ReceiptOutcome {
            job_id: local_job,
            result_id: local_result,
            amount_received,
        };
        // PAID: drop the delivered binding. Single-flight was already freed at delivery.
        self.awaiting_payment.remove(idx);
        Ok(Some(outcome))
    }

    /// Run agent → push → publish result. On fail/timeout publish feedback error and clear flight.
    pub async fn execute_active_job(&mut self) -> Result<String, DaemonError> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| DaemonError::Policy("no active job".into()))?
            .clone();
        if now_unix() > active.deadline_unix {
            self.fail_active(ErrorReasonCode::AgentTimeout, "job deadline exceeded")
                .await?;
            return Err(DaemonError::Policy("job deadline exceeded".into()));
        }

        let seller_cfg = require_seller_config(&self.home)?.clone();
        // Gate #10 (empty-base): stamp delivery identity into a fresh git workdir (no
        // harness commit). Deliver only if every commit is agent-authored + non-empty tree.
        // Do NOT capture before-OID on empty / require advancement — dogfood is agent-from-empty.
        let identity = seller_git::DeliveryAgentIdentity::for_seller(&self.seller_pubkey);
        // A contribution job forks the PINNED target at base_oid onto a per-job unique
        // branch carrying the FULL job_id; a from-scratch job uses the empty-base workdir.
        let branch = match &active.contribution {
            Some(_) => crate::contribution::ForkRef::unique_branch(&active.job_id),
            None => format!("mobee/{}", &active.job_id[..8.min(active.job_id.len())]),
        };
        let init_result = match &active.contribution {
            Some(contribution) => {
                // Fork base fetch needs NIP-98 auth for relay-git reads — same seller key the
                // push path reads below (:879). Kept local to this arm so the anonymous
                // empty-base path is untouched; fetch itself gates auth to relay-git targets.
                let fork_auth = seller_git::PushAuth {
                    secret_key_hex: home::read_secret_key_hex(&self.home)?,
                };
                seller_git::init_contribution_workdir(
                    &active.workdir,
                    &identity,
                    contribution.target.clone_url(),
                    contribution.base.branch(),
                    contribution.base.oid(),
                    &branch,
                    Some(&fork_auth),
                )
            }
            None => {
                seller_git::init_empty_delivery_workdir(&active.workdir, &identity)
            }
        };
        if let Err(error) = init_result {
            let code = if active.contribution.is_some() {
                ErrorReasonCode::GitForkFailed
            } else {
                ErrorReasonCode::Internal
            };
            self.fail_active(code, &error.to_string()).await?;
            return Err(error.into());
        }
        let run_started = std::time::Instant::now();
        // Item #16(e): the daemon OWNS delivery — append explicit, secret-free instructions so
        // the agent commits its deliverable to git (the daemon pushes it) instead of guessing.
        // Read-on-start: inline the MEMORY.md index when memory is enabled (byte-identical
        // prompt when disabled).
        let memory_section = self.read_on_start_section();
        let prompt = compose_agent_prompt(
            &active.offer.task,
            &seller_cfg.git_remote,
            memory_section.as_deref(),
        );
        // Item #16(c): retry a transient agent error while the deadline still has room. The
        // feedback-kind error (fail_active, below) is published only after the attempt budget or
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
                self.fail_active(agent_error_reason_code(&error), &error.to_string())
                    .await?;
                return Err(error);
            }
        };
        let after_oid = seller_git::try_head_oid(&active.workdir);
        // Contribution scopes the agent-authorship gate to `base_oid..HEAD` (the base history is the
        // target's, not agent-authored); from-scratch requires the whole history agent-authored.
        let gate_result = match &active.contribution {
            Some(contribution) => seller_git::require_agent_authored_contribution(
                &active.workdir,
                &identity,
                contribution.base.oid(),
                after_oid.as_deref(),
            ),
            None => seller_git::require_agent_authored_delivery(
                &active.workdir,
                &identity,
                after_oid.as_deref(),
            ),
        };
        if let Err(error) = gate_result {
            self.fail_active(ErrorReasonCode::AgentRunFailed, &error.to_string())
                .await?;
            return Err(error.into());
        }

        // From-scratch: name the empty-base commits onto the job branch (best-effort). Contribution
        // is ALREADY on `branch` (set by init_contribution_workdir at base_oid), so skip the reset.
        if active.contribution.is_none() {
            let _ = seller_git::point_branch_at_head(&active.workdir, &branch);
        }

        // NIP-98: key from 0600 file → git child env only (never argv / never logged).
        let push_secret = home::read_secret_key_hex(&self.home)?;
        let push_auth = seller_git::PushAuth {
            secret_key_hex: push_secret,
        };
        let commit = match seller_git::push_branch_with_auth(
            &active.workdir,
            &seller_cfg.git_remote,
            &branch,
            Some(&push_auth),
        ) {
            Ok(oid) => oid,
            Err(error) => {
                // Display path must not echo the secret (SellerGitError is scrubbed).
                self.fail_active(ErrorReasonCode::GitPushFailed, &error.to_string())
                    .await?;
                return Err(error.into());
            }
        };
        drop(push_auth);

        let job_hash = job_hash_for_offer(&active.job_id, &active.offer.task, active.offer.amount);
        // The seller signs the RECEIPT PREIMAGE (binds the trade + the
        // delivered git object, D4) — not the bare job-hash. The buyer reconstructs this
        // exact preimage and co-signs it. `exec_metadata_commitment` is the empty marker:
        // exec-metadata is NOT covered by the co-signature (Item 2, seller-claimed).
        // Derive the delivery discriminator from the SAME typed `Delivery` the buyer's pay path
        // uses (was a `DeliveryKind::Fork` hardcode) so both sides agree by construction ("fork").
        let delivery_kind = seller_delivery_kind(&seller_cfg.git_remote, &branch, &commit)?;
        // Bind the seller-authored `creq` into the receipt so BOTH co-signatures
        // commit to the payment terms the seller published. The creq is reconstructed from the
        // SAME inputs used at claim time (`build_seller_creq` is pure over job id / amount / unit /
        // accepted_mints / seller key), so its hash equals the one the buyer read off the claim
        // and threaded through its pay path — the co-signatures agree by construction. The mint is
        // the realized mint the buyer pays at (the seller's default accepted mint), normalized as a
        // `MintUrl` exactly as the buyer builds it, so the two receipt bytes cannot drift.
        let authored_creq = gateway::creq::build_seller_creq(
            &active.job_id,
            active.offer.amount,
            &active.offer.unit,
            &self.home.config.accepted_mints,
            &self.seller_pubkey,
        )
        .map_err(|error| DaemonError::Seller(SellerError::Io(error.to_string())))?;
        let realized_mint = mint_url(self.home.config.default_mint())?.to_string();
        let preimage = crate::receipt::ReceiptPreimage {
            job_hash: job_hash.clone(),
            offer_id: active.job_id.clone(),
            amount: active.offer.amount,
            unit: "sat".to_owned(),
            mint: realized_mint,
            buyer_pubkey: active.buyer_pubkey.clone(),
            seller_pubkey: self.seller_pubkey.clone(),
            delivery_integrity_hash: commit.clone(),
            delivery_kind: delivery_kind.as_str().to_owned(),
            exec_metadata_commitment: crate::receipt::EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
            creq_hash: Some(gateway::creq_hash_hex(&authored_creq)),
        };
        let seller_sig = sign_receipt_hash(&self.keys, &preimage.digest_hex())?;
        // Harness-generic PUBLIC seller-claimed usage block (opportunistic;
        // absent fields stay absent). `usage` carries the ACP-native token/model/cost the driver
        // surfaced this run — `None` when the harness exposed nothing.
        let exec_metadata = seller_exec_metadata(
            &seller_cfg.agent_command,
            seller_cfg.agent.as_deref(),
            wall_time_ms,
            usage.as_ref(),
        );
        let mut draft = git_result_draft(
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
        // On a contribution, echo the pinned target + base + accepts and append the
        // seller's schnorr signature over the authorship tuple {job_id, seller_pubkey, target_repo,
        // base_oid, fork_ref, commit_oid}. The fork_ref is (this seller's git_remote, the
        // per-job unique branch); the fork tip is `commit`. The buyer verifies this sig at the
        // pre-pay seam and equality-checks the echo against its signed offer.
        if let Some(contribution) = &active.contribution {
            match crate::contribution::ForkRef::new(&seller_cfg.git_remote, &branch) {
                Ok(fork) => {
                    let tuple = crate::contribution::AuthorshipTuple {
                        job_id: active.job_id.clone(),
                        seller_pubkey: self.seller_pubkey.clone(),
                        target: contribution.target.clone(),
                        base_oid: contribution.base.oid().to_owned(),
                        fork,
                        commit_oid: commit.clone(),
                    };
                    let tuple_sig = crate::contribution::sign_authorship_tuple(&self.keys, &tuple);
                    draft
                        .tags
                        .extend(crate::contribution::contribution_result_tags(contribution, &tuple_sig));
                }
                Err(error) => {
                    self.fail_active(ErrorReasonCode::Internal, &error.to_string())
                        .await?;
                    return Err(DaemonError::Seller(SellerError::Io(error.to_string())));
                }
            }
        }
        let result_id = match publish_draft(&self.home, &self.keys, &draft).await {
            Ok(id) => id,
            Err(error) => {
                self.fail_active(ErrorReasonCode::Internal, &error.to_string())
                    .await?;
                return Err(error);
            }
        };
        if let Some(slot) = self.active.as_mut() {
            slot.result_id = Some(result_id.clone());
            // Stash the delivery facts so the terminal episode (paid at receipt, or
            // unpaid at eviction) is one complete append. `transcript_ref` is a POINTER to the
            // already-durable per-job transcript (`run_agent_job` wrote it); never a copy.
            let (harness, _) =
                harness_and_transport(&seller_cfg.agent_command, seller_cfg.agent.as_deref());
            slot.delivery = Some(DeliveryRecord {
                result_id: result_id.clone(),
                commit_oid: commit.clone(),
                git_remote: seller_cfg.git_remote.clone(),
                branch: branch.clone(),
                delivery_kind: delivery_kind.as_str().to_owned(),
                harness,
                wall_time_ms,
                usage: usage.clone(),
                transcript_ref: format!("seller-jobs/{}/seller-run.jsonl", active.job_id),
                deliver_ts: now_unix(),
            });
        }
        // DELIVERED announce: the result-kind result is published and pushed. Diagnostic only.
        self.announce(crate::announce::AnnounceEvent::delivered(
            now_unix(),
            &self.seller_pubkey,
            &active.job_id,
            &result_id,
            &commit,
            active.offer.amount,
        ));
        Ok(result_id)
    }

    async fn fail_active(
        &mut self,
        code: ErrorReasonCode,
        human_reason: &str,
    ) -> Result<(), DaemonError> {
        if let Some(active) = self.active.take() {
            let reason = seller_error_content(code, human_reason);
            // Capture the ERRORED terminal, threading `reason` (previously discarded)
            // into `error_reason`. Best-effort; the feedback-kind below is the buyer-facing surface.
            self.record_errored_episode(&active, &reason);
            // JOB-FAILED announce (with the machine reason). Diagnostic; the feedback-kind stays the
            // buyer-facing surface.
            self.announce(crate::announce::AnnounceEvent::job_failed(
                now_unix(),
                &self.seller_pubkey,
                &active.job_id,
                &reason,
            ));
            let draft = error_draft(
                &active.job_id,
                &active.buyer_pubkey,
                &self.seller_pubkey,
                reason,
            );
            let _ = publish_draft(&self.home, &self.keys, &draft).await;
        }
        Self::end_flight();
        Ok(())
    }

    /// Read-on-start: the rendered `MEMORY.md` section to inline into the job prompt, or
    /// `None` when memory is disabled or there is no non-empty index. Ensures the memory dir on
    /// first use (seeding operator-notes.md + a non-empty index). Best-effort: any error degrades
    /// to `None` (no memory), never blocks the job.
    fn read_on_start_section(&self) -> Option<String> {
        let cfg = &self.home.config.seller_memory;
        if !cfg.memory_enabled {
            return None;
        }
        let dir = crate::seller_memory::memory_dir(&self.home.root);
        if let Err(error) = crate::seller_memory::ensure_memory_dir(&dir) {
            eprintln!("seller memory: ensure dir failed (skipping read-on-start): {error}");
            return None;
        }
        match crate::seller_memory::read_on_start_section(
            &dir,
            cfg.read_on_start_template_path.as_deref(),
        ) {
            Ok(section) => section,
            Err(error) => {
                eprintln!("seller memory: read-on-start failed (skipping): {error}");
                None
            }
        }
    }

    /// Retro: resolve the plan for a delivered-PAID job, or `None` to skip. `None` when
    /// retro is disabled, the memory dir can't be prepared, or no `delivered_paid` episode exists
    /// for `job_id` (retro fires on delivered-paid ONLY — refusals/errors never reach here).
    /// Driver-free so it is testable without the `acp` feature.
    fn retro_context(&self, job_id: &str) -> Option<RetroPlan> {
        let cfg = &self.home.config.seller_memory;
        if !cfg.retro_enabled {
            return None;
        }
        let memory_dir = crate::seller_memory::memory_dir(&self.home.root);
        if let Err(error) = crate::seller_memory::ensure_memory_dir(&memory_dir) {
            eprintln!("seller retro: ensure memory dir failed (skip): {error}");
            return None;
        }
        let episode = match EpisodeLog::open(&self.home.root).last_delivered_paid(job_id) {
            Ok(Some(episode)) => episode,
            Ok(None) => return None,
            Err(error) => {
                eprintln!("seller retro: episode read failed (skip): {error}");
                return None;
            }
        };
        // Seed the retro with the episode + the ABSOLUTE transcript path (§ Retro: fresh turn
        // seeded with episode + transcript_ref; the transcript is a pointer, never copied).
        let episode_json = serde_json::to_string_pretty(&episode).unwrap_or_default();
        let transcript_abs = episode
            .transcript_ref
            .as_ref()
            .map(|rel| self.home.root.join(rel).display().to_string())
            .unwrap_or_else(|| "(no transcript recorded for this job)".to_owned());
        let prompt = crate::seller_memory::retro_prompt(
            &memory_dir,
            &episode_json,
            &transcript_abs,
            cfg.retro_prompt_path.as_deref(),
        );
        let seller_cfg = require_seller_config(&self.home).ok()?;
        let log_path = job_workdir(&self.home, job_id).join("seller-retro.jsonl");
        Some(RetroPlan {
            memory_dir,
            prompt,
            log_path,
            agent_command: seller_cfg.agent_command.clone(),
        })
    }

    /// Retro trigger: after a delivered-PAID receipt, run ONE best-effort agent turn to
    /// update memory. **Fully detached** — it MUST NOT run in the seller event loop: a retro is a
    /// whole agent turn (up to the job timeout, or a hang) and the loop is single-tasked, so an
    /// inline retro would stop the daemon from collecting kind-1059 payments (the money path) and
    /// from claiming offers until the retro finished (regression: "wraps parked, never collected").
    /// So it runs on its OWN OS thread with its OWN runtime and this call returns immediately;
    /// the money path never waits on retro. No-op without the `acp` feature.
    #[cfg(feature = "acp")]
    fn maybe_run_retro(&self, job_id: &str) {
        let Some(plan) = self.retro_context(job_id) else {
            return;
        };
        if plan.agent_command.is_empty() {
            return;
        }
        // AcpDriver is !Send, so it is BUILT inside the thread; `plan` is owned/Send.
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("seller retro: runtime build failed (skip): {error}");
                    return;
                }
            };
            if let Some(parent) = plan.log_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let timeout = Duration::from_secs(crate::seller::DEFAULT_JOB_TIMEOUT_SECS);
            let mut driver = crate::driver::AcpDriver::new(
                crate::driver::AgentCommand::new(
                    plan.agent_command[0].clone(),
                    plan.agent_command[1..].to_vec(),
                ),
                crate::driver::PermissionOutcome::Allow,
                timeout,
            );
            if let Err(error) = runtime.block_on(run_retro_turn(
                &mut driver,
                &plan.memory_dir,
                &plan.prompt,
                &plan.log_path,
            )) {
                eprintln!("seller retro: best-effort turn failed (money path unaffected): {error}");
            }
        });
    }

    #[cfg(not(feature = "acp"))]
    fn maybe_run_retro(&self, _job_id: &str) {}

    /// Best-effort episode append — diagnostic, NEVER fails a trade. Logs on error, swallows it.
    /// Episodes carry ids/amounts/mint/task-text/self-reported-usage only; no token/key material.
    fn write_episode(&self, episode: &Episode) {
        let log = EpisodeLog::open(&self.home.root);
        if let Err(error) = log.append(episode) {
            eprintln!(
                "seller episode capture failed (non-fatal) job_id={}: {error}",
                episode.job_id
            );
        }
        // Source of truth (the append above) is attempted FIRST; only then does the brain-telemetry
        // stream fan the same episode out to the pluggable sink/mirror. Best-effort and off the hot
        // path — a sink/mirror failure never blocks the seller loop or affects the append above.
        self.emit_telemetry(episode);
    }

    /// Best-effort brain/episode telemetry — fan one captured episode out to the `[telemetry]` sink
    /// command and/or mirror file so an operator can watch the seller's reasoning/economics live.
    /// NEVER blocks the event loop or the episode append: [`crate::telemetry::emit`] does the mirror
    /// write inline (fast, durable append) and dispatches the sink on its OWN detached thread,
    /// returning immediately; a missing/slow/hung/failing sink is logged once and swallowed. No-op
    /// when the channel is disabled or unpointed. Telemetry carries only the episode (ids/amounts/
    /// task-text/self-reported-usage) + the public seller pubkey/job id — never a token/key/secret.
    fn emit_telemetry(&self, episode: &Episode) {
        let cfg = &self.home.config.telemetry;
        if !cfg.enabled {
            return;
        }
        crate::telemetry::emit(cfg, &crate::telemetry::TelemetryEvent::episode(now_unix(), episode));
    }

    /// Best-effort lifecycle announce — dispatch one JSON event to the configured
    /// `[seller_announce]` sink command. NEVER blocks the event loop: [`crate::announce::dispatch`]
    /// runs the whole spawn+write+bounded-wait on its OWN detached thread and returns immediately;
    /// a missing/slow/hung/failing sink is logged once and swallowed. No-op when no sink is
    /// configured (feature OFF ⇒ zero behavior change). Announce events carry only ids/amounts/
    /// reasons already public on the relay or in the seller log — never a token/key/plaintext.
    fn announce(&self, event: crate::announce::AnnounceEvent) {
        let cfg = &self.home.config.seller_announce;
        if !cfg.is_enabled() {
            return;
        }
        crate::announce::dispatch(
            &cfg.command,
            Duration::from_millis(cfg.timeout_ms),
            &event,
        );
    }

    /// Refused terminal. Best-effort re-parse of the offer for its facts (classify does
    /// not hand them back). No episode for a non-offer, a dedup re-see, or an unparseable event —
    /// those are not freshly-classified jobs.
    fn record_refused_episode(&self, event: &nostr_sdk::Event, skip: &OfferSkip) {
        if matches!(skip, OfferSkip::NotAnOffer { .. } | OfferSkip::AlreadyProcessed) {
            return;
        }
        let draft = event_to_draft(event);
        let Ok(offer) = parse_offer(&draft) else {
            return; // Unparseable ⇒ no offer facts to record.
        };
        let rate = require_seller_config(&self.home)
            .map(|cfg| cfg.rate_sats)
            .unwrap_or(0);
        let contribution = crate::contribution::parse_contribution_offer(&draft.tags)
            .ok()
            .flatten();
        let mut episode = Episode::new(
            EpisodeKind::Refused,
            EpisodeOutcome::Refused,
            now_unix(),
            &self.seller_pubkey,
            event.id.to_hex(),
        );
        fill_offer_facts(
            &mut episode,
            &offer,
            &event.pubkey.to_hex(),
            rate,
            self.home.config.default_mint(),
            contribution.as_ref(),
        );
        episode.refusal_reason_code = Some(skip.code().to_owned());
        episode.refusal_reason = Some(skip.reason());
        self.write_episode(&episode);
    }

    /// Errored terminal (claimed job that failed before or during delivery).
    fn record_errored_episode(&self, active: &ActiveJob, reason: &str) {
        let rate = require_seller_config(&self.home)
            .map(|cfg| cfg.rate_sats)
            .unwrap_or(0);
        let mut episode = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::Errored,
            now_unix(),
            &self.seller_pubkey,
            &active.job_id,
        );
        fill_offer_facts(
            &mut episode,
            &active.offer,
            &active.buyer_pubkey,
            rate,
            self.home.config.default_mint(),
            active.contribution.as_ref(),
        );
        episode.claim_id = Some(active.claim_id.clone());
        episode.resolved_deadline_unix = Some(active.deadline_unix);
        episode.error_reason = Some(reason.to_owned());
        if let Some(delivery) = &active.delivery {
            fill_delivery_facts(&mut episode, delivery);
        }
        self.write_episode(&episode);
    }

    /// Delivered-PAID terminal. Complete episode: offer + claim + delivery + payment.
    fn record_paid_episode(&self, job: &ActiveJob, amount_received: u64, expected_amount: u64) {
        let rate = require_seller_config(&self.home)
            .map(|cfg| cfg.rate_sats)
            .unwrap_or(0);
        let mut episode = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::DeliveredPaid,
            now_unix(),
            &self.seller_pubkey,
            &job.job_id,
        );
        fill_offer_facts(
            &mut episode,
            &job.offer,
            &job.buyer_pubkey,
            rate,
            self.home.config.default_mint(),
            job.contribution.as_ref(),
        );
        episode.claim_id = Some(job.claim_id.clone());
        episode.resolved_deadline_unix = Some(job.deadline_unix);
        if let Some(delivery) = &job.delivery {
            fill_delivery_facts(&mut episode, delivery);
        }
        episode.amount_received = Some(amount_received);
        episode.expected_amount = Some(expected_amount);
        episode.swap_ok = Some(true);
        episode.collect_ts = Some(now_unix());
        self.write_episode(&episode);
    }

    /// Delivered-but-UNPAID terminal (awaiting-payment backpressure eviction).
    fn record_delivered_unpaid_episode(&self, job: &ActiveJob) {
        let rate = require_seller_config(&self.home)
            .map(|cfg| cfg.rate_sats)
            .unwrap_or(0);
        let mut episode = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::DeliveredUnpaid,
            now_unix(),
            &self.seller_pubkey,
            &job.job_id,
        );
        fill_offer_facts(
            &mut episode,
            &job.offer,
            &job.buyer_pubkey,
            rate,
            self.home.config.default_mint(),
            job.contribution.as_ref(),
        );
        episode.claim_id = Some(job.claim_id.clone());
        episode.resolved_deadline_unix = Some(job.deadline_unix);
        if let Some(delivery) = &job.delivery {
            fill_delivery_facts(&mut episode, delivery);
        }
        episode.expected_amount = Some(job.offer.amount);
        self.write_episode(&episode);
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

/// Opportunistic driver usage → serde-friendly episode mirror (absent-stays-absent;
/// a field the harness did not surface stays `None`, never zero-filled).
fn usage_record(usage: Option<&UsageMetadata>) -> UsageRecord {
    let Some(usage) = usage else {
        return UsageRecord::default();
    };
    UsageRecord {
        model: usage.model.clone(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cost_amount: usage.cost.as_ref().map(|cost| cost.amount.clone()),
        cost_basis: usage.cost.as_ref().map(|cost| cost.basis.clone()),
        // Transport is a property of the harness/adapter (see `harness_and_transport`), not of a
        // per-run usage capture, so the episode's usage mirror does not carry it.
        transport: None,
    }
}

/// Fill the offer-facts group shared by every episode kind. Pure over its inputs.
fn fill_offer_facts(
    episode: &mut Episode,
    offer: &ParsedOffer,
    buyer_pubkey: &str,
    rate_sats: u64,
    // `episode.mint` is sourced from the seller's default accepted mint rather
    // than `offer.mint_url` (consumer re-pointed off the offer). Job E replaces this with the
    // realized mint from the buyer's payment payload.
    mint: &str,
    contribution: Option<&ContributionOffer>,
) {
    episode.offer_task = offer.task.clone();
    episode.output_type = offer.output.clone();
    episode.amount = offer.amount;
    episode.unit = offer.unit.clone();
    episode.mint = mint.to_owned();
    episode.deadline_unix = offer.deadline_unix;
    episode.offer_target = offer.seller_pubkey.clone();
    episode.buyer_pubkey = buyer_pubkey.to_owned();
    episode.configured_rate_sats = rate_sats;
    match contribution {
        Some(contribution) => {
            episode.job_class = "contribution".to_owned();
            episode.contribution_target = Some(contribution.target.clone_url().to_owned());
            episode.contribution_base_oid = Some(contribution.base.oid().to_owned());
        }
        None => episode.job_class = "from_scratch".to_owned(),
    }
}

/// Fill the delivery-facts group from a captured [`DeliveryRecord`].
fn fill_delivery_facts(episode: &mut Episode, delivery: &DeliveryRecord) {
    episode.result_id = Some(delivery.result_id.clone());
    episode.commit_oid = Some(delivery.commit_oid.clone());
    episode.fork_git_remote = Some(delivery.git_remote.clone());
    episode.fork_branch = Some(delivery.branch.clone());
    episode.delivery_kind = Some(delivery.delivery_kind.clone());
    episode.usage = usage_record(delivery.usage.as_ref());
    episode.wall_time_ms = Some(delivery.wall_time_ms);
    episode.harness = Some(delivery.harness.clone());
    episode.transcript_ref = Some(delivery.transcript_ref.clone());
    episode.deliver_ts = Some(delivery.deliver_ts);
}

/// Delivery discriminator for the seller's commit/fork delivery, derived from the SAME typed
/// [`GitDelivery`](crate::delivery::GitDelivery) the buyer's pay path uses — NOT a hardcoded
/// label — so buyer and seller derive it from one abstraction (`"fork"`). Fails closed if the
/// just-pushed fields somehow do not type (impossible on the success path — a git push returns
/// a canonical oid); never silently relabels or emits a bogus kind.
fn seller_delivery_kind(
    git_remote: &str,
    branch: &str,
    commit_oid: &str,
) -> Result<crate::receipt::DeliveryKind, DaemonError> {
    let delivery = crate::delivery::GitDelivery::new(
        git_remote.to_owned(),
        branch.to_owned(),
        crate::delivery::CommitOid::parse(commit_oid.to_owned())
            .map_err(|error| DaemonError::Policy(format!("delivery oid: {error}")))?,
    )
    .map_err(|error| DaemonError::Policy(format!("delivery typing: {error}")))?;
    Ok(delivery.delivery_kind())
}

/// Build the seller-claimed PUBLIC usage block for a result-kind result.
///
/// This block is PUBLIC and harness-generic. It is **opportunistic**:
/// emit only fields the seller can source. `harness` is resolved from the configured preset
/// label (else the agent command), `wall_time` is measured, and
/// `metadata_trust=seller-claimed` is required whenever any field is present (anchor rule).
///
/// `usage_transport` is the harness/adapter's declared capture axis (`acp-native` for the
/// codex adapter, `side-channel` otherwise), resolved from the configured harness identity.
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
    let (harness, transport) = harness_and_transport(agent_command, agent_preset);
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

/// Best-effort harness id + usage transport.
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
    // A non-built-in label is a config-defined `[agents]` preset: the preset name IS the
    // harness identity (conservative `side-channel` transport — nothing is known about it).
    if let Some(preset) = agent_preset {
        match preset.trim().to_ascii_lowercase().as_str() {
            "claude" => return ("claude-agent-acp".to_owned(), "side-channel"),
            "codex" => return ("codex-acp-ng".to_owned(), "acp-native"),
            "cursor" => return ("cursor-agent".to_owned(), "side-channel"),
            "" => {}
            _ => return (preset.trim().to_owned(), "side-channel"),
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
/// is surfaced to the caller — which then publishes the feedback-kind error exactly once — ONLY
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
            // error so the caller publishes feedback-kind exactly once (past deadline / exhausted).
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
fn compose_agent_prompt(task: &str, git_remote: &str, memory_section: Option<&str>) -> String {
    let base = format!(
        "{task}\n\n\
         ---\n\
         DELIVERY (required). Deliver your work by committing it with git in your current \
         working directory:\n\
         - Make one or more non-empty commits authored by you. Do not leave the deliverable \
         uncommitted and do not only print it to the console.\n\
         - You do NOT need to push and you are NOT handed any credentials: the daemon pushes \
         your committed branch to the bound git remote ({git_remote}) on your behalf.\n\
         Anything not committed to git will not be delivered."
    );
    // Read-on-start: when memory is enabled the rendered index section is appended.
    // When `None` (memory_enabled=false, or no non-empty index) the output is byte-IDENTICAL to
    // the memory-disabled prompt (golden invariant).
    match memory_section {
        Some(section) => format!("{base}\n\n{section}"),
        None => base,
    }
}

/// Kind-feedback `status=error` draft for a targeted under-rate refusal. Content carries the
/// machine-readable rate-gate reason (same string the skip log already has) so buyers can
/// distinguish rate-refusal from a crash / empty-content failure.
fn under_rate_error_draft(
    offer_id: &str,
    buyer_pubkey: &str,
    seller_pubkey: &str,
    reason: &str,
) -> EventDraft {
    error_draft(offer_id, buyer_pubkey, seller_pubkey, reason)
}

fn seller_error_content(code: ErrorReasonCode, human: &str) -> String {
    let sanitized = truncate_human_reason(&strip_absolute_paths(human), 300);
    let human = if sanitized.trim().is_empty() {
        "seller aborted the job without a detailed reason"
    } else {
        sanitized.trim()
    };
    format!("{}: {human}", code.as_str())
}

fn strip_absolute_paths(input: &str) -> String {
    input
        .split_whitespace()
        .map(strip_absolute_path_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_absolute_path_token(token: &str) -> String {
    let mut first_path = None;
    for (idx, ch) in token.char_indices() {
        if ch == '/' {
            first_path = Some(idx);
            break;
        }
    }
    let Some(start) = first_path else {
        return token.to_owned();
    };
    if start > 0 && token[..start].ends_with(':') && token[start..].starts_with("//") {
        return token.to_owned();
    }
    if start > 0 {
        let prefix = &token[..start];
        if prefix
            .chars()
            .last()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            return token.to_owned();
        }
    }

    let suffix_start = token[start..]
        .find(|ch: char| matches!(ch, ',' | ';' | ':' | ')' | ']' | '}'))
        .map(|offset| start + offset)
        .unwrap_or(token.len());
    format!("{}<path>{}", &token[..start], &token[suffix_start..])
}

fn truncate_human_reason(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in input.chars().take(max_chars) {
        out.push(ch);
    }
    out
}

fn agent_error_reason_code(error: &DaemonError) -> ErrorReasonCode {
    match error {
        DaemonError::AcpRequired => ErrorReasonCode::AgentSpawnFailed,
        DaemonError::Agent(message) => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("failed to spawn") || lower.contains("no such file") {
                ErrorReasonCode::AgentSpawnFailed
            } else if lower.contains("timed out")
                || lower.contains("timeout")
                || lower.contains("deadline")
            {
                ErrorReasonCode::AgentTimeout
            } else {
                ErrorReasonCode::AgentRunFailed
            }
        }
        _ => ErrorReasonCode::AgentRunFailed,
    }
}

fn contribution_refusal_error_content(skip: &OfferSkip) -> String {
    match skip {
        OfferSkip::ContributionUnsupported => {
            seller_error_content(ErrorReasonCode::ContributionUnsupported, &skip.reason())
        }
        OfferSkip::ContributionMalformed { .. } => {
            seller_error_content(ErrorReasonCode::ContributionMalformed, &skip.reason())
        }
        _ => seller_error_content(ErrorReasonCode::Internal, &skip.reason()),
    }
}

fn reconcile_error_content(liveness: ClaimLiveness) -> String {
    seller_error_content(ErrorReasonCode::ClaimReleased, reconcile_reason(liveness))
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

/// Publish one addressable kind-30340 heartbeat off the daemon loop's tick. This is best-effort
/// liveness/discovery signal: it MUST never crash or wedge the loop, so a publish failure or a
/// hung relay is bounded by a timeout and log-and-continue (the loop keeps serving offers and
/// collecting payments regardless). No-op when there is no `[seller]` config.
async fn publish_heartbeat(daemon: &SellerDaemon) {
    let Some(draft) = daemon.heartbeat_draft() else {
        return;
    };
    let event = draft.to_event_draft();
    let publish = publish_draft(&daemon.home, &daemon.keys, &event);
    match tokio::time::timeout(std::time::Duration::from_secs(10), publish).await {
        Ok(Ok(id)) => eprintln!(
            "seller heartbeat published id={id} accepting={} queue_depth={} rate_sats={}",
            draft.accepting, draft.queue_depth, draft.rate_sats
        ),
        Ok(Err(error)) => {
            eprintln!("seller heartbeat publish failed (continuing): {error}")
        }
        Err(_) => eprintln!("seller heartbeat publish timed out (continuing)"),
    }
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

/// Everything a retro turn needs, resolved WITHOUT a driver so it is testable and works
/// under `--no-default-features` (no `acp`). `None` from [`SellerDaemon::retro_context`] means "do
/// not run a retro" (disabled, or no paid episode to distill).
#[derive(Debug, Clone)]
struct RetroPlan {
    memory_dir: PathBuf,
    prompt: String,
    log_path: PathBuf,
    agent_command: Vec<String>,
}

/// Retro write-back: run ONE best-effort agent turn whose session cwd is the memory dir
/// (so the agent can read/write `MEMORY.md` and topic files by relative path), seeded with the
/// retro prompt. Merge-not-clobber is enforced at RUNTIME here, not by prompt prose: operator-owned
/// files are snapshotted before the turn and byte-restored after — regardless of what the agent
/// did. Generic over [`Driver`] so it is exercised with the mock driver in tests and an
/// `AcpDriver` in production.
///
/// The money path NEVER waits on this: the caller invokes it only after the receipt is journaled,
/// and swallows any error it returns.
async fn run_retro_turn<D: crate::driver::Driver>(
    driver: &mut D,
    memory_dir: &Path,
    prompt: &str,
    log_path: &Path,
) -> Result<(), DaemonError> {
    use crate::driver::{ContentBlock, PromptTurn, SessionConfig};
    use crate::engine::{run_job, RunParams};
    use crate::event::{JobExecutionStatus, JobId};
    use crate::log::EventLog;

    // Merge-not-clobber (runtime): capture operator files BEFORE the turn.
    let snapshot = crate::seller_memory::snapshot_operator_files(memory_dir)
        .map_err(|error| DaemonError::Agent(format!("retro snapshot: {error}")))?;
    let mut log =
        EventLog::open(log_path).map_err(|error| DaemonError::Agent(error.to_string()))?;
    let params = RunParams {
        session_config: SessionConfig {
            // cwd = memory dir ⇒ the retro turn's writes land in `memory/` by relative path.
            cwd: memory_dir.to_path_buf(),
            mcp_servers: Vec::new(),
            env: Vec::new(),
        },
        prompt: PromptTurn {
            input: vec![ContentBlock::Text {
                text: prompt.to_owned(),
            }],
        },
    };
    let outcome = run_job(driver, &mut log, &JobId("seller-retro".to_owned()), params, &mut |_| {})
        .await;
    // Restore operator files whatever the turn did (success, failure, or clobber attempt).
    let restore = crate::seller_memory::restore_snapshot(&snapshot);
    let outcome = outcome.map_err(|error| DaemonError::Agent(error.to_string()))?;
    restore.map_err(|error| DaemonError::Agent(format!("retro restore: {error}")))?;
    match outcome.terminal {
        JobExecutionStatus::Completed => Ok(()),
        other => Err(DaemonError::Agent(format!("retro terminal {other:?}"))),
    }
}

/// Handle one gift-wrap: unwrap (one site), then apply or buffer.
pub async fn ingest_gift_wrap(
    daemon: &mut SellerDaemon,
    event: &nostr_sdk::Event,
) -> Result<Option<ReceiptOutcome>, DaemonError> {
    let event_id = event.id.to_hex();
    // #57: log EVERY wrap the seller sees — silence must mean "no wraps", never "lost money".
    // The applied case logs a receipt at the caller; the not-ours / buffered cases log here.
    eprintln!("seller wrap seen event={event_id}");
    let Some(received) = unwrap_own_payment_gift_wrap(&daemon.keys, event).await? else {
        eprintln!("seller wrap event={event_id}: not a decodable own-payment wrap (skipped)");
        return Ok(None);
    };
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
    let payload_job = received.payload.job_id().to_owned();
    // #57: an already-receipted job's wrap is a re-see of consumed money (idempotent) — skip it,
    // do NOT buffer it forever. The journal pay-once guard is the source of truth.
    if daemon.journal.has_receipt(&payload_job).unwrap_or(false) {
        eprintln!(
            "seller wrap event={event_id}: job {payload_job} already receipted, skipping (not buffered)"
        );
        return Ok(ApplyResult::Buffered);
    }
    // Does a delivered-but-unpaid job bind this payment? Bind by job id (the NUT-18 payload's
    // `id`) — result id is resolved locally in `try_apply_payment` (NUT-18 carries no result id).
    let binds = daemon
        .awaiting_payment
        .iter()
        .any(|job| job.job_id.as_str() == payload_job);
    if !binds {
        // No delivered job matches yet. Before buffering, try to reconstruct the delivered-job
        // bind from ON-RELAY ground truth: a seat that delivered under an older build (or whose
        // journal is lost/incomplete) has no local `Delivery` entry for `restore_delivered_unpaid`
        // to rebuild, so the wrap would otherwise buffer forever (the exact recovery case the
        // backfill exists for). Reconstruction is fail-closed (see `reconstruct_delivered_bind`) —
        // it binds only a job whose result AND claim this seller itself authored, and hands the
        // job to the SAME redeem path with its full guards. A miss (early pay for a
        // still-processing job, an unverifiable wrap) leaves the wrap buffered as before;
        // misattribution is impossible — `try_apply_payment` only receives against an exact
        // job+result match.
        if !daemon.reconstruct_bind_from_relay(&received).await {
            eprintln!(
                "seller wrap event={event_id} buffered: no delivered job binds job_id={} yet",
                received.payload.job_id()
            );
            daemon.buffer_payment(event_id, received);
            return Ok(ApplyResult::Buffered);
        }
        // Bind restored from the relay — fall through to the normal apply path (full redeem guards).
    }
    match daemon.try_apply_payment(received).await? {
        Some(outcome) => Ok(ApplyResult::Applied(outcome)),
        None => {
            eprintln!("seller wrap event={event_id}: bound job not applied (re-buffered)");
            Ok(ApplyResult::Buffered)
        }
    }
}

/// Reconcile buffered payments after result publish (B2 early-pay). Applies every buffered
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

/// Env override for the boot push-preflight (see [`boot_preflight_enabled`]).
const BOOT_PUSH_PREFLIGHT_ENV: &str = "MOBEE_SELLER_BOOT_PUSH_PREFLIGHT";

/// Boot push-preflight gate: probe iff the `[seller_preflight]` knob is on AND the env override does
/// not disable it. An env value of `0/false/no/off` disables (the tests-off switch); the env can
/// never force the probe on when the config knob is off. Pure over (config, env) → unit-testable.
fn boot_preflight_enabled(config: &crate::home::MobeeConfig, env_override: Option<&str>) -> bool {
    if let Some(value) = env_override {
        let v = value.trim().to_ascii_lowercase();
        if matches!(v.as_str(), "0" | "false" | "no" | "off") {
            return false;
        }
    }
    config.seller_preflight.boot_push_preflight
}

/// Format the boot-preflight outcome into ONE log line. NEVER returns an error — a failed probe is
/// advisory: the daemon names the git-version cause + fix and keeps running (reads/claims still work;
/// per-job deliveries fail-close per job as today). Pure over the probe result, so the probe seam is
/// mockable in tests without git or a relay.
fn boot_preflight_outcome(result: Result<(), crate::seller_git::SellerGitError>) -> String {
    match result {
        Ok(()) => "seller boot push preflight OK (write-auth path reachable)".to_owned(),
        Err(error) => format!(
            "seller WARN: boot push preflight FAILED ({error}). Most likely cause: git < 2.54 \
             silently drops the Authorization credential on the git-receive-pack POST (reads work, \
             pushes 401) — run `git version` and upgrade to 2.54+ (or run `mobee doctor`). \
             Continuing to run: reads and claims still work; deliveries will fail-close per job \
             until this is fixed."
        ),
    }
}

/// Run the boot push-preflight through a mockable probe seam. Returns the log line to emit, or
/// `None` when the preflight is disabled. Never blocks boot and never errors.
fn run_boot_push_preflight<P>(enabled: bool, probe: P) -> Option<String>
where
    P: FnOnce() -> Result<(), crate::seller_git::SellerGitError>,
{
    if !enabled {
        return None;
    }
    Some(boot_preflight_outcome(probe()))
}

/// Wire the real probe seam from a live daemon and emit the outcome to stderr. Only probes when a
/// relay-git canonical delivery repo is configured — public/BYO https deliveries push to the
/// seller's own remote, outside the relay NIP-98 write-auth surface this guards. Best-effort: a
/// missing seller key or config just skips the probe. Never blocks boot.
fn run_boot_push_preflight_for_daemon(daemon: &SellerDaemon) {
    let enabled = boot_preflight_enabled(
        &daemon.home.config,
        std::env::var(BOOT_PUSH_PREFLIGHT_ENV).ok().as_deref(),
    );
    let Some(seller) = daemon.home.config.seller.clone() else {
        return;
    };
    if !crate::delivery_transport::is_relay_git_locator(&seller.git_remote) {
        return;
    }
    let auth = crate::home::read_secret_key_hex(&daemon.home)
        .ok()
        .map(|secret_key_hex| crate::seller_git::PushAuth { secret_key_hex });
    let outcome = run_boot_push_preflight(enabled, || {
        crate::seller_git::preflight_push_probe(&seller.git_remote, auth.as_ref())
    });
    if let Some(line) = outcome {
        eprintln!("{line}");
    }
}

/// Outcome of [`wait_for_nip42_auth`].
enum AuthWait {
    /// The relay issued a NIP-42 challenge and `automatic_authentication` completed it.
    Authenticated,
    /// The relay issued NO challenge within the window. NOT a failure (see the fn doc).
    NoChallenge,
}

/// Drain `notifications` until the relay's NIP-42 AUTH completes, the relay actively rejects
/// auth (fatal), or the window elapses with no challenge (`NoChallenge`, non-fatal).
///
/// Caller must subscribe `relay.notifications()` **before** `connect` so the
/// `Authenticated` event cannot be missed.
///
/// mobee-relay p-gates kind-1059: unauthenticated `REQ kinds:[1059] #p:self` is
/// `CLOSED` with `restricted:` (not `auth-required:`). nostr-sdk 0.44 treats
/// `restricted:` as `Remove` — the sub is dropped, so the post-auth
/// `resubscribe()` never restores it. Auth **before** the 1059 subscribe is
/// therefore load-bearing for seller receive, and mobee-relay challenges on connect so
/// `Authenticated` arrives in milliseconds.
///
/// A window with NO challenge is reported as `NoChallenge` rather than a fatal error: a relay
/// that challenges only lazily (on the first `REQ`/`EVENT`, e.g. the in-process test relay) will
/// challenge when the daemon subscribes below, and `automatic_authentication` completes auth
/// then. The caller logs the degrade loudly. An ACTIVE rejection (`AuthenticationFailed`) or a
/// relay shutdown stays fatal (fail-closed), unchanged.
async fn wait_for_nip42_auth(
    notifications: &mut tokio::sync::broadcast::Receiver<nostr_sdk::pool::RelayNotification>,
    timeout: std::time::Duration,
) -> Result<AuthWait, DaemonError> {
    use nostr_sdk::pool::RelayNotification;

    let within_window = tokio::time::timeout(timeout, async {
        loop {
            match notifications.recv().await {
                Ok(RelayNotification::Authenticated) => return Ok(AuthWait::Authenticated),
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
    .await;

    // Elapsed with no challenge → NoChallenge (non-fatal). Within the window → the loop's result
    // (Authenticated, or a fatal active failure).
    within_window.unwrap_or(Ok(AuthWait::NoChallenge))
}

/// Cap on the number of STORED offers the UNTARGETED (open-pool) offer filter requests on
/// (re)subscribe when a backfill window is set — a flood guard on the INITIAL query only.
/// nostr 0.44 `Filter::limit` is "maximum number of events returned in the initial query"
/// (`nostr-0.44.4/src/filter.rs`): it bounds the stored-events burst, does NOT affect live
/// streaming, and is NOT part of `match_event`. So even a very large `offer_backfill_secs` can
/// pull at most this many stored open-pool offers (the rest arrive live). Distinct from
/// `limit(0)` (the live-only sentinel: request ZERO stored events).
const OFFER_BACKFILL_LIMIT: usize = 500;

/// Build the seller's offer-kind offer subscription filter(s).
///
/// Always includes the TARGETED filter (`#p` == seller pubkey) in its ORIGINAL shape — no
/// `since`, no `limit` — at ALL `backfill_secs` values: stored targeted offers are addressed to
/// this seller and have always backfilled in full on (re)subscribe (p-pinned + low-volume ⇒ no
/// firehose risk; bounding it would be a pure regression). Staleness
/// protection on this path is the classify-level deadline-expiry refusal, not a filter bound.
///
/// When `claim_open_pool` is set, ALSO returns an UNtargeted filter (no pubkey pin): open-pool
/// offers carry no `p` tag, so a pubkey-pinned filter alone never delivers them and
/// `--claim-open-pool` is DOA. A targeted offer matches BOTH filters (deduped by event id at
/// the call site); the downstream money-safety gates (`classify_offer` rate/expiry + the
/// backfill pre-claim check) still decide whether any backfilled offer is actually claimed.
///
/// The backfill window applies to the UNTARGETED filter ONLY — the field gap it fixes is an
/// open-pool offer posted before the daemon came online, invisible by design
/// under the live-only bound:
///  * `backfill_secs == 0` → **live-only** (byte-identical pre-backfill shape):
///    `since(subscribe_now)` + `limit(0)` (request ZERO stored events). Only open-pool offers
///    posted WHILE the daemon runs are delivered — no full-history offer firehose on startup.
///  * `backfill_secs > 0` → `since(subscribe_now - backfill_secs).limit(OFFER_BACKFILL_LIMIT)`:
///    stored open-pool offers within the window backfill (bounded burst); `limit(0)` is DROPPED
///    (it would suppress every stored event and defeat the window).
fn offer_subscription_filters(
    seller_pubkey: nostr_sdk::PublicKey,
    claim_open_pool: bool,
    subscribe_now: nostr_sdk::Timestamp,
    backfill_secs: u64,
) -> Vec<nostr_sdk::Filter> {
    use nostr_sdk::prelude::{Filter, Kind, Timestamp};

    // TARGETED (`#p == self`): ORIGINAL shape, untouched by the window knob. The `#t=mobee`
    // namespace guard is required so a foreign event squatting the offer kind is never even
    // delivered.
    let mut filters = vec![Filter::new()
        .kind(Kind::Custom(JOB_OFFER_KIND))
        .hashtag(gateway::MOBEE_TAG)
        .pubkey(seller_pubkey)];

    if claim_open_pool {
        // UNtargeted (open-pool): the backfill window applies HERE only. `since` anchor:
        // `now - backfill_secs` (saturating); backfill_secs == 0 ⇒ `since(now)`.
        let since = Timestamp::from(subscribe_now.as_secs().saturating_sub(backfill_secs));
        let untargeted = Filter::new()
            .kind(Kind::Custom(JOB_OFFER_KIND))
            .hashtag(gateway::MOBEE_TAG)
            .since(since);
        let untargeted = if backfill_secs > 0 {
            // Window requested: bounded stored-offer burst (drop the live-only `limit(0)`).
            untargeted.limit(OFFER_BACKFILL_LIMIT)
        } else {
            // Live-only: `limit(0)` requests ZERO stored events (byte-identical pre-backfill).
            untargeted.limit(0)
        };
        filters.push(untargeted);
    }
    filters
}

/// The seller's LIVE offer subscription(s), grouped as they are registered on the relay.
///
/// Each element is ONE long-lived subscription — a single NIP-01 `REQ` whose filters the relay
/// OR-matches. The offer filters are grouped into ONE subscription: the pinned (`#p` ==
/// self) filter AND — under `claim_open_pool` — the un-pinned open-pool filter ride the SAME
/// `REQ`. This grouping is load-bearing: an earlier half-fix registered the un-pinned filter as
/// a SEPARATE second subscription, which delivered stored events (backfill) but no LIVE offers —
/// a running open-pool seller never reacted to a fresh untargeted offer, only claiming it after
/// a restart re-fetched it from stored events. Callers MUST subscribe each group as one `REQ`
/// (one `pool().subscribe(filters, ..)` call), never one subscription per filter.
fn offer_subscriptions(
    seller_pubkey: nostr_sdk::PublicKey,
    claim_open_pool: bool,
    subscribe_now: nostr_sdk::Timestamp,
    backfill_secs: u64,
) -> Vec<Vec<nostr_sdk::Filter>> {
    vec![offer_subscription_filters(
        seller_pubkey,
        claim_open_pool,
        subscribe_now,
        backfill_secs,
    )]
}

/// The seller's kind-1059 payment (gift-wrap) filter: p-tagged to the seller, **NO `t=mobee`
/// hashtag**. NIP-59 gift-wraps are opaque and CANNOT carry a namespace tag, so a hashtag filter
/// here would match zero wraps and silently strand real payments (#57). This is the tag-free
/// invariant the regression test pins; it is used for BOTH the live subscription and the boot
/// backfill.
fn wrap_subscription_filter(seller_pubkey: nostr_sdk::PublicKey) -> nostr_sdk::Filter {
    nostr_sdk::Filter::new()
        .kind(nostr_sdk::Kind::GiftWrap)
        .pubkey(seller_pubkey)
}

/// Cap on the number of most-recently-seen offer ids retained for in-loop dedup.
///
/// The dedup set is DEFENSE-IN-DEPTH only (see [`offer_event_should_process`]): it collapses an
/// offer that matched >1 filter, or a reconnect re-delivery, into one `on_offer_event` call. It
/// is NOT the money-idempotency guard — that is the DURABLE journal `has_claim` check in
/// `classify_offer`, which skips any already-claimed offer regardless of this set (and which the
/// daemon relies on wholesale after any restart, when this set starts empty). So forgetting the
/// OLDEST ids is safe: it cannot enable a second claim, only a re-check against the journal.
/// Without a bound the set would grow by one `EventId` per offer for the seller's whole lifetime
/// (a slow leak); 10k recent ids is ample to dedup filter-overlap and reconnect re-delivery.
const SEEN_OFFERS_CAP: usize = 10_000;

/// Insertion-ordered bounded set of seen offer ids.
///
/// A `VecDeque` holds ids in insertion order (oldest at the front) for O(1) eviction; the
/// `HashSet` gives O(1) membership. On an insert that grows the set past [`SEEN_OFFERS_CAP`] the
/// oldest id is evicted from BOTH. A recently-seen id is still reported "already seen"; only ids
/// older than the last `SEEN_OFFERS_CAP` distinct ids are forgotten. The two structures stay in
/// lockstep: an id is pushed to `order` exactly when it is newly added to `set`, and eviction
/// pops the front of `order` and removes that same id from `set`, so `order` never holds a
/// duplicate and `order.len() == set.len()` always.
#[derive(Default)]
struct BoundedSeen {
    order: VecDeque<nostr_sdk::EventId>,
    set: std::collections::HashSet<nostr_sdk::EventId>,
}

impl BoundedSeen {
    /// Insert `id`, returning `true` iff it was NEW — matching the `HashSet::insert` bool
    /// semantic [`offer_event_should_process`] relies on. A currently-retained id returns
    /// `false` (already seen). When the insert grows the set past [`SEEN_OFFERS_CAP`], the
    /// oldest retained id is evicted from both structures.
    fn insert(&mut self, id: nostr_sdk::EventId) -> bool {
        if !self.set.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > SEEN_OFFERS_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.set.len()
    }
}

/// Whether a relay event in the seller loop should be handed to `on_offer_event`.
///
/// True iff the event is a offer-kind offer not seen before. Routing is by KIND ONLY: a
/// non-p-tagged (open-pool) offer routes exactly like a targeted one, so the notification path
/// never drops untargeted offers. The event-id dedup makes a targeted offer that matched more
/// than one offer filter (or a reconnect re-delivery) reach `on_offer_event` at most once. The
/// set is bounded ([`BoundedSeen`]); forgetting an OLD id is money-safe (durable `has_claim`).
fn offer_event_should_process(
    event_kind: u16,
    event_id: nostr_sdk::EventId,
    seen_offers: &mut BoundedSeen,
) -> bool {
    event_kind == JOB_OFFER_KIND && seen_offers.insert(event_id)
}

/// Optional hooks for the seller loop, kept crate-internal so the public entrypoint stays a
/// one-arg [`run_forever`]. Production uses [`RunHooks::default`]; the integration test supplies
/// a readiness sender and a short auth-wait to drive the loop deterministically.
#[derive(Default)]
pub(crate) struct RunHooks {
    /// Fires once, right after the daemon is online (subscribed + past the NIP-42 auth wait) —
    /// i.e. the point at which it will react to LIVE offers. The test owns the receiver and uses
    /// it to assert readiness instead of scraping stderr.
    pub ready: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// Override the NIP-42 auth wait. `None` = production default (20s). A challenge-on-connect
    /// relay authenticates in milliseconds regardless of this value.
    pub auth_wait: Option<std::time::Duration>,
    /// Fires after each PERIODIC wrap-backfill run with the number of stored kind-1059(s) the
    /// fetch returned. The integration test uses it to prove the periodic timer fires and re-runs
    /// the stored-wrap backfill (test seam, not a stderr scrape). `None` in production.
    pub wrap_backfill_done: Option<tokio::sync::mpsc::UnboundedSender<usize>>,
}

/// Long-running seller loop: NIP-42 AUTH, then subscribe offers+1059 from START.
pub async fn run_forever(daemon: SellerDaemon) -> Result<(), DaemonError> {
    run_forever_hooked(daemon, RunHooks::default()).await
}

/// What the event loop does when `notifications.recv()` returns an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecvControl {
    /// Stay alive and keep processing — a transient lag must NEVER permanently deafen the seller.
    Continue,
    /// The channel is closed (pool shut down) — end the loop.
    Stop,
}

/// Classify a broadcast `recv()` error. A `Lagged` (the loop fell behind while blocked in a long
/// agent turn) is RECOVERABLE: tokio drops the overflowed messages but keeps delivering new ones,
/// so the seller continues rather than going silently deaf to all further offers AND kind-1059
/// payments (the money-path regression: wraps parked, never collected). Only `Closed` stops it.
fn classify_recv_error(error: &tokio::sync::broadcast::error::RecvError) -> RecvControl {
    match error {
        tokio::sync::broadcast::error::RecvError::Lagged(_) => RecvControl::Continue,
        tokio::sync::broadcast::error::RecvError::Closed => RecvControl::Stop,
    }
}

/// Effective periodic wrap-backfill cadence (seconds): the [`WRAP_BACKFILL_INTERVAL_ENV`] test
/// seam wins over the [`WRAP_BACKFILL_INTERVAL_SECS`] constant; a `0` or unparseable value is
/// ignored (falls back to the constant). This is the extracted cadence DECISION function so the
/// timing is unit-testable without a live relay (no production config path sets the env).
fn resolve_wrap_backfill_interval_secs() -> u64 {
    match std::env::var(WRAP_BACKFILL_INTERVAL_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => secs,
            _ => WRAP_BACKFILL_INTERVAL_SECS,
        },
        Err(_) => WRAP_BACKFILL_INTERVAL_SECS,
    }
}

/// Fetch stored kind-1059 payment gift-wraps p-tagged to this seller since the last journaled
/// receipt and run EACH through the SAME idempotent redeem path as the live subscription
/// ([`ingest_gift_wrap`] → `try_apply_or_buffer`: already-receipted wraps skip, already-redeemed
/// refuse at the mint, unverifiable buffer — all existing money guards, unchanged).
///
/// Shared by the #57 BOOT backfill (recovery on restart) and the #57 PERIODIC backfill (recovery
/// for a RUNNING daemon whose aged relay subscription silently stopped delivering wraps). The
/// `since` cursor and filter are identical on both paths; a fresh short-lived REQ (`fetch_events`)
/// is what recovers the aged-subscription case. Timeout-bounded + log-and-continue: a slow or
/// failing relay can NEVER wedge the caller. `source` is a log marker ("" boot, " (periodic)"
/// periodic); `authed_note` is an optional diagnostic suffix on the fetch-attempt line. Returns
/// the number of stored wraps the fetch returned (0 on error/timeout) so callers can surface it.
async fn run_wrap_backfill(
    daemon: &mut SellerDaemon,
    client: &nostr_sdk::Client,
    wrap_filter: nostr_sdk::Filter,
    source: &str,
    authed_note: &str,
) -> usize {
    let backfill_since = daemon.journal.last_receipt_ts().unwrap_or(None).unwrap_or(0);
    // Log the ATTEMPT unconditionally, BEFORE the fetch — silence must never read as "no wraps".
    eprintln!(
        "seller wrap backfill{source}: fetching stored kind-1059(s) since ts={backfill_since}{authed_note}"
    );
    // Hard-cap the fetch so an auth-gated relay that never EOSEs cannot wedge the caller.
    match tokio::time::timeout(
        Duration::from_secs(15),
        client.fetch_events(
            wrap_filter.since(nostr_sdk::Timestamp::from(backfill_since)),
            Duration::from_secs(12),
        ),
    )
    .await
    {
        Ok(Ok(events)) => {
            let count = events.len();
            eprintln!(
                "seller wrap backfill{source}: {count} stored kind-1059(s) returned since ts={backfill_since}"
            );
            for event in events {
                match ingest_gift_wrap(daemon, &event).await {
                    Ok(Some(receipt)) => eprintln!(
                        "seller receipt (backfill{source}) job_id={} result_id={} amount_received={}",
                        receipt.job_id, receipt.result_id, receipt.amount_received
                    ),
                    Ok(None) => {}
                    Err(error) if is_idempotent_already_redeemed(&error) => eprintln!(
                        "seller pay: kind-1059 already redeemed (backfill{source} idempotent re-see): {error}"
                    ),
                    Err(error) => eprintln!("seller pay path (backfill{source}): {error}"),
                }
            }
            count
        }
        Ok(Err(error)) => {
            eprintln!(
                "seller WARN: wrap backfill{source} fetch failed (continuing; live 1059 subscription active): {error}"
            );
            0
        }
        Err(_) => {
            eprintln!(
                "seller WARN: wrap backfill{source} fetch timed out after 15s (continuing; live 1059 subscription active)"
            );
            0
        }
    }
}

/// [`run_forever`] with test/observability hooks (see [`RunHooks`]).
pub(crate) async fn run_forever_hooked(
    mut daemon: SellerDaemon,
    hooks: RunHooks,
) -> Result<(), DaemonError> {
    use std::time::Duration;
    use nostr_sdk::prelude::{
        Client, Kind, RelayMessage, RelayPoolNotification, RelayUrl, SubscribeOptions,
        SubscriptionId, Timestamp,
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
    let auth_wait = hooks.auth_wait.unwrap_or(Duration::from_secs(20));
    let nip42_label = match wait_for_nip42_auth(&mut relay_notifications, auth_wait).await? {
        AuthWait::Authenticated => "authenticated",
        AuthWait::NoChallenge => {
            eprintln!(
                "seller WARN: relay issued no NIP-42 AUTH challenge within {auth_wait:?}; \
                 proceeding (automatic_authentication stays ON, so a relay that challenges on a \
                 REQ is still authenticated). If this relay p-gates kind-1059, seller receive may \
                 be degraded until auth completes."
            );
            "no-challenge"
        }
    };

    // Restart-reconcile: release any orphaned in-flight claims from a prior run BEFORE
    // serving new offers, so a claim left live by a crash never reads "processing" forever
    // (evidence job 0867a213). Durable via journal; feedback-kind surface is best-effort.
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

    // #57: rebuild delivered-but-unpaid jobs into awaiting_payment BEFORE the wrap subscribe +
    // backfill, so a stored/buffered payment wrap can bind and redeem on this boot (the missing leg
    // between "wrap seen" and "collect ok"). Non-fatal on error.
    match daemon.restore_delivered_unpaid() {
        Ok(n) if n > 0 => {
            eprintln!("seller reconcile: restored {n} delivered-but-unpaid job(s) into awaiting_payment")
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("seller WARN: failed to restore delivered-but-unpaid jobs (continuing): {error}")
        }
    }

    // Offer subscription: the TARGETED filter (p-tag == seller) AND — under open-pool — the
    // BOUNDED un-pinned filter ride ONE long-lived subscription (a single REQ, OR-matched per
    // NIP-01) via `offer_subscriptions` + `pool().subscribe`. Registered as a SEPARATE second
    // subscription (the earlier half-fix) the un-pinned filter delivered stored events (backfill)
    // but never LIVE offers, so a running open-pool seller ignored fresh untargeted offers. Group
    // them into ONE REQ (`Client::subscribe` takes a single filter — one REQ per filter is the
    // bug). The event-id dedup in the loop still processes each offer exactly once. Sub id(s) are
    // captured so the Loud-Closed fallback can detect a relay CLOSE of the offer subscription.
    let seller_pubkey = daemon.keys.public_key();
    // Create the notifications receiver BEFORE any REQ (offer subscribe, wrap subscribe, wrap
    // backfill). A tokio broadcast only delivers to receivers that already exist, so a stored event
    // returned by a REQ before this receiver is created would be dropped. This latent race widened
    // once the #57 boot backfill added a network round-trip between the offer subscribe and the
    // loop — capture the receiver up front so backfilled offers/wraps are never missed.
    let mut notifications = client.notifications();
    let (claim_open_pool, offer_backfill_secs) = require_seller_config(&daemon.home)
        .map(|cfg| (cfg.claim_open_pool, cfg.offer_backfill_secs))
        .unwrap_or((false, 0));
    // Single subscribe anchor, shared by the UNTARGETED filter's window (`since(now - window)`;
    // the targeted filter carries no time bound) and the backfill discriminator below
    // (`created_at < daemon_start_unix` ⇒ posted before we came online ⇒ subject to the
    // pre-claim money-safety check, on BOTH paths — targeted-history backfill included).
    // Captured once so the filter bound and the discriminator agree.
    let subscribe_now = Timestamp::now();
    let daemon_start_unix = subscribe_now.as_secs();
    let mut offer_sub_ids: Vec<SubscriptionId> = Vec::new();
    for filters in offer_subscriptions(
        seller_pubkey,
        claim_open_pool,
        subscribe_now,
        offer_backfill_secs,
    ) {
        let output = client
            .pool()
            .subscribe(filters, SubscribeOptions::default())
            .await
            .map_err(|error| DaemonError::Relay(format!("subscribe offers: {error}")))?;
        offer_sub_ids.push(output.val);
    }
    let wrap_filter = wrap_subscription_filter(seller_pubkey);
    client
        .subscribe(wrap_filter.clone(), None)
        .await
        .map_err(|error| DaemonError::Relay(format!("subscribe 1059: {error}")))?;


    // Status line: never echo secrets.
    eprintln!(
        "seller daemon online pubkey={} relay={} mint={} nip42={nip42_label}",
        daemon.seller_pubkey(),
        daemon.home.config.relay_url,
        daemon.home.config.default_mint()
    );
    // ONLINE announce: subscribed + past the NIP-42 auth wait ⇒ reacting to live offers.
    daemon.announce(crate::announce::AnnounceEvent::online(
        now_unix(),
        daemon.seller_pubkey(),
        &daemon.home.config.relay_url,
        daemon.home.config.default_mint(),
        nip42_label,
    ));
    // Readiness hook: online + subscribed ⇒ ready to react to LIVE offers.
    if let Some(ready) = &hooks.ready {
        let _ = ready.send(());
    }

    // #57 boot backfill (recovery) — runs AFTER online/readiness so the daemon reports up promptly
    // and the backfill can never hide behind a hang. kind-1059 is auth-gated on mobee-relay (dark
    // kind): a REQ sent before NIP-42 completes is CLOSED `restricted:` and dropped, so the stored
    // wrap is never served (the #57 live-acceptance failure). Confirm auth FIRST, then fetch stored
    // wraps p-tagged to us since the last journaled receipt and run each through the SAME redeem
    // path — idempotent via the journal pay-once guard, so it can never double-spend. A live offer
    // posted during this window is buffered in `notifications` and drained when the loop starts.
    let backfill_authed = nip42_label == "authenticated"
        || matches!(
            // If the connect-time challenge already authenticated us this returns immediately; if
            // the relay defers to the REQ, the subscribes above triggered the challenge and this
            // catches the completion. Short + non-fatal so it never wedges boot.
            wait_for_nip42_auth(&mut relay_notifications, Duration::from_secs(3)).await,
            Ok(AuthWait::Authenticated)
        );
    // Boot recovery: run the shared stored-wrap backfill once (source marker "", auth diagnostic
    // suffix). The periodic timer in the loop re-runs this SAME helper on a cadence so a running
    // daemon whose subscription later ages out still recovers without a restart.
    let _ = run_wrap_backfill(
        &mut daemon,
        &client,
        wrap_filter,
        "",
        &format!(" (nip42_authed={backfill_authed})"),
    )
    .await;

    // Boot push preflight: surface WRITE-auth/git-version breakage now, not at job-delivery time.
    // Advisory only — logs-and-continues; never refuses boot (see run_boot_push_preflight).
    run_boot_push_preflight_for_daemon(&daemon);

    // Both offer filters ride ONE subscription, so the relay delivers each offer once even when
    // a targeted offer matches both filters. Keep an event-id dedup as defense-in-depth (e.g.
    // reconnect re-delivery) so each offer id reaches `on_offer_event` at most once. Bounded to
    // the most-recent `SEEN_OFFERS_CAP` ids so it can't leak over the seller's lifetime; an
    // evicted-then-re-delivered claimed offer is still caught by the durable journal `has_claim`.
    let mut seen_offers = BoundedSeen::default();
    // Loud-Closed fallback fires at most once (targeted-only is not itself broad-filtered).
    let mut offer_fallback_done = false;
    // `notifications` was created up front (before the REQs) so no backfilled event is dropped.

    // Heartbeat cadence. Env overrides config for tests. `interval()`'s
    // first `tick()` completes immediately, so an enabled seller advertises liveness right after
    // going online, then every `interval_secs`.
    let heartbeat_enabled =
        crate::heartbeat::resolve_enabled(&daemon.home.config.seller_heartbeat);
    let heartbeat_interval_secs =
        crate::heartbeat::resolve_interval_secs(&daemon.home.config.seller_heartbeat);
    let mut heartbeat_interval =
        tokio::time::interval(Duration::from_secs(heartbeat_interval_secs.max(1)));
    if heartbeat_enabled {
        eprintln!(
            "seller heartbeat enabled: kind-30340 d=mobee-seller every {heartbeat_interval_secs}s"
        );
    }

    // #57 periodic wrap backfill: re-run the boot stored-wrap backfill every N seconds so a
    // running daemon whose relay subscription has aged out (silently stopped delivering kind-1059
    // wraps) recovers WITHOUT a restart. `interval_at` (not `interval`) starts one period out, so
    // the first periodic run does NOT double the boot backfill we just ran. It rides the SAME
    // select loop as the heartbeat (never a blocking side-thread) and each run is timeout-bounded
    // + log-and-continue, so it can never wedge or crash offer/payment handling.
    let wrap_backfill_interval_secs = resolve_wrap_backfill_interval_secs();
    let mut wrap_backfill_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(wrap_backfill_interval_secs),
        Duration::from_secs(wrap_backfill_interval_secs),
    );
    eprintln!(
        "seller wrap backfill (periodic) enabled: re-fetch stored kind-1059(s) every {wrap_backfill_interval_secs}s"
    );

    loop {
        // Resilient recv: a broadcast LAG (the loop fell behind while blocked in a long agent
        // turn) must NOT permanently deafen the daemon. tokio's broadcast drops the overflowed
        // messages and keeps delivering NEW ones, so we LOG and CONTINUE; only a genuinely closed
        // channel ends the loop. Before this a `Lagged` ended `while let Ok(..)` — the seller went
        // silently deaf to all further offers AND kind-1059 payments (wraps parked, never
        // collected) until restart. Missed stored events re-backfill on resubscribe/restart.
        let notification = tokio::select! {
            // The heartbeat tick rides the SAME loop (never a blocking side-thread): publishing is
            // timeout-bounded + log-and-continue, so it can NEVER wedge or crash offer/payment
            // handling. Disabled ⇒ the branch is inert and select only waits on the relay stream.
            _ = heartbeat_interval.tick(), if heartbeat_enabled => {
                publish_heartbeat(&daemon).await;
                continue;
            }
            // Periodic wrap backfill tick (#57). Same idempotent redeem path as boot; a fresh REQ
            // recovers an aged, silently-deaf subscription. Timeout-bounded inside the helper.
            _ = wrap_backfill_interval.tick() => {
                let count = run_wrap_backfill(
                    &mut daemon,
                    &client,
                    wrap_subscription_filter(seller_pubkey),
                    " (periodic)",
                    "",
                )
                .await;
                if let Some(tx) = &hooks.wrap_backfill_done {
                    let _ = tx.send(count);
                }
                continue;
            }
            recv = notifications.recv() => match recv {
                Ok(notification) => notification,
                Err(error) => match classify_recv_error(&error) {
                    RecvControl::Continue => {
                        eprintln!(
                            "seller WARN: notification stream {error}; continuing (NOT going deaf). \
                             Missed stored offers/payments re-backfill on resubscribe."
                        );
                        continue;
                    }
                    RecvControl::Stop => break,
                },
            },
        };
        match notification {
            RelayPoolNotification::Event { event, .. } => {
                if event.kind == Kind::GiftWrap {
                    match ingest_gift_wrap(&mut daemon, &event).await {
                        Ok(Some(receipt)) => {
                            eprintln!(
                                "seller receipt job_id={} result_id={} amount_received={}",
                                receipt.job_id, receipt.result_id, receipt.amount_received
                            );
                            // Delivered-PAID ⇒ best-effort retro. Detached (own thread);
                            // returns immediately so wrap collection is never blocked.
                            daemon.maybe_run_retro(&receipt.job_id);
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
                // An offer (offer-kind) routes to `on_offer_event` REGARDLESS of p-tag — open-pool
                // offers carry none, so a p-tag gate here would silently drop them. Deduped by
                // event id so each offer is processed once (see `offer_event_should_process`).
                if offer_event_should_process(event.kind.as_u16(), event.id, &mut seen_offers) {
                    // BACKFILL money-safety pre-claim check (guards #c/#d), for offers posted
                    // BEFORE we came online (`created_at < daemon_start_unix`). A live offer
                    // (posted while we run) keeps the byte-identical fast path — no relay query.
                    // This gates on backfilled-vs-live, NOT on window size, so it holds for ANY
                    // `offer_backfill_secs`. Fail-closed: an inconclusive relay read SKIPS. Every
                    // outcome is logged with a reason (never a silent drop).
                    if event.created_at.as_secs() < daemon_start_unix {
                        if let Some(reason) = daemon.backfill_offer_blocked(&event).await {
                            eprintln!("seller skip offer {}: {reason}", event.id.to_hex());
                            continue;
                        }
                    }
                    match daemon.on_offer_event(&event).await {
                        Ok(Some(_)) => {
                            match daemon.execute_active_job().await {
                                Ok(result_id) => {
                                    eprintln!("seller published result_id={result_id}");
                                    // DELIVERED: free the single-flight slot so new offers can
                                    // be claimed while this job awaits payment (#15).
                                    daemon.mark_delivered();
                                    match reconcile_after_result(&mut daemon).await {
                                        Ok(Some(receipt)) => {
                                            eprintln!(
                                                "seller receipt (reconcile) job_id={} amount_received={}",
                                                receipt.job_id, receipt.amount_received
                                            );
                                            // Delivered-PAID ⇒ detached best-effort retro.
                                            daemon.maybe_run_retro(&receipt.job_id);
                                        }
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
            // Loud-Closed fallback (hardening #3): the relay CLOSED a subscription. If it's the
            // grouped OFFER subscription (e.g. the relay restricts the broad open-pool filter),
            // the seller would go SILENTLY deaf to offers. LOG IT LOUDLY and degrade to the
            // TARGETED-only offer filter (`#p==self`) — which a relay that only objects to the
            // broad filter still accepts — so the seller stays targeted-alive with a visible log
            // instead of dying quietly. nostr-sdk removes a CLOSED (error/restricted/blocked) sub
            // and does not re-subscribe it, so this re-subscribe is the recovery path.
            RelayPoolNotification::Message {
                message: RelayMessage::Closed { subscription_id, message },
                ..
            } => {
                let is_offer_sub = offer_sub_ids.iter().any(|id| id == subscription_id.as_ref());
                if is_offer_sub && !offer_fallback_done {
                    offer_fallback_done = true;
                    eprintln!(
                        "seller WARN: relay CLOSED the offer subscription (sub_id={}): \"{}\". \
                         Falling back to the TARGETED-only offer filter (#p==self); open-pool \
                         (untargeted) offers will NOT be received until the relay accepts the \
                         grouped subscription again.",
                        subscription_id.as_ref(),
                        message
                    );
                    // Re-subscribe targeted-only. The targeted filter carries no time bound
                    // (original full-backfill shape at all window values), so stored targeted
                    // offers — including any posted while the grouped subscription was down —
                    // are re-delivered; the window/anchor args are passed for signature
                    // uniformity and unused on this path (`claim_open_pool = false`).
                    match client
                        .pool()
                        .subscribe(
                            offer_subscription_filters(
                                seller_pubkey,
                                false,
                                Timestamp::now(),
                                offer_backfill_secs,
                            ),
                            SubscribeOptions::default(),
                        )
                        .await
                    {
                        Ok(output) => offer_sub_ids.push(output.val),
                        Err(error) => eprintln!(
                            "seller ERROR: targeted-only fallback subscribe failed: {error}"
                        ),
                    }
                }
            }
            RelayPoolNotification::Shutdown => break,
            _ => {}
        }
    }
    Ok(())
}

/// Serialises tests that touch the process-global single-flight lock [`FLIGHT`]: the local-relay
/// daemon integration tests (which claim offers, taking `FLIGHT`) and `single_flight_mutex`.
/// Without this they race on `FLIGHT` when cargo runs tests concurrently. Poison-tolerant at the
/// lock sites (a panicking test must not wedge the others).
#[cfg(test)]
static FLIGHT_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    /// #57 TEST (1) — periodic backfill CADENCE decision function. The interval is a fixed
    /// constant in production (300s); the env seam (used only by tests) overrides it, and a
    /// `0`/unparseable value is ignored. This is the extracted `resolve_wrap_backfill_interval_secs`
    /// that lets the cadence be exercised without a live relay. Serialised against the daemon
    /// integration tests (which also read this env) via `FLIGHT_TEST_GUARD`.
    #[test]
    fn wrap_backfill_interval_env_overrides_default() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // No env seam ⇒ the fixed production constant (5 min).
        unsafe {
            std::env::remove_var(WRAP_BACKFILL_INTERVAL_ENV);
        }
        assert_eq!(WRAP_BACKFILL_INTERVAL_SECS, 300);
        assert_eq!(
            resolve_wrap_backfill_interval_secs(),
            WRAP_BACKFILL_INTERVAL_SECS
        );
        // A valid positive override wins (the fast-cadence test seam).
        unsafe {
            std::env::set_var(WRAP_BACKFILL_INTERVAL_ENV, "2");
        }
        assert_eq!(resolve_wrap_backfill_interval_secs(), 2);
        // `0` and unparseable values are ignored ⇒ fall back to the constant.
        unsafe {
            std::env::set_var(WRAP_BACKFILL_INTERVAL_ENV, "0");
        }
        assert_eq!(
            resolve_wrap_backfill_interval_secs(),
            WRAP_BACKFILL_INTERVAL_SECS
        );
        unsafe {
            std::env::set_var(WRAP_BACKFILL_INTERVAL_ENV, "nonsense");
        }
        assert_eq!(
            resolve_wrap_backfill_interval_secs(),
            WRAP_BACKFILL_INTERVAL_SECS
        );
        unsafe {
            std::env::remove_var(WRAP_BACKFILL_INTERVAL_ENV);
        }
    }

    // The seller-side receipt-preimage delivery discriminator is DERIVED from the typed
    // `GitDelivery` ("fork"), not a hardcoded label — buyer and seller agree by construction.
    #[test]
    fn seller_delivery_kind_derives_fork_from_typed_delivery() {
        let kind = seller_delivery_kind(
            "https://relay.example/git/job.git",
            "mobee/abcd1234",
            &"a".repeat(40),
        )
        .expect("commit delivery types");
        assert_eq!(kind, crate::receipt::DeliveryKind::Fork);
        assert_eq!(kind.as_str(), "fork");
    }

    #[test]
    fn open_refuses_non_testnut_mint() {
        let root = temp("mint");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home.config.accepted_mints = vec!["https://real-mint.example".into()];
        home.config.seller = Some(SellerConfig {
            agent_command: vec!["echo".into()],
            rate_sats: 1,
            git_remote: "https://example.invalid/repo.git".into(),
            job_timeout_secs: None,
            agent: None,
            claim_open_pool: false,
            offer_backfill_secs: 0,
            contribution_enabled: true,
        });
        home::save_config(&home).expect("save");
        let err = match SellerDaemon::open(home) {
            Ok(_) => panic!("non-testnut must fail-closed"),
            Err(error) => error,
        };
        assert!(err.to_string().contains("fail-closed") || err.to_string().contains("testnut"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Issue #49 real-mint switch: with `allow_real_mints=true` the boot fence admits a real
    // (non-testnut) accepted_mints entry that is refused by default.
    #[test]
    fn open_admits_real_mint_when_allow_real_mints_true() {
        let root = temp("mint-real");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home.config.accepted_mints = vec!["https://minibits.example".into()];
        home.config.allow_real_mints = true;
        home.config.seller = Some(SellerConfig {
            agent_command: vec!["echo".into()],
            rate_sats: 1,
            git_remote: "https://example.invalid/repo.git".into(),
            job_timeout_secs: None,
            agent: None,
            claim_open_pool: false,
            offer_backfill_secs: 0,
            contribution_enabled: true,
        });
        home::save_config(&home).expect("save");
        SellerDaemon::open(home).expect("real mint admitted with allow_real_mints=true");
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
    fn boot_preflight_enabled_defaults_on_and_env_disables() {
        let mut config = crate::home::MobeeConfig::default();
        assert!(boot_preflight_enabled(&config, None), "default must be on");
        for off in ["0", "false", "no", "OFF", " false "] {
            assert!(
                !boot_preflight_enabled(&config, Some(off)),
                "env {off:?} must disable"
            );
        }
        assert!(
            boot_preflight_enabled(&config, Some("1")),
            "a non-disabling env value keeps it on"
        );
        config.seller_preflight.boot_push_preflight = false;
        assert!(
            !boot_preflight_enabled(&config, None),
            "config-off wins with no env"
        );
        assert!(
            !boot_preflight_enabled(&config, Some("1")),
            "env cannot force a config-off preflight on"
        );
    }

    #[test]
    fn boot_preflight_failure_logs_and_continues() {
        // Mock the probe seam: a FAILING probe must still yield a (Some) advisory line naming the
        // git-version cause + fix — never an error, never a panic. The daemon logs it and keeps
        // running (this fn returning Some, not the probe's Err, IS the logs-and-continues contract).
        let line = run_boot_push_preflight(true, || {
            Err(crate::seller_git::SellerGitError::AuthFailed("mock 401".into()))
        })
        .expect("enabled preflight must yield a log line");
        assert!(line.contains("FAILED"), "{line}");
        assert!(line.contains("2.54"), "{line}");
        assert!(line.to_lowercase().contains("continuing"), "{line}");
    }

    #[test]
    fn boot_preflight_success_reports_ok() {
        let line =
            run_boot_push_preflight(true, || Ok(())).expect("enabled preflight yields a line");
        assert!(line.contains("OK"), "{line}");
    }

    #[test]
    fn boot_preflight_disabled_skips_probe_seam() {
        let mut probe_ran = false;
        let out = run_boot_push_preflight(false, || {
            probe_ran = true;
            Ok(())
        });
        assert!(out.is_none(), "disabled preflight must produce no line");
        assert!(!probe_ran, "disabled preflight must not invoke the probe seam");
    }

    #[test]
    fn single_flight_mutex() {
        // Serialise against the daemon integration tests: both touch the global FLIGHT lock.
        let _guard = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
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

    // Item #16(c): a transient agent error is retried WITHIN the deadline; feedback-kind is
    // published only after the attempt budget or the deadline is spent.
    #[tokio::test]
    async fn retry_recovers_from_a_transient_error_within_the_deadline() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        // Deadline far away ⇒ never the limiter; a transient first error must be retried,
        // NOT burn the claim (publish feedback) while the deadline still has room.
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
        assert!(out.is_err(), "exhausted retries ⇒ error so caller publishes feedback-kind");
        assert_eq!(attempts.get(), 3, "bounded to the attempt budget");
    }

    #[tokio::test]
    async fn retry_past_deadline_makes_one_attempt_then_surfaces_the_error() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        // `now` (5_000) is already past the deadline (1_000) ⇒ no retry budget at all: one
        // attempt, then the error surfaces so the caller publishes feedback-kind.
        let out = run_agent_with_retry(1_000, 3, || 5_000, |attempt| {
            attempts.set(attempt);
            async move {
                Err::<Option<UsageMetadata>, DaemonError>(DaemonError::Agent("late".into()))
            }
        })
        .await;
        assert!(out.is_err(), "past deadline ⇒ error (caller publishes feedback-kind)");
        assert_eq!(attempts.get(), 1, "no retry once the deadline has passed");
    }

    // Item #16(e): the daemon appends explicit, secret-free delivery instructions.
    #[test]
    fn composed_prompt_carries_task_and_daemon_owned_delivery_instructions() {
        let remote = "https://relay.example/git/abc.git";
        let prompt = compose_agent_prompt("build a widget", remote, None);
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
    fn custom_preset_label_is_the_reported_harness_identity() {
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        // A config-defined `[agents]` preset (non-built-in label): the preset name IS the
        // harness id — never argv0, never a family guess from the launch command.
        let argv = vec!["/opt/adapters/grok-acp".to_string(), "stdio".to_string()];
        let tags = seller_exec_metadata(&argv, Some("grok"), 42, None);
        assert_eq!(value(&tags, "harness").as_deref(), Some("grok"));
        assert_eq!(value(&tags, "usage_transport").as_deref(), Some("side-channel"));

        // Built-in labels keep their adapter identities (custom seam must not regress them).
        let builtin = seller_exec_metadata(&argv, Some("codex"), 42, None);
        assert_eq!(value(&builtin, "harness").as_deref(), Some("codex-acp-ng"));
    }

    #[test]
    fn open_pool_filter_lands_fresh_untargeted_offer_only_when_enabled_and_bounds_history() {
        use nostr_sdk::prelude::{
            EventBuilder, Filter, Keys, Kind, MatchEventOptions, Tag, Timestamp,
        };

        let seller = Keys::generate();
        let buyer = Keys::generate();

        // A TARGETED offer carries a p-tag == seller; an UNtargeted (open-pool) offer has none.
        let targeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .tag(Tag::public_key(seller.public_key()))
            .sign_with_keys(&buyer)
            .expect("sign targeted offer");
        // A FRESH untargeted offer (dated after the filter's `since(now)`). Built in the future
        // so it deterministically clears the bound regardless of sub-second test timing.
        let fresh_untargeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .custom_created_at(Timestamp::now() + 60u64)
            .sign_with_keys(&buyer)
            .expect("sign fresh untargeted offer");
        // A STALE untargeted offer (an hour old) — the relay's offer history.
        let stale_untargeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .custom_created_at(Timestamp::from(Timestamp::now().as_secs().saturating_sub(3600)))
            .sign_with_keys(&buyer)
            .expect("sign stale untargeted offer");

        let matches_any = |filters: &[Filter], event: &nostr_sdk::Event| {
            filters
                .iter()
                .any(|filter| filter.match_event(event, MatchEventOptions::new()))
        };

        // A 300s backfill window: comfortably covers the `now`/`now+60` offers, excludes the
        // hour-old one. (`match_event` only consults `since`, not `limit`.)
        let window = 300u64;

        // Targeted-only (claim_open_pool = false): the targeted offer matches, the untargeted
        // (open-pool) offer does NOT — this is exactly why --claim-open-pool was DOA.
        let targeted_only =
            offer_subscription_filters(seller.public_key(), false, Timestamp::now(), window);
        assert!(
            matches_any(&targeted_only, &targeted),
            "targeted offer must match the pinned filter"
        );
        assert!(
            !matches_any(&targeted_only, &fresh_untargeted),
            "untargeted offer must NOT match without open-pool"
        );

        // Open-pool (claim_open_pool = true): the 2nd un-pinned filter lands the FRESH untargeted
        // offer. RED-ON-REVERT: drop the `filters.push(...)` in offer_subscription_filters and
        // this assert fails — the untargeted offer no longer matches, so no claim fires.
        let open_pool =
            offer_subscription_filters(seller.public_key(), true, Timestamp::now(), window);
        assert!(
            matches_any(&open_pool, &targeted),
            "targeted offer still matches under open-pool"
        );
        assert!(
            matches_any(&open_pool, &fresh_untargeted),
            "fresh untargeted offer MUST match under open-pool (the fix)"
        );
        // The un-pinned filter is BOUNDED (`since(now - window)`): an offer OLDER than the window
        // does NOT match, so a running seller never replays the relay's full offer history.
        // RED-ON-REVERT: drop the `.since(..)` bound and this assert fails (the flood returns).
        assert!(
            !matches_any(&open_pool, &stale_untargeted),
            "an untargeted offer older than the backfill window MUST NOT match the bounded filter"
        );
    }

    // #57 regression: the seller's kind-1059 payment filter must match a gift-wrap that carries NO
    // t=mobee tag. NIP-59 wraps are opaque and cannot carry a namespace tag; an A′-style hashtag
    // filter here would return zero wraps and silently strand real payments.
    #[test]
    fn wrap_filter_matches_untagged_gift_wrap() {
        use nostr_sdk::prelude::{EventBuilder, Keys, Kind, MatchEventOptions, Tag};

        let seller = Keys::generate();
        let sender = Keys::generate();
        let filter = wrap_subscription_filter(seller.public_key());

        // A 1059 wrap p-tagged to the seller with NO t=mobee tag (as real gift-wraps are).
        let wrap = EventBuilder::new(Kind::GiftWrap, "opaque")
            .tag(Tag::public_key(seller.public_key()))
            .sign_with_keys(&sender)
            .expect("sign wrap");
        assert!(
            filter.match_event(&wrap, MatchEventOptions::new()),
            "seller 1059 filter must match a p-tagged wrap that has no t=mobee tag (#57)"
        );

        // A wrap p-tagged to someone else must NOT match.
        let other = EventBuilder::new(Kind::GiftWrap, "opaque")
            .tag(Tag::public_key(Keys::generate().public_key()))
            .sign_with_keys(&sender)
            .expect("sign wrap");
        assert!(!filter.match_event(&other, MatchEventOptions::new()));
    }

    #[test]
    fn open_pool_offers_ride_one_grouped_subscription() {
        use nostr_sdk::prelude::{
            EventBuilder, Filter, Keys, Kind, MatchEventOptions, Tag, Timestamp,
        };

        let seller = Keys::generate();
        let buyer = Keys::generate();

        let targeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .tag(Tag::public_key(seller.public_key()))
            .sign_with_keys(&buyer)
            .expect("sign targeted offer");
        let fresh_untargeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .custom_created_at(Timestamp::now() + 60u64)
            .sign_with_keys(&buyer)
            .expect("sign fresh untargeted offer");

        let matches_any = |filters: &[Filter], event: &nostr_sdk::Event| {
            filters
                .iter()
                .any(|filter| filter.match_event(event, MatchEventOptions::new()))
        };

        // Open-pool: the offer filters are registered as EXACTLY ONE live subscription (a single
        // REQ) carrying BOTH the pinned and the (bounded) un-pinned filter, so the un-pinned
        // filter rides the same long-lived subscription and streams LIVE offers. The half-fix
        // registered the un-pinned filter as a SEPARATE second subscription (two subscriptions),
        // which the relay dropped after backfill. RED-ON-REVERT: return one subscription per
        // filter (as the half-fix did) and this `len() == 1` assertion fails.
        let subs = offer_subscriptions(seller.public_key(), true, Timestamp::now(), 300);
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
            matches_any(live, &fresh_untargeted),
            "the single live subscription must ALSO match fresh untargeted (open-pool) offers"
        );

        // Targeted-only: still one subscription; matches targeted, not untargeted.
        let subs = offer_subscriptions(seller.public_key(), false, Timestamp::now(), 300);
        assert_eq!(subs.len(), 1, "one offer subscription when not open-pool");
        assert!(matches_any(&subs[0], &targeted));
        assert!(
            !matches_any(&subs[0], &fresh_untargeted),
            "untargeted offers must not match without open-pool"
        );
    }

    #[test]
    fn untargeted_offer_routes_to_on_offer_event_and_dedups() {
        use nostr_sdk::prelude::{EventBuilder, Keys, Kind};

        let buyer = Keys::generate();
        // An UNtargeted (open-pool) offer carries no p-tag.
        let untargeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .sign_with_keys(&buyer)
            .expect("sign untargeted offer");

        let mut seen = BoundedSeen::default();

        // A non-p-tagged offer MUST route to `on_offer_event` on the LIVE push — this rules
        // out the "notification path drops non-p-tagged offer" failure mode. RED-ON-REVERT: gate
        // routing on a p-tag and this first assertion fails for the untargeted offer.
        assert!(
            offer_event_should_process(untargeted.kind.as_u16(), untargeted.id, &mut seen),
            "an untargeted offer must route to on_offer_event"
        );
        // Dedup by event id: a re-delivered offer id is processed at most once.
        assert!(
            !offer_event_should_process(untargeted.kind.as_u16(), untargeted.id, &mut seen),
            "a re-delivered offer id must be deduped, not double-processed"
        );
        // A non-offer kind (e.g. gift-wrap 1059) does not route as an offer.
        assert!(
            !offer_event_should_process(Kind::GiftWrap.as_u16(), untargeted.id, &mut seen),
            "non-offer events must not route to on_offer_event"
        );
    }

    /// A distinct, deterministic `EventId` per index — a zero-padded 64-char hex (32 bytes).
    /// Avoids signing thousands of real events just to fill the bounded set in the cap test.
    fn eid(i: usize) -> nostr_sdk::EventId {
        nostr_sdk::EventId::from_hex(&format!("{i:064x}")).expect("valid 32-byte event-id hex")
    }

    #[test]
    fn bounded_seen_caps_and_forgets_oldest() {
        // (a) CAP + eviction. Insert CAP+K distinct ids; the set must retain at most CAP, and
        // the OLDEST K (the first inserted) must be FORGOTTEN — a forgotten id re-inserts as
        // NEW (true) again. RED-ON-REVERT: drop the cap/eviction (plain HashSet) and the set
        // grows to CAP+K (the `<= CAP` assert fails) AND forgotten ids read as already-seen
        // (the "re-inserts as NEW" assert flips to false).
        const K: usize = 5;
        let mut seen = BoundedSeen::default();
        for i in 0..(SEEN_OFFERS_CAP + K) {
            assert!(
                seen.insert(eid(i)),
                "each of the first CAP+K distinct ids is new on first sight: {i}"
            );
        }
        assert!(
            seen.len() <= SEEN_OFFERS_CAP,
            "bounded set must never exceed the cap (held {})",
            seen.len()
        );
        assert_eq!(
            seen.len(),
            SEEN_OFFERS_CAP,
            "after CAP+K inserts the set holds exactly CAP (oldest K evicted)"
        );
        // The oldest K ids (0..K) were evicted → each re-inserts as NEW.
        for i in 0..K {
            assert!(
                seen.insert(eid(i)),
                "a forgotten (oldest, evicted) id must re-insert as NEW: {i}"
            );
        }
    }

    #[test]
    fn bounded_seen_dedups_recent_id() {
        // (b) window dedup. A recently-inserted id is "already seen": first insert is NEW
        // (true), a second insert within the retained window returns false (skip) — the
        // semantic `offer_event_should_process` relies on for filter-overlap / re-delivery.
        let mut seen = BoundedSeen::default();
        let id = eid(42);
        assert!(seen.insert(id), "first sight of an id is NEW");
        assert!(!seen.insert(id), "a recently-seen id is already seen (deduped)");
        // Interleave other ids while staying well under the cap — the recent id is retained.
        for i in 100..200 {
            seen.insert(eid(i));
        }
        assert!(
            !seen.insert(id),
            "an id still within the retained window stays already-seen"
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
            contribution: None,
            delivery: None,
        }
    }

    fn test_daemon(label: &str) -> (PathBuf, SellerDaemon) {
        let root = temp(label);
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home.config.accepted_mints = vec![DEFAULT_MINT_URL.into()];
        home.config.seller = Some(SellerConfig {
            agent_command: vec!["echo".into()],
            rate_sats: 1,
            git_remote: "https://example.invalid/repo.git".into(),
            job_timeout_secs: None,
            agent: None,
            claim_open_pool: false,
            offer_backfill_secs: 0,
            contribution_enabled: true,
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
            seller_pubkey,
        );
        let draft = offer.to_event_draft();
        let builder = gateway::nostr::event_builder(&draft).expect("event builder");
        builder.sign_with_keys(buyer).expect("sign offer")
    }

    fn contribution_offer_event(
        buyer: &nostr_sdk::Keys,
        seller_pubkey: &str,
        deadline: u64,
        extra_tags: Vec<TagSpec>,
    ) -> nostr_sdk::Event {
        let offer =
            crate::gateway::OfferDraft::new("do a task", "text/plain", 5, deadline, seller_pubkey);
        let mut draft = offer.to_event_draft();
        draft.tags.extend(extra_tags);
        let builder = gateway::nostr::event_builder(&draft).expect("event builder");
        builder.sign_with_keys(buyer).expect("sign offer")
    }

    fn sample_contribution_tags() -> Vec<TagSpec> {
        let c = ContributionOffer {
            target: crate::contribution::TargetRepoPin::new(
                "aa".repeat(32),
                "https://mobee-relay.orveth.dev/git/owner/repo.git",
            )
            .unwrap(),
            base: crate::contribution::ContributionBase::new("main", "77".repeat(20)).unwrap(),
            accepts: vec!["fork".into()],
        };
        crate::contribution::contribution_offer_tags(&c)
    }

    // The daemon RECOGNISES a contribution offer, threads the pins into the claim intent.
    #[test]
    fn classify_admits_contribution_offer_and_threads_pins() {
        let (root, daemon) = test_daemon("contrib-admit");
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;
        let ev = contribution_offer_event(&buyer, &seller_pk, now + 3600, sample_contribution_tags());
        match daemon.classify_offer(&ev, now).expect("classify") {
            OfferDisposition::Claim(intent) => {
                let c = intent.contribution.expect("contribution threaded into claim intent");
                assert_eq!(c.target.owner_pubkey(), "aa".repeat(32));
                assert_eq!(c.base.oid(), "77".repeat(20));
                assert!(c.accepts_fork());
            }
            other => panic!("must admit a contribution offer, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // A seller with contribution support DISABLED refuses (interop feedback-kind skip).
    #[test]
    fn classify_refuses_contribution_when_disabled() {
        let (root, mut daemon) = test_daemon("contrib-disabled");
        if let Some(seller) = daemon.home.config.seller.as_mut() {
            seller.contribution_enabled = false;
        }
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;
        let ev = contribution_offer_event(&buyer, &seller_pk, now + 3600, sample_contribution_tags());
        assert!(matches!(
            daemon.classify_offer(&ev, now).expect("classify"),
            OfferDisposition::Skip(OfferSkip::ContributionUnsupported)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    // A malformed contribution offer is REFUSED (fail-closed) — never run from-scratch.
    #[test]
    fn classify_refuses_malformed_contribution_offer() {
        let (root, daemon) = test_daemon("contrib-malformed");
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;
        // job-class=contribution but a broken base oid.
        let bad = vec![
            TagSpec::new([crate::contribution::TAG_JOB_CLASS, crate::contribution::JOB_CLASS_CONTRIBUTION]),
            TagSpec::new([crate::contribution::TAG_TARGET_REPO, &"aa".repeat(32), "https://x/git/o/r.git"]),
            TagSpec::new([crate::contribution::TAG_BASE, "main", "not-an-oid"]),
            TagSpec::new([crate::contribution::TAG_ACCEPTS, "fork"]),
        ];
        let ev = contribution_offer_event(&buyer, &seller_pk, now + 3600, bad);
        assert!(matches!(
            daemon.classify_offer(&ev, now).expect("classify"),
            OfferDisposition::Skip(OfferSkip::ContributionMalformed { .. })
        ));
        let _ = std::fs::remove_dir_all(&root);
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

    // Money-safety guard #a (PURE): an offer whose OWN deadline already passed is REFUSED, so a
    // (backfilled) offer can never be resurrected with a fresh `now + timeout` deadline. Injected
    // `now` keeps it deterministic — no wall clock.
    #[test]
    fn classify_refuses_offer_past_its_own_deadline() {
        let (root, daemon) = test_daemon("expired");
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;

        // Deadline in the PAST ⇒ DeadlineExpired skip with a named, non-empty reason.
        let expired = offer_event(&buyer, &seller_pk, 5, now - 1);
        match daemon.classify_offer(&expired, now).expect("classify expired") {
            OfferDisposition::Skip(skip) => {
                assert!(matches!(skip, OfferSkip::DeadlineExpired { .. }), "got {skip:?}");
                let reason = skip.reason();
                assert!(
                    !reason.is_empty() && reason.contains("expired"),
                    "skip reason must name expiry (never silent): {reason}"
                );
            }
            other => panic!("an offer past its deadline must be REFUSED, got {other:?}"),
        }

        // Boundary: deadline == now is refused (`<= now`); a strictly-future deadline is admitted.
        assert!(
            matches!(
                daemon.classify_offer(&offer_event(&buyer, &seller_pk, 5, now), now).expect("at-now"),
                OfferDisposition::Skip(OfferSkip::DeadlineExpired { .. })
            ),
            "deadline == now must be refused"
        );
        assert!(
            matches!(
                daemon.classify_offer(&offer_event(&buyer, &seller_pk, 5, now + 1), now).expect("future"),
                OfferDisposition::Claim(_)
            ),
            "a strictly-future deadline must be admitted"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Targeted under-rate refusal: the feedback-kind `status=error` draft CONTENT must carry the
    // same machine-readable rate-gate reason the skip log already has (buyers distinguish
    // rate-refusal from a crash / empty-content failure).
    #[test]
    fn under_rate_error_draft_content_carries_rate_gate_reason() {
        let reason = "offer amount 3 sat below seller rate_sats 5";
        let draft = under_rate_error_draft("offer-id", "buyer-pk", "seller-pk", reason);
        assert_eq!(draft.kind, gateway::JOB_FEEDBACK_KIND);
        assert_eq!(
            draft.content, reason,
            "feedback-kind content must carry the rate-gate reason, not stay empty"
        );
        assert!(
            draft.tags.iter().any(|t| {
                t.0.first().map(String::as_str) == Some("status")
                    && t.0.get(1).map(String::as_str) == Some("error")
            }),
            "must be status=error: {:?}",
            draft.tags
        );
        assert!(
            draft.tags.iter().any(|t| {
                t.0.first().map(String::as_str) == Some("e")
                    && t.0.get(1).map(String::as_str) == Some("offer-id")
            }),
            "must e-tag the refused offer: {:?}",
            draft.tags
        );
    }

    // classify → RateGate reason is the clean policy string that under_rate_error_draft embeds.
    #[test]
    fn classify_under_rate_reason_is_plumbed_into_error_draft_content() {
        let (root, mut daemon) = test_daemon("under-rate-content");
        if let Some(seller) = daemon.home.config.seller.as_mut() {
            seller.rate_sats = 5;
        }
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;
        let ev = offer_event(&buyer, &seller_pk, 3, now + 3600);
        let reason = match daemon.classify_offer(&ev, now).expect("classify") {
            OfferDisposition::Skip(OfferSkip::RateGate { reason }) => reason,
            other => panic!("targeted under-rate must RateGate-skip, got {other:?}"),
        };
        assert_eq!(
            reason, "offer amount 3 sat below seller rate_sats 5",
            "classify must surface the machine-readable rate-gate reason"
        );
        let draft = under_rate_error_draft(
            &ev.id.to_hex(),
            &ev.pubkey.to_hex(),
            &seller_pk,
            &reason,
        );
        assert_eq!(
            draft.content, reason,
            "emitted feedback-kind content must equal the rate-gate reason"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn assert_error_content_starts_with(code: &str, content: &str) {
        assert!(!content.is_empty(), "error content must not be empty");
        assert!(
            content.starts_with(&format!("{code}: ")),
            "error content must start with {code}:, got {content:?}"
        );
    }

    #[test]
    fn seller_error_content_is_machine_readable_truncated_and_path_scrubbed() {
        let long = format!(
            "failed in /Users/seller/private/job/worktree/file.rs while using https://relay.example/git/repo.git {}",
            "x".repeat(400)
        );
        let content = seller_error_content(ErrorReasonCode::AgentRunFailed, &long);
        assert_error_content_starts_with("agent_run_failed", &content);
        let human = content
            .strip_prefix("agent_run_failed: ")
            .expect("prefix checked");
        assert!(human.chars().count() <= 300, "human part is capped: {}", human.len());
        assert!(!human.contains("/Users/seller/private"), "absolute path leaked: {human}");
        assert!(human.contains("<path>"), "path redaction marker absent: {human}");
        assert!(
            human.contains("https://relay.example/git/repo.git"),
            "URLs are public locators, not filesystem paths: {human}"
        );
    }

    #[test]
    fn agent_error_reason_code_covers_spawn_timeout_and_run_failure() {
        assert_eq!(
            agent_error_reason_code(&DaemonError::Agent("failed to spawn ACP agent: no such file".into()))
                .as_str(),
            "agent_spawn_failed"
        );
        assert_eq!(
            agent_error_reason_code(&DaemonError::Agent(
                "ACP request 3 timed out waiting for response".into()
            ))
            .as_str(),
            "agent_timeout"
        );
        assert_eq!(
            agent_error_reason_code(&DaemonError::Agent("agent terminal Failed".into())).as_str(),
            "agent_run_failed"
        );
    }

    #[test]
    fn active_job_abort_error_drafts_carry_machine_readable_content() {
        // These are the active-job fail_active publishers. Live publish itself is intentionally
        // not exercised here: publish_draft hits the relay; the regression is the draft content
        // passed to that publisher.
        let cases = [
            (
                "agent_spawn_failed",
                seller_error_content(ErrorReasonCode::AgentSpawnFailed, "failed to spawn ACP agent"),
            ),
            (
                "agent_run_failed",
                seller_error_content(ErrorReasonCode::AgentRunFailed, "agent terminal Failed"),
            ),
            (
                "agent_timeout",
                seller_error_content(ErrorReasonCode::AgentTimeout, "job deadline exceeded"),
            ),
            (
                "git_fork_failed",
                seller_error_content(ErrorReasonCode::GitForkFailed, "seller git fetch failed"),
            ),
            (
                "git_push_failed",
                seller_error_content(ErrorReasonCode::GitPushFailed, "seller git push failed"),
            ),
            (
                "internal",
                seller_error_content(ErrorReasonCode::Internal, "result publish failed"),
            ),
        ];
        for (code, content) in cases {
            let draft = error_draft("offer-id", "buyer-pk", "seller-pk", content);
            assert_error_content_starts_with(code, &draft.content);
        }
    }

    #[test]
    fn contribution_refusal_error_drafts_carry_machine_readable_content() {
        let unsupported = contribution_refusal_error_content(&OfferSkip::ContributionUnsupported);
        assert_error_content_starts_with("contribution_unsupported", &unsupported);

        let malformed = contribution_refusal_error_content(&OfferSkip::ContributionMalformed {
            reason: "bad base oid".into(),
        });
        assert_error_content_starts_with("contribution_malformed", &malformed);
    }

    #[test]
    // #57: a delivered-but-unpaid job in the journal is rebuilt into awaiting_payment on boot, so a
    // stored/buffered wrap can bind and redeem. Without this the wrap buffers forever.
    #[test]
    fn restore_delivered_unpaid_rebuilds_awaiting_payment() {
        let (root, mut daemon) = test_daemon("restore-unpaid");
        let buyer = "cc".repeat(32);
        daemon
            .journal
            .append_claim("del-job", "del-claim", &buyer, 1_000_000_000)
            .expect("claim");
        daemon
            .journal
            .append_delivery("del-job", "del-result", 15, "sat", &buyer)
            .expect("delivery");
        assert!(daemon.awaiting_payment.is_empty());

        let restored = daemon.restore_delivered_unpaid().expect("restore");
        assert_eq!(restored, 1);
        assert_eq!(daemon.awaiting_payment.len(), 1);
        let job = &daemon.awaiting_payment[0];
        assert_eq!(job.job_id, "del-job");
        assert_eq!(job.result_id.as_deref(), Some("del-result"));
        assert_eq!(job.offer.amount, 15);
        assert_eq!(job.offer.unit, "sat");
        // seller_pubkey=None so the redeem's assert_seller_matches(self) passes.
        assert!(job.offer.seller_pubkey.is_none());

        // Idempotent: a delivered job already in awaiting_payment is not duplicated.
        let again = daemon.restore_delivered_unpaid().expect("restore again");
        assert_eq!(again, 0);
        assert_eq!(daemon.awaiting_payment.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── #b: reconstruct a delivered-job bind from on-relay result+claim during wrap backfill ──
    // The recovery case `restore_delivered_unpaid` cannot cover: no local journal Delivery entry.

    const RECON_KEYSET_ID: &str = "009a1f293253e41e";

    /// A received NUT-18 payment for `job_id` paid at `mint`, sealed by `buyer`.
    fn recon_received(job_id: &str, mint: &str, buyer: &nostr_sdk::Keys) -> ReceivedPayment {
        use std::str::FromStr;

        use cashu::secret::Secret;
        use cashu::{Amount, CurrencyUnit, Id, MintUrl, Proof, SecretKey};

        let proof = Proof::new(
            Amount::from(5),
            Id::from_str(RECON_KEYSET_ID).expect("keyset id"),
            Secret::new("recon-test-secret-do-not-leak"),
            SecretKey::generate().public_key(),
        );
        ReceivedPayment {
            payload: crate::payment_send::PaymentPayload {
                seller_pubkey: String::new(),
                payload: cashu::nuts::nut18::PaymentRequestPayload {
                    id: Some(job_id.to_owned()),
                    memo: None,
                    mint: MintUrl::from_str(mint).expect("mint url"),
                    unit: CurrencyUnit::Sat,
                    proofs: vec![proof],
                },
            },
            buyer_pubkey: buyer.public_key(),
        }
    }

    /// A JobView carrying one result + one claim, each authored by the given seller, plus an offer.
    /// `claim_creq` is the seller-authored `creq` tag value (built with `build_seller_creq`).
    fn recon_job_view(
        job_id: &str,
        offer_amount: u64,
        buyer_pubkey: &str,
        result_seller: &str,
        claim_seller: &str,
        claim_creq: Option<String>,
    ) -> crate::job_lifecycle::JobView {
        use crate::job_lifecycle::{ClaimView, JobView, OfferView, ResultView};
        JobView {
            job_id: job_id.to_owned(),
            offer: Some(OfferView {
                event_id: job_id.to_owned(),
                created_at: 1_000,
                author_pubkey: buyer_pubkey.to_owned(),
                author_display_name: None,
                task: "task".into(),
                output: "text/plain".into(),
                amount_sats: offer_amount,
                deadline_unix: 2_000_000_000,
                seller_pubkey: Some(result_seller.to_owned()),
                seller_display_name: None,
                targeted: true,
                repo: None,
                branch: None,
                job_class: None,
                contribution: None,
            }),
            claims: vec![ClaimView {
                claim_id: format!("claim-{job_id}"),
                created_at: 1_100,
                seller_pubkey: claim_seller.to_owned(),
                display_name: None,
                status: "processing".into(),
                live: false,
                creq: claim_creq,
            }],
            results: vec![ResultView {
                result_id: format!("result-{job_id}"),
                created_at: 1_200,
                seller_pubkey: result_seller.to_owned(),
                display_name: None,
                job_hash: None,
                repo: None,
                branch: None,
                commit_oid: None,
                amount_sats: Some(offer_amount),
                seller_signature: None,
                contribution: None,
            }],
            live_claim_id: None,
            accepted: None,
            pending: false,
        }
    }

    // Happy path: no journal bind, but a self-authored result + claim (with a matching creq) on the
    // relay → the bind is reconstructed and lands in awaiting_payment so the normal redeem path's
    // job+result lookup succeeds (redeem fires through the SAME guards — no bypass).
    #[test]
    fn reconstruct_binds_from_relay_when_journal_lacks_delivery() {
        let (root, daemon) = test_daemon("recon-happy");
        let seller = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_id = "job-recon-happy";
        let creq = gateway::creq::build_seller_creq(
            job_id,
            15,
            "sat",
            &[DEFAULT_MINT_URL.into()],
            &seller,
        )
        .expect("build creq");
        let view = recon_job_view(job_id, 15, &buyer_hex, &seller, &seller, Some(creq));
        let received = recon_received(job_id, DEFAULT_MINT_URL, &buyer);

        assert!(daemon.awaiting_payment.is_empty());
        let job = daemon
            .reconstruct_delivered_bind(&view, &received)
            .expect("reconstruct must bind a self-authored result+claim");
        assert_eq!(job.job_id, job_id);
        assert_eq!(job.result_id.as_deref(), Some(format!("result-{job_id}").as_str()));
        assert_eq!(job.claim_id, format!("claim-{job_id}"));
        assert_eq!(job.offer.amount, 15);
        assert_eq!(job.offer.unit, "sat");
        assert_eq!(job.buyer_pubkey, buyer_hex);
        // seller_pubkey=None so the redeem's assert_seller_matches(self) passes.
        assert!(job.offer.seller_pubkey.is_none());

        // The reconstructed job binds the payment (the redeem path's job+result lookup succeeds).
        let mut daemon = daemon;
        daemon.awaiting_payment.push(job);
        assert!(daemon
            .awaiting_payment
            .iter()
            .any(|j| j.job_id == received.payload.job_id()));
        let _ = std::fs::remove_dir_all(&root);
    }

    // A result authored by ANOTHER seller for J → refuse (never bind money to a foreign delivery).
    #[test]
    fn reconstruct_refuses_foreign_authored_result() {
        let (root, daemon) = test_daemon("recon-foreign");
        let seller = daemon.seller_pubkey().to_owned();
        let foreign = nostr_sdk::Keys::generate().public_key().to_hex();
        let buyer = nostr_sdk::Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_id = "job-recon-foreign";
        // creq is well-formed and self-authored on the claim, but the RESULT is foreign.
        let creq = gateway::creq::build_seller_creq(
            job_id,
            15,
            "sat",
            &[DEFAULT_MINT_URL.into()],
            &seller,
        )
        .expect("build creq");
        let view = recon_job_view(job_id, 15, &buyer_hex, &foreign, &seller, Some(creq));
        let received = recon_received(job_id, DEFAULT_MINT_URL, &buyer);

        let err = daemon
            .reconstruct_delivered_bind(&view, &received)
            .expect_err("foreign result must refuse");
        assert!(err.contains("no self-authored result"), "reason: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // The self claim's creq lists a DIFFERENT mint than the payload realized mint (creq mismatch vs
    // payload), while config accepts the payload mint → refuse at the creq-binding guard.
    #[test]
    fn reconstruct_refuses_creq_mismatch_vs_payload() {
        let (root, daemon) = test_daemon("recon-creq-mismatch");
        let seller = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_id = "job-recon-creq";
        // creq authored over a mint the buyer did NOT pay at; payload pays at DEFAULT_MINT_URL
        // (which IS in accepted_mints), isolating the creq-vs-payload mismatch from the allow-list.
        let other_mint = "https://other-mint.example/";
        let creq =
            gateway::creq::build_seller_creq(job_id, 15, "sat", &[other_mint.into()], &seller)
                .expect("build creq");
        let view = recon_job_view(job_id, 15, &buyer_hex, &seller, &seller, Some(creq));
        let received = recon_received(job_id, DEFAULT_MINT_URL, &buyer);

        let err = daemon
            .reconstruct_delivered_bind(&view, &received)
            .expect_err("creq mismatch must refuse");
        assert!(err.contains("creq mismatch"), "reason: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // The payload realized mint is NOT in the seller's accepted_mints (and the creq lists it, so
    // this isolates the allow-list guard) → refuse; the mint allow-list is never relaxed.
    #[test]
    fn reconstruct_refuses_mint_outside_accepted_mints() {
        let (root, daemon) = test_daemon("recon-mint-unlisted");
        let seller = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_id = "job-recon-mint";
        let unlisted = "https://unlisted-mint.example/";
        // creq authors the unlisted mint (so it binds the payload), but config accepted_mints only
        // holds DEFAULT_MINT_URL → the allow-list guard refuses.
        let creq =
            gateway::creq::build_seller_creq(job_id, 15, "sat", &[unlisted.into()], &seller)
                .expect("build creq");
        let view = recon_job_view(job_id, 15, &buyer_hex, &seller, &seller, Some(creq));
        let received = recon_received(job_id, unlisted, &buyer);

        let err = daemon
            .reconstruct_delivered_bind(&view, &received)
            .expect_err("unlisted mint must refuse");
        assert!(
            err.contains("not in seller accepted_mints"),
            "reason: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reconcile_release_error_draft_carries_machine_readable_content() {
        let content = reconcile_error_content(ClaimLiveness::Expired);
        let draft = error_draft("orphan-job", "buyer-pk", "seller-pk", content);
        assert_error_content_starts_with("claim_released", &draft.content);
    }

    // window == 0 reproduces the pre-backfill subscription shape byte-identically PER FILTER:
    // targeted = ORIGINAL no-since/no-limit (full targeted-history backfill, always the
    // behavior); untargeted = `since(now) + limit(0)` (live-only, zero stored offers).
    #[test]
    fn window_zero_reproduces_live_only_filter_shape() {
        use nostr_sdk::prelude::{Keys, Timestamp};
        let seller = Keys::generate();
        let now = Timestamp::now();
        let filters = offer_subscription_filters(seller.public_key(), true, now, 0);
        assert_eq!(filters.len(), 2, "open-pool ⇒ targeted + untargeted filter");
        // Targeted: ORIGINAL shape — no time bound, no stored-offer cap.
        assert_eq!(filters[0].since, None, "window=0 targeted must carry NO since (original shape)");
        assert_eq!(filters[0].limit, None, "window=0 targeted must carry NO limit (original shape)");
        // Untargeted: since(now) + limit(0) — the byte-identical live-only shape.
        assert_eq!(filters[1].since, Some(now), "window=0 untargeted must be since(now)");
        assert_eq!(
            filters[1].limit,
            Some(0),
            "window=0 untargeted must keep limit(0) (request ZERO stored offers)"
        );
    }

    // window > 0 bounds + caps the UNTARGETED (open-pool) filter ONLY: `since(now - window)`
    // with the flood cap replacing limit(0). The TARGETED filter keeps its original
    // no-since/no-limit shape at ALL window values (the field gap was
    // open-pool; bounding the p-pinned filter would be a pure regression — the deadline-expiry
    // refusal in classify_offer is the staleness guard on both paths).
    #[test]
    fn positive_window_bounds_and_caps_untargeted_filter_only() {
        use nostr_sdk::prelude::{Keys, Timestamp};
        let seller = Keys::generate();
        let now = Timestamp::now();
        let window = 1200u64;
        let expected_since = Timestamp::from(now.as_secs() - window);
        let filters = offer_subscription_filters(seller.public_key(), true, now, window);
        assert_eq!(filters.len(), 2);
        // Targeted: shape UNCHANGED by the window knob.
        assert_eq!(filters[0].since, None, "targeted must carry NO since at any window value");
        assert_eq!(filters[0].limit, None, "targeted must carry NO limit at any window value");
        // Untargeted: since(now - window) + flood cap; limit(0) dropped.
        assert_eq!(filters[1].since, Some(expected_since), "untargeted since must be now-window");
        assert_eq!(
            filters[1].limit,
            Some(OFFER_BACKFILL_LIMIT),
            "untargeted must DROP limit(0) for the flood cap when window > 0"
        );
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

    // ── Layer-0 episode capture ────────────────────────────────────────────────────

    /// Acceptance (piece A): a delivered→paid job appends exactly ONE episode line with
    /// `outcome=delivered_paid`, populated `result_id`/`commit_oid`/`amount_received` and a
    /// `transcript_ref` pointing at an on-disk `seller-run.jsonl`. `record_paid_episode` is the
    /// exact writer `try_apply_payment` invokes after journaling the receipt — reverting its body
    /// drops the line (line-count 1→0), which is the piece-A red-on-revert for the paid writer.
    #[test]
    fn paid_episode_writer_appends_one_complete_delivered_paid_line() {
        let (root, daemon) = test_daemon("ep-paid");
        // A real on-disk transcript at the pointed path (run_agent_job writes this in production).
        let transcript_rel = "seller-jobs/jobpaid/seller-run.jsonl";
        let transcript_abs = daemon.home().root.join(transcript_rel);
        std::fs::create_dir_all(transcript_abs.parent().unwrap()).expect("mkdir jobdir");
        std::fs::write(&transcript_abs, b"{\"event\":\"stub\"}\n").expect("write transcript");

        let mut job = active_job(
            "jobpaid",
            daemon.seller_pubkey(),
            Some("res-xyz"),
            2_000_000_000,
            &root,
        );
        job.delivery = Some(DeliveryRecord {
            result_id: "res-xyz".into(),
            commit_oid: "c".repeat(40),
            git_remote: "https://example.invalid/repo.git".into(),
            branch: "mobee/jobpaid".into(),
            delivery_kind: "fork".into(),
            harness: "claude-agent-acp".into(),
            wall_time_ms: 4242,
            usage: None,
            transcript_ref: transcript_rel.into(),
            deliver_ts: 111,
        });

        daemon.record_paid_episode(&job, 21, 21);

        let log = crate::episode::EpisodeLog::open(&daemon.home().root);
        let entries = log.entries().expect("entries");
        assert_eq!(entries.len(), 1, "exactly one episode line for a paid job");
        let episode = &entries[0];
        assert_eq!(episode.outcome, EpisodeOutcome::DeliveredPaid);
        assert_eq!(episode.episode_kind, EpisodeKind::Claimed);
        assert_eq!(episode.result_id.as_deref(), Some("res-xyz"));
        assert_eq!(episode.commit_oid.as_deref(), Some("c".repeat(40).as_str()));
        assert_eq!(episode.amount_received, Some(21));
        assert_eq!(episode.expected_amount, Some(21));
        assert_eq!(episode.transcript_ref.as_deref(), Some(transcript_rel));
        // The pointer resolves to a real file under MOBEE_HOME (pointer, never a copy).
        assert!(
            daemon.home().root.join(episode.transcript_ref.as_ref().unwrap()).is_file(),
            "transcript_ref must point at an on-disk seller-run.jsonl"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Acceptance (piece A): a refused offer appends exactly one `episode_kind=refused` episode
    /// with a non-empty `refusal_reason_code` matching the `OfferSkip` variant, AND the money-path
    /// `seller-journal.jsonl` is byte-unchanged (episodes are a separate stream) and still parses
    /// green. Drives the real daemon writer via `on_offer_event` → reverting the
    /// `record_refused_episode` call drops the line (piece-A red-on-revert for the refused writer).
    #[tokio::test]
    async fn refused_offer_appends_one_refused_episode_and_leaves_journal_untouched() {
        let (root, mut daemon) = test_daemon("ep-refused");
        let journal_before = std::fs::read(daemon.journal.path()).unwrap_or_default();

        // Untargeted offer + claim_open_pool=false ⇒ RateGate refusal on a NON-self target, so
        // the skip path does no relay I/O (publish_under_rate_error_if_targeted early-returns).
        let buyer = nostr_sdk::Keys::generate();
        let draft = crate::gateway::OfferDraft::untargeted(
            "do a task",
            "text/plain",
            10,
            now_unix() + 3_600,
        )
        .to_event_draft();
        let event = gateway::nostr::event_builder(&draft)
            .expect("event builder")
            .sign_with_keys(&buyer)
            .expect("sign offer");

        let claimed = daemon.on_offer_event(&event).await.expect("skip is Ok");
        assert!(claimed.is_none(), "untargeted offer must be refused, not claimed");

        let log = crate::episode::EpisodeLog::open(&daemon.home().root);
        let entries = log.entries().expect("entries");
        assert_eq!(entries.len(), 1, "exactly one refused episode");
        assert_eq!(entries[0].episode_kind, EpisodeKind::Refused);
        assert_eq!(entries[0].outcome, EpisodeOutcome::Refused);
        assert_eq!(entries[0].refusal_reason_code.as_deref(), Some("RateGate"));
        assert!(
            !entries[0].refusal_reason.as_deref().unwrap_or("").is_empty(),
            "refusal_reason is never empty"
        );
        assert_eq!(entries[0].job_id, event.id.to_hex());

        // Money-safety: the journal is a SEPARATE, untouched stream.
        let journal_after = std::fs::read(daemon.journal.path()).unwrap_or_default();
        assert_eq!(
            journal_before, journal_after,
            "seller-journal.jsonl must be byte-unchanged by episode capture"
        );
        daemon.journal.entries().expect("journal still parses green");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Layer-1 read-on-start ──────────────────────────────────────────────────────

    /// With memory DISABLED the composed prompt is byte-identical to the memory-disabled
    /// golden. The expected string is a hardcoded literal (NOT recomputed from the function
    /// under test), so any drift in the disabled path fails this golden.
    #[test]
    fn composed_prompt_disabled_memory_is_byte_identical_golden() {
        let remote = "https://relay.example/git/abc.git";
        let expected = "build a widget\n\n\
---\n\
DELIVERY (required). Deliver your work by committing it with git in your current working directory:\n\
- Make one or more non-empty commits authored by you. Do not leave the deliverable uncommitted and do not only print it to the console.\n\
- You do NOT need to push and you are NOT handed any credentials: the daemon pushes your committed branch to the bound git remote (https://relay.example/git/abc.git) on your behalf.\n\
Anything not committed to git will not be delivered.";
        assert_eq!(
            compose_agent_prompt("build a widget", remote, None),
            expected,
            "disabled-memory prompt must be byte-identical to the pre-piece-13 golden"
        );
    }

    /// Acceptance (piece B): memory_enabled=false ⇒ the daemon produces NO read-on-start section
    /// and does not even create the memory dir.
    #[tokio::test]
    async fn disabled_memory_yields_no_section_and_no_dir() {
        let (root, mut daemon) = test_daemon("mem-disabled");
        daemon.home.config.seller_memory.memory_enabled = false;
        assert!(daemon.read_on_start_section().is_none(), "no section when disabled");
        assert!(
            !crate::seller_memory::memory_dir(&daemon.home().root).exists(),
            "disabled memory must not create the memory dir"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Acceptance (piece B): with memory enabled and a non-empty index, the composed prompt
    /// contains the index text and the absolute memory path; and first use seeds operator-notes.md
    /// stamped author: operator.
    #[tokio::test]
    async fn enabled_memory_inlines_index_and_seeds_operator_notes() {
        let (root, daemon) = test_daemon("mem-enabled");
        // default config ⇒ memory_enabled = true.
        let section = daemon.read_on_start_section().expect("section present");
        let dir = crate::seller_memory::memory_dir(&daemon.home().root);
        assert!(section.contains("Seller memory index"), "index text inlined: {section}");
        assert!(
            section.contains(&dir.display().to_string()),
            "absolute memory path named: {section}"
        );
        // The full composed prompt is a superset of the disabled output.
        let prompt = compose_agent_prompt("t", "https://r/git/x.git", Some(&section));
        assert!(prompt.contains("Seller memory index"));
        assert!(prompt.starts_with("t\n\n---\nDELIVERY"), "delivery block preserved");

        // On first creation the dir carries operator-notes.md stamped author: operator.
        let notes = dir.join(crate::seller_memory::OPERATOR_NOTES_FILE);
        assert!(notes.is_file(), "operator-notes.md seeded on first read-on-start");
        let author = crate::seller_memory::frontmatter_author(
            &std::fs::read_to_string(&notes).expect("read notes"),
        );
        assert_eq!(author.as_deref(), Some(crate::seller_memory::AUTHOR_OPERATOR));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Acceptance (piece B): the read-on-start template seam overrides the in-repo default when
    /// `read_on_start_template_path` is set (daemon path).
    #[tokio::test]
    async fn read_on_start_template_seam_used_by_daemon() {
        let (root, mut daemon) = test_daemon("mem-seam");
        let template = daemon.home().root.join("my-read.tmpl");
        std::fs::write(&template, "CUSTOM-FRAME {memory_index}").expect("write template");
        daemon.home.config.seller_memory.read_on_start_template_path = Some(template);
        let section = daemon.read_on_start_section().expect("section");
        assert!(section.starts_with("CUSTOM-FRAME"), "operator template used: {section}");
        assert!(!section.contains("SELLER MEMORY"), "default framing not used");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A not-an-offer / dedup re-see does not produce an episode (only freshly-classified jobs do).
    #[test]
    fn refused_episode_skips_non_offer_and_already_processed() {
        let (root, daemon) = test_daemon("ep-refused-skip");
        let event = offer_event(&nostr_sdk::Keys::generate(), daemon.seller_pubkey(), 5, now_unix() + 3_600);
        daemon.record_refused_episode(&event, &OfferSkip::NotAnOffer { kind: 1 });
        daemon.record_refused_episode(&event, &OfferSkip::AlreadyProcessed);
        let log = crate::episode::EpisodeLog::open(&daemon.home().root);
        assert!(log.entries().expect("entries").is_empty(), "no episode for non-job skips");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Retro write-back ───────────────────────────────────────────────────────────

    /// A test Driver that simulates a misbehaving retro agent: on the prompt turn it CLOBBERS a
    /// target (operator) file, then completes. Proves merge-not-clobber is enforced at runtime
    /// (not by prompt prose): `run_retro_turn` must byte-revert the file afterward.
    struct ClobberingDriver {
        target: PathBuf,
        clobber_with: Vec<u8>,
    }

    impl crate::driver::Driver for ClobberingDriver {
        fn id(&self) -> crate::event::RuntimeId {
            crate::event::RuntimeId("clobber".into())
        }
        async fn ready(&mut self) -> Result<crate::driver::Readiness, crate::driver::DriverError> {
            Ok(crate::driver::Readiness {
                runtime_id: self.id(),
                protocol_version: crate::driver::acp::PROTOCOL_VERSION,
            })
        }
        async fn start_session(
            &mut self,
            _cfg: crate::driver::SessionConfig,
        ) -> Result<crate::driver::SessionId, crate::driver::DriverError> {
            Ok("clobber-session".into())
        }
        async fn prompt(
            &mut self,
            _session_id: &crate::driver::SessionId,
            _turn: crate::driver::PromptTurn,
        ) -> Result<crate::driver::UpdateStream, crate::driver::DriverError> {
            std::fs::write(&self.target, &self.clobber_with).expect("clobber write");
            Ok(crate::driver::UpdateStream::new(
                vec![crate::driver::SessionUpdate::TurnEnded(
                    crate::driver::StopReason::Completed,
                )],
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ))
        }
        async fn on_permission(
            &mut self,
            _req: crate::driver::PermissionRequest,
        ) -> crate::driver::PermissionOutcome {
            crate::driver::PermissionOutcome::Allow
        }
        async fn artifacts(
            &self,
            _session_id: &crate::driver::SessionId,
        ) -> Result<Vec<crate::driver::Artifact>, crate::driver::DriverError> {
            Ok(Vec::new())
        }
        async fn cancel(
            &mut self,
            _session_id: &crate::driver::SessionId,
        ) -> Result<(), crate::driver::DriverError> {
            Ok(())
        }
        async fn shutdown(&mut self) -> Result<(), crate::driver::DriverError> {
            Ok(())
        }
    }

    fn write_paid_episode(daemon: &SellerDaemon, job_id: &str) {
        let mut episode = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::DeliveredPaid,
            7,
            daemon.seller_pubkey(),
            job_id,
        );
        episode.result_id = Some(format!("res-{job_id}"));
        episode.transcript_ref = Some(format!("seller-jobs/{job_id}/seller-run.jsonl"));
        crate::episode::EpisodeLog::open(&daemon.home().root)
            .append(&episode)
            .expect("append paid episode");
    }

    /// Acceptance (piece C): a completed job triggers exactly ONE extra agent turn (carrying the
    /// retro prompt) and `MEMORY.md` exists afterward. The turn's session cwd is the memory dir
    /// (set by `run_retro_turn`; the merge-not-clobber test below depends on that cwd's writes).
    #[tokio::test]
    async fn retro_turn_issues_exactly_one_turn_and_memory_index_exists() {
        use crate::driver::{MockDriver, ScriptedSession, SessionUpdate, StopReason};
        use crate::event::RuntimeId;
        let root = temp("retro-one");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        let dir = crate::seller_memory::memory_dir(&root);
        crate::seller_memory::ensure_memory_dir(&dir).expect("ensure");

        let script = ScriptedSession {
            session_id: "retro-1".into(),
            updates: vec![SessionUpdate::TurnEnded(StopReason::Completed)],
            artifacts: Vec::new(),
        };
        let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![script]);
        let log_path = root.join("seller-retro.jsonl");
        run_retro_turn(&mut driver, &dir, "distill this job", &log_path)
            .await
            .expect("retro turn ok");

        assert_eq!(driver.prompt_history().len(), 1, "exactly one extra agent turn");
        let (_sid, turn) = &driver.prompt_history()[0];
        assert!(
            matches!(&turn.input[0], crate::driver::ContentBlock::Text { text } if text == "distill this job"),
            "the extra turn carried the retro prompt"
        );
        assert!(dir.join("MEMORY.md").is_file(), "MEMORY.md exists after retro");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Acceptance (piece C): with retro_enabled=false no retro is planned (no extra turn issued).
    #[test]
    fn retro_disabled_plans_nothing() {
        let (root, mut daemon) = test_daemon("retro-off");
        write_paid_episode(&daemon, "jd");
        daemon.home.config.seller_memory.retro_enabled = false;
        assert!(
            daemon.retro_context("jd").is_none(),
            "retro_enabled=false ⇒ no plan ⇒ no extra turn"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Acceptance (piece C): the retro plan is seeded with the episode (JSON) + the ABSOLUTE
    /// transcript path, uses the default framing (which instructs `author: agent` + never touch
    /// operator files), and honors the retro_prompt_path seam. No paid episode ⇒ no plan.
    #[test]
    fn retro_context_seeds_episode_and_honors_prompt_seam() {
        let (root, mut daemon) = test_daemon("retro-ctx");
        assert!(daemon.retro_context("absent").is_none(), "no paid episode ⇒ no plan");

        write_paid_episode(&daemon, "jp");
        let plan = daemon.retro_context("jp").expect("plan for paid job");
        assert!(plan.prompt.contains("\"job_id\": \"jp\""), "episode json seeded: {}", plan.prompt);
        assert!(plan.prompt.contains("DURABLE MEMORY"), "default retro framing");
        assert!(
            plan.prompt.contains("author: agent"),
            "prompt instructs the agent to stamp author: agent"
        );
        let transcript_abs = daemon
            .home()
            .root
            .join("seller-jobs/jp/seller-run.jsonl")
            .display()
            .to_string();
        assert!(plan.prompt.contains(&transcript_abs), "absolute transcript path seeded");

        // Seam override wins.
        let template = daemon.home().root.join("retro.tmpl");
        std::fs::write(&template, "SEAM DISTILLER for {episode_json}").expect("write template");
        daemon.home.config.seller_memory.retro_prompt_path = Some(template);
        let plan2 = daemon.retro_context("jp").expect("plan2");
        assert!(plan2.prompt.starts_with("SEAM DISTILLER for"), "uses operator retro template");
        assert!(!plan2.prompt.contains("DURABLE MEMORY"), "default framing not used");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Acceptance (piece C): a retro forced to fail surfaces an error the caller swallows, and the
    /// MONEY path is green — the journal (a real claim+receipt) is byte-unchanged and still parses.
    #[tokio::test]
    async fn retro_failure_leaves_money_path_green() {
        use crate::driver::MockDriver;
        use crate::event::RuntimeId;
        let (root, daemon) = test_daemon("retro-fail");
        // Money state: a real journaled claim + receipt.
        daemon.journal.append_claim("jf", "cf", "bf", 2_000_000_000).expect("claim");
        daemon
            .journal
            .append_receipt("jf", "rf", "jf", "rf", 7, 7, DEFAULT_MINT_URL, "bf", true)
            .expect("receipt");
        let journal_before = std::fs::read(daemon.journal.path()).expect("read journal");

        let dir = crate::seller_memory::memory_dir(&daemon.home().root);
        crate::seller_memory::ensure_memory_dir(&dir).expect("ensure");
        // Empty scripts ⇒ start_session ScriptExhausted ⇒ the retro turn FAILS.
        let mut driver = MockDriver::new(RuntimeId("mock".into()), Vec::new());
        let log_path = root.join("seller-retro.jsonl");
        let result = run_retro_turn(&mut driver, &dir, "distill", &log_path).await;
        assert!(result.is_err(), "forced retro failure surfaces Err (the caller swallows it)");

        assert_eq!(
            std::fs::read(daemon.journal.path()).expect("read"),
            journal_before,
            "money path GREEN: journal byte-unchanged across a failed retro"
        );
        daemon.journal.entries().expect("journal still parses green");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Acceptance (piece C): merge-not-clobber across a retro RUN. An agent that clobbers an
    /// `author: operator` file mid-turn is byte-reverted by `run_retro_turn`. Reverting the
    /// restore leaves the file HIJACKED — the piece-C red-on-revert target.
    #[tokio::test]
    async fn retro_run_reverts_operator_clobber() {
        let root = temp("retro-merge");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        let dir = crate::seller_memory::memory_dir(&root);
        crate::seller_memory::ensure_memory_dir(&dir).expect("ensure");
        let notes = dir.join(crate::seller_memory::OPERATOR_NOTES_FILE);
        let notes_before = std::fs::read(&notes).expect("read notes");
        // A pre-existing operator topic file too.
        let house = dir.join("house-rules.md");
        std::fs::write(&house, "---\nauthor: operator\n---\nORIGINAL").expect("write house");

        let mut driver = ClobberingDriver {
            target: notes.clone(),
            clobber_with: b"HIJACKED BY AGENT".to_vec(),
        };
        let log_path = root.join("seller-retro.jsonl");
        run_retro_turn(&mut driver, &dir, "distill", &log_path)
            .await
            .expect("retro ok");

        assert_eq!(
            std::fs::read(&notes).expect("read"),
            notes_before,
            "operator-notes.md byte-unchanged across the retro (merge-not-clobber)"
        );
        assert_eq!(
            std::fs::read_to_string(&house).expect("read"),
            "---\nauthor: operator\n---\nORIGINAL",
            "operator topic file byte-unchanged across the retro"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── GATE-RED regression: the daemon must not go deaf to kind-1059 payments ────────────────

    /// Root-cause guard #1 (deaf-daemon): a broadcast LAG must NOT terminate the event loop.
    /// Before the fix, `while let Ok(..) = recv().await` ended the loop on `Lagged`, so a seller
    /// that fell behind during a long agent turn went silently deaf to ALL further offers and
    /// kind-1059 payments (wraps parked, never collected) until restart. Reverting the fix
    /// (Lagged ⇒ Stop) fails this test.
    #[test]
    fn lagged_recv_keeps_daemon_alive_only_closed_stops() {
        use tokio::sync::broadcast::error::RecvError;
        assert_eq!(
            classify_recv_error(&RecvError::Lagged(42)),
            RecvControl::Continue,
            "a broadcast lag must keep the daemon alive (never go deaf to payments)"
        );
        assert_eq!(
            classify_recv_error(&RecvError::Closed),
            RecvControl::Stop,
            "only a closed channel ends the loop"
        );
    }

    /// A Driver whose prompt turn BLOCKS (simulating a slow/hanging retro agent), then completes.
    struct SlowDriver {
        block: std::time::Duration,
    }

    impl crate::driver::Driver for SlowDriver {
        fn id(&self) -> crate::event::RuntimeId {
            crate::event::RuntimeId("slow".into())
        }
        async fn ready(&mut self) -> Result<crate::driver::Readiness, crate::driver::DriverError> {
            Ok(crate::driver::Readiness {
                runtime_id: self.id(),
                protocol_version: crate::driver::acp::PROTOCOL_VERSION,
            })
        }
        async fn start_session(
            &mut self,
            _cfg: crate::driver::SessionConfig,
        ) -> Result<crate::driver::SessionId, crate::driver::DriverError> {
            Ok("slow-session".into())
        }
        async fn prompt(
            &mut self,
            _session_id: &crate::driver::SessionId,
            _turn: crate::driver::PromptTurn,
        ) -> Result<crate::driver::UpdateStream, crate::driver::DriverError> {
            std::thread::sleep(self.block); // a retro agent turn that takes real time
            Ok(crate::driver::UpdateStream::new(
                vec![crate::driver::SessionUpdate::TurnEnded(
                    crate::driver::StopReason::Completed,
                )],
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ))
        }
        async fn on_permission(
            &mut self,
            _req: crate::driver::PermissionRequest,
        ) -> crate::driver::PermissionOutcome {
            crate::driver::PermissionOutcome::Allow
        }
        async fn artifacts(
            &self,
            _session_id: &crate::driver::SessionId,
        ) -> Result<Vec<crate::driver::Artifact>, crate::driver::DriverError> {
            Ok(Vec::new())
        }
        async fn cancel(
            &mut self,
            _session_id: &crate::driver::SessionId,
        ) -> Result<(), crate::driver::DriverError> {
            Ok(())
        }
        async fn shutdown(&mut self) -> Result<(), crate::driver::DriverError> {
            Ok(())
        }
    }

    /// Root-cause guard #2 (retro must not block the loop): a retro turn runs to completion on a
    /// DETACHED thread, so the caller (the event loop) is not blocked for the retro's duration.
    /// This is exactly the pattern `maybe_run_retro` now uses (own thread + own runtime). Before
    /// the fix the retro ran inline via `.await`, blocking wrap collection for the whole turn.
    #[test]
    fn retro_turn_runs_detached_without_blocking_caller() {
        use std::time::{Duration, Instant};
        let root = temp("retro-detach");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        let dir = crate::seller_memory::memory_dir(&root);
        crate::seller_memory::ensure_memory_dir(&dir).expect("ensure");
        let log_path = root.join("seller-retro.jsonl");

        let block = Duration::from_millis(800);
        let started = Instant::now();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let mut driver = SlowDriver { block };
            runtime.block_on(run_retro_turn(&mut driver, &dir, "distill", &log_path))
        });
        // The caller (loop) returns immediately — NOT blocked for the retro's ~800ms turn.
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "spawning the retro must not block the caller (elapsed {:?})",
            started.elapsed()
        );
        let result = handle.join().expect("retro thread");
        assert!(result.is_ok(), "detached retro completes: {result:?}");
        assert!(
            started.elapsed() >= block,
            "the retro really did run in the background (took its full turn)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// LOCAL-RELAY integration tests: drive [`run_forever_hooked`] end-to-end against an in-process
/// NIP-01 relay (`nostr-relay-builder`) and assert on RELAY TRAFFIC + TIMING — never on
/// stderr/scrollback. They prove the open-pool live-delivery behaviour:
///  * **A** — the daemon reaches READY quickly (mpsc readiness hook, not a log scrape);
///  * **B** — a running seller does NOT replay the relay's offer history (bounded startup ingest);
///  * **C** — a fresh UNtargeted offer posted to a RUNNING seller is CLAIMED without a
///    restart (relay receives the seller's claim-kind `status=processing` tagged `e=<offer id>`) —
///    the "untargeted offer → claim without restart" proof that was previously never obtained;
///  * **D** — if the relay CLOSES the broad open-pool subscription, the daemon still reaches READY
///    and degrades to the TARGETED-only filter, still claiming a fresh targeted offer (the
///    Loud-Closed fallback).
///
/// NIP-42 deviation (noted): the in-process relay issues AUTH challenges only LAZILY (on the
/// first REQ/EVENT), whereas mobee-relay challenges on connect. So these tests run the relay
/// WITHOUT NIP-42 gating and rely on the daemon's non-fatal `NoChallenge` auth path (a
/// challenge-on-connect relay still authenticates in milliseconds — unchanged behaviour there).
/// Offer DELIVERY is the behaviour under test here; the p-gated kind-1059 receive (money path) is
/// exercised separately against a real relay.
#[cfg(test)]
mod local_relay_it {
    use super::*;
    use crate::gateway::{self, OfferDraft};
    use crate::home::{self, SellerConfig, DEFAULT_MINT_URL};
    use nostr_relay_builder::prelude::{
        Alphabet, BoxedFuture, LocalRelay, PolicyResult, QueryPolicy, RelayBuilder, SingleLetterTag,
    };
    use nostr_sdk::prelude::{
        Client, Filter, Keys, Kind, PublicKey, RelayPoolNotification, Timestamp,
    };
    use std::collections::HashSet;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    static IT_SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_root(label: &str) -> std::path::PathBuf {
        let n = IT_SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("mobee-it-{label}-{}-{n}", std::process::id()))
    }

    /// Bootstrap a seller home bound to `relay_url` (mint stays the fail-closed testnut default).
    /// `offer_backfill_secs` sets the offer-backfill window (0 = live-only, pre-backfill shape).
    fn seller_home(
        root: &std::path::Path,
        relay_url: &str,
        claim_open_pool: bool,
        offer_backfill_secs: u64,
    ) -> home::MobeeHome {
        let mut h = home::bootstrap(root).expect("bootstrap seller home");
        h.config.relay_url = relay_url.to_string();
        assert_eq!(
            h.config.default_mint(),
            DEFAULT_MINT_URL,
            "home mint must be the fail-closed testnut default"
        );
        h.config.seller = Some(SellerConfig {
            // Stub agent: the CLAIM (claim-kind processing) is published in `on_offer_event` BEFORE
            // execution, so no real ACP agent is needed — execution fails fast after the claim we
            // assert on, releasing single-flight.
            agent_command: vec!["true".into()],
            rate_sats: 1,
            git_remote: "https://example.invalid/mobee-it.git".into(),
            job_timeout_secs: Some(5),
            agent: None,
            claim_open_pool,
            offer_backfill_secs,
            contribution_enabled: true,
        });
        h
    }

    /// Start the in-process relay from `builder`. Keep the returned handle alive for the whole
    /// test — dropping every clone shuts the relay down.
    async fn start_relay(builder: RelayBuilder) -> (LocalRelay, String) {
        let relay = LocalRelay::new(builder);
        relay.run().await.expect("relay run");
        let url = relay.url().await.to_string();
        (relay, url)
    }

    async fn connect_client(relay_url: &str) -> Client {
        let client = Client::new(Keys::generate());
        client.add_relay(relay_url).await.expect("add relay");
        client.connect().await;
        client.wait_for_connection(Duration::from_secs(5)).await;
        client
    }

    /// Build an offer draft carrying the fail-closed testnut mint (so it is never skipped for the
    /// wrong reason) and a FUTURE deadline (~1h out) so the offer-freshness gate does not refuse
    /// it. `targeted_to = Some(hex)` ⇒ a `#p`-tagged (targeted) offer; `None` ⇒ open.
    fn offer_draft(targeted_to: Option<&str>) -> OfferDraft {
        offer_draft_with_deadline(targeted_to, now_unix() + 3_600)
    }

    /// Like [`offer_draft`] but with an explicit `deadline_unix` — used to build an already-lapsed
    /// offer (deadline in the past) for the deadline-expiry money-safety test.
    fn offer_draft_with_deadline(targeted_to: Option<&str>, deadline_unix: u64) -> OfferDraft {
        match targeted_to {
            Some(pk) => OfferDraft::new("do a task", "text", 10, deadline_unix, pk),
            None => OfferDraft::untargeted("do a task", "text", 10, deadline_unix),
        }
    }

    /// Sign an offer with `buyer` via the SAME event bridge the buyer CLI uses, then publish it.
    async fn publish_offer(
        client: &Client,
        buyer: &Keys,
        draft: &OfferDraft,
        created_at: Option<Timestamp>,
    ) -> nostr_sdk::EventId {
        let mut builder =
            gateway::nostr::event_builder(&draft.to_event_draft()).expect("offer event builder");
        if let Some(ts) = created_at {
            builder = builder.custom_created_at(ts);
        }
        let event = builder.sign_with_keys(buyer).expect("sign offer");
        let id = event.id;
        client.send_event(&event).await.expect("publish offer");
        id
    }

    fn tag_value(event: &nostr_sdk::Event, name: &str) -> Option<String> {
        event.tags.iter().find_map(|t| {
            let slice = t.as_slice();
            (slice.first().map(String::as_str) == Some(name))
                .then(|| slice.get(1).cloned())
                .flatten()
        })
    }

    /// Collect every feedback-kind the seller publishes as `(e-tag, status)`. The receiver is created
    /// before the daemon starts so no startup claim is missed.
    fn spawn_claim_collector(
        client: &Client,
        seller_pk: PublicKey,
    ) -> Arc<Mutex<Vec<(String, String)>>> {
        let claims: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = claims.clone();
        let mut notif = client.notifications();
        tokio::spawn(async move {
            while let Ok(n) = notif.recv().await {
                if let RelayPoolNotification::Event { event, .. } = n {
                    let kind = event.kind.as_u16();
                    if (kind == gateway::JOB_CLAIM_KIND || kind == gateway::JOB_FEEDBACK_KIND)
                        && event.pubkey == seller_pk
                    {
                        if let Some(e) = tag_value(&event, "e") {
                            let status = tag_value(&event, "status").unwrap_or_default();
                            sink.lock().unwrap_or_else(|e| e.into_inner()).push((e, status));
                        }
                    }
                }
            }
        });
        claims
    }

    fn claims_contain(claims: &Arc<Mutex<Vec<(String, String)>>>, offer_id: &str, status: &str) -> bool {
        claims
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(e, s)| e == offer_id && s == status)
    }

    /// Poll `cond` until true or `timeout` elapses.
    async fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut cond: F) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn hooks(ready: tokio::sync::mpsc::UnboundedSender<()>) -> RunHooks {
        RunHooks {
            ready: Some(ready),
            // Short auth-wait: the ungated in-process relay never challenges, so proceed fast.
            auth_wait: Some(Duration::from_millis(500)),
            wrap_backfill_done: None,
        }
    }

    /// Run the daemon on a dedicated OS thread with its own current-thread runtime — mirroring
    /// production `run_forever_blocking`. This sidesteps `tokio::spawn`'s `Send` bound: under the
    /// `acp` feature the daemon future is not `Send` (`AcpDriver` holds a non-`Sync` mpsc
    /// receiver). The daemon reaches the relay over localhost and signals READY over `ready`.
    fn spawn_daemon_thread(
        daemon: SellerDaemon,
        ready: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("daemon runtime");
            let _ = rt.block_on(run_forever_hooked(daemon, hooks(ready)));
        })
    }

    /// Like [`spawn_daemon_thread`] but also wires the PERIODIC wrap-backfill hook, so the #57
    /// periodic tests can observe each periodic run's stored-1059 count without scraping stderr.
    fn spawn_daemon_thread_with_backfill_hook(
        daemon: SellerDaemon,
        ready: tokio::sync::mpsc::UnboundedSender<()>,
        backfill_done: tokio::sync::mpsc::UnboundedSender<usize>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("daemon runtime");
            let _ = rt.block_on(run_forever_hooked(
                daemon,
                RunHooks {
                    ready: Some(ready),
                    auth_wait: Some(Duration::from_millis(500)),
                    wrap_backfill_done: Some(backfill_done),
                },
            ));
        })
    }

    // ── Assertions A + B + C on one running open-pool seller ────────────────────────────────
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_untargeted_delivery_to_running_open_pool_seller() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Clear any single-flight left by a prior test's (now relay-less) daemon thread.
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        // Seed the relay's offer HISTORY: M offers dated 2 min in the PAST (mix untargeted +
        // targeted-foreign), all with the fail-closed testnut mint. A BOUNDED running seller must
        // ingest ~none of these; an unbounded one would fetch them all and claim one.
        let buyer = Keys::generate();
        let foreign_seller = Keys::generate().public_key().to_hex();
        let seeder = connect_client(&relay_url).await;
        let past = Timestamp::from(Timestamp::now().as_secs().saturating_sub(120));
        const M: usize = 60;
        let mut historical_ids: HashSet<String> = HashSet::new();
        for i in 0..M {
            let draft = if i % 2 == 0 {
                offer_draft(None)
            } else {
                offer_draft(Some(&foreign_seller))
            };
            let id = publish_offer(&seeder, &buyer, &draft, Some(past)).await;
            historical_ids.insert(id.to_hex());
        }

        // Open the seller daemon (open-pool) and capture its pubkey.
        let home = seller_home(&unique_root("live"), &relay_url, true, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");

        // Observer collects the seller's feedback-kind BEFORE the daemon starts.
        let observer = connect_client(&relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        // Start the daemon (own thread + current-thread runtime) with the readiness hook.
        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let _daemon = spawn_daemon_thread(daemon, ready_tx);

        // A. READY ≤ 10s (mpsc hook — not a stderr scrape).
        let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "A: daemon must reach READY within 10s (got {ready:?})"
        );

        // B. BOUNDED startup ingest: after a settle window a bounded seller has claimed ZERO
        // historical offers. On the unbounded/ungrouped filter it would fetch all M and claim ≥1.
        tokio::time::sleep(Duration::from_secs(3)).await;
        {
            let claimed = claims.lock().unwrap_or_else(|e| e.into_inner());
            let historical_hits: Vec<&(String, String)> = claimed
                .iter()
                .filter(|(e, _)| historical_ids.contains(e))
                .collect();
            assert!(
                historical_hits.is_empty(),
                "B: bounded seller must NOT claim historical offers; saw {} historical claim(s): {:?}",
                historical_hits.len(),
                historical_hits
            );
        }

        // C. LIVE untargeted delivery (THE goal proof): publish a FRESH untargeted offer to the
        // RUNNING seller and assert the relay receives its feedback-kind status=processing claim
        // (e=<offer id>) within 10s — an untargeted offer claimed WITHOUT a restart.
        let live_id = publish_offer(&seeder, &buyer, &offer_draft(None), None)
            .await
            .to_hex();
        let claimed_live = wait_until(Duration::from_secs(10), || {
            claims_contain(&claims, &live_id, "processing")
        })
        .await;
        assert!(
            claimed_live,
            "C: running open-pool seller must CLAIM a fresh untargeted offer (feedback-kind \
             processing e={live_id}) within 10s — the live untargeted-delivery proof"
        );

        // Quiesce: let execution finish so single-flight is released before the guard drops.
        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    // ── #57: boot backfill retrieves a stored kind-1059 without wedging the daemon ───────────
    // A stored gift-wrap p-tagged to the seller (the stranded-payment shape) is seeded BEFORE boot.
    // The reordered boot backfill (after online/readiness, auth-confirmed, hard-capped) must fetch
    // and ingest it, reach READY, and keep processing LIVE offers. Proves the recovery path runs
    // end-to-end over a real stored 1059 without hanging boot. (The auth-gate itself is exercised by
    // the live relay on deploy; here the local relay serves 1059 without NIP-42.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn boot_backfill_over_stored_wrap_stays_healthy() {
        use nostr_sdk::prelude::{EventBuilder, Tag};

        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        // Open the seller (to learn its pubkey), then seed a stored 1059 gift-wrap p-tagged to it.
        let home = seller_home(&unique_root("backfill-wrap"), &relay_url, true, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");

        let seeder = connect_client(&relay_url).await;
        let past = Timestamp::from(Timestamp::now().as_secs().saturating_sub(120));
        let stored_wrap = EventBuilder::new(Kind::GiftWrap, "opaque-stored-wrap")
            .tag(Tag::public_key(seller_pk))
            .custom_created_at(past)
            .sign_with_keys(&Keys::generate())
            .expect("sign stored wrap");
        seeder
            .send_event(&stored_wrap)
            .await
            .expect("seed stored wrap");

        // Observer for the seller's claims (before boot, so none is missed).
        let observer = connect_client(&relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let _daemon = spawn_daemon_thread(daemon, ready_tx);

        // The boot backfill retrieves + ingests the stored 1059 (opaque ⇒ not a decodable
        // own-payment wrap) WITHOUT wedging boot: readiness still fires.
        let ready = tokio::time::timeout(Duration::from_secs(12), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "daemon must reach READY despite a stored wrap in the boot backfill (got {ready:?})"
        );

        // And it still processes LIVE work after the backfill: a fresh targeted offer is claimed.
        let buyer = Keys::generate();
        let live_id = publish_offer(&seeder, &buyer, &offer_draft(Some(&seller_pk.to_hex())), None)
            .await
            .to_hex();
        let claimed = wait_until(Duration::from_secs(12), || {
            claims_contain(&claims, &live_id, "processing")
        })
        .await;
        assert!(
            claimed,
            "seller must still CLAIM a live offer after boot-backfilling a stored wrap (e={live_id})"
        );

        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    // ── #57 TEST (2): a wrap stored AFTER boot is picked up by a PERIODIC backfill run ────────
    // The field bug: a live-but-AGED relay subscription silently stops delivering kind-1059 wraps,
    // so a payment sent to a running daemon sits unredeemed until restart. The fix is a periodic
    // timer that re-runs the boot stored-wrap backfill. TESTABILITY (honest): the in-process relay
    // CANNOT reproduce an aged, silently-deaf live subscription — fresh in-process subs always
    // deliver. What is reproducible, and what actually fixes the bug, is the RECOVERY mechanism: a
    // periodic run independently RE-FETCHES stored wraps p-tagged to us (a fresh short-lived REQ)
    // and runs each through the SAME `ingest_gift_wrap` path. We seed a wrap AFTER boot and assert
    // a periodic run returns it (count >= 1) — proving the timer fires and the fetch+ingest runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn periodic_backfill_picks_up_wrap_stored_after_boot() {
        use nostr_sdk::prelude::{EventBuilder, Tag};

        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        // Fast periodic cadence via the test seam (held under the guard, so no concurrent daemon
        // test reads this env).
        unsafe {
            std::env::set_var(WRAP_BACKFILL_INTERVAL_ENV, "2");
        }

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;
        let home = seller_home(&unique_root("periodic-pickup"), &relay_url, true, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let (backfill_tx, mut backfill_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
        let _daemon = spawn_daemon_thread_with_backfill_hook(daemon, ready_tx, backfill_tx);

        // Up and running; boot backfill already ran (no wrap stored yet).
        let ready = tokio::time::timeout(Duration::from_secs(12), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "daemon must reach READY (got {ready:?})"
        );

        // Seed a stored 1059 gift-wrap p-tagged to the seller AFTER boot (the stranded-payment
        // shape an aged live sub would miss). `last_receipt_ts == 0`, so the since-cursor covers it.
        let seeder = connect_client(&relay_url).await;
        let stored_wrap = EventBuilder::new(Kind::GiftWrap, "opaque-post-boot-wrap")
            .tag(Tag::public_key(seller_pk))
            .sign_with_keys(&Keys::generate())
            .expect("sign stored wrap");
        seeder
            .send_event(&stored_wrap)
            .await
            .expect("seed stored wrap");

        // A periodic run AFTER the wrap was stored must return >= 1 stored 1059 (fetched+ingested).
        let picked_up = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match backfill_rx.recv().await {
                    Some(count) if count >= 1 => break true,
                    Some(_) => continue,
                    None => break false,
                }
            }
        })
        .await;
        assert!(
            matches!(picked_up, Ok(true)),
            "a periodic backfill run must FETCH+INGEST the wrap stored after boot (got {picked_up:?})"
        );

        unsafe {
            std::env::remove_var(WRAP_BACKFILL_INTERVAL_ENV);
        }
        relay.shutdown();
    }

    // ── #57 TEST (3): repeated periodic runs stay idempotent and never wedge the loop ─────────
    // A stored wrap seeded BEFORE boot is re-seen by EVERY periodic run. Each run re-ingests it
    // through the SAME guarded, idempotent path (`ingest_gift_wrap` → `try_apply_or_buffer`:
    // journal pay-once / mint-refuse — money guards unchanged and covered by the money-path tests;
    // an opaque wrap decodes to "not ours" and makes no redeem attempt at all). We prove: (a) the
    // timer fires REPEATEDLY over the same wrap, and (b) the daemon still claims a fresh live offer
    // afterwards — i.e. the periodic runs never block the event loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn periodic_backfill_reruns_stay_idempotent_and_healthy() {
        use nostr_sdk::prelude::{EventBuilder, Tag};

        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();
        unsafe {
            std::env::set_var(WRAP_BACKFILL_INTERVAL_ENV, "2");
        }

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;
        let home = seller_home(&unique_root("periodic-idem"), &relay_url, true, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");

        // Seed the stored wrap BEFORE boot so every periodic run re-sees the SAME wrap.
        let seeder = connect_client(&relay_url).await;
        let stored_wrap = EventBuilder::new(Kind::GiftWrap, "opaque-repeat-wrap")
            .tag(Tag::public_key(seller_pk))
            .sign_with_keys(&Keys::generate())
            .expect("sign stored wrap");
        seeder
            .send_event(&stored_wrap)
            .await
            .expect("seed stored wrap");

        // Observer for the seller's claims (before boot, so none is missed).
        let observer = connect_client(&relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let (backfill_tx, mut backfill_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
        let _daemon = spawn_daemon_thread_with_backfill_hook(daemon, ready_tx, backfill_tx);

        let ready = tokio::time::timeout(Duration::from_secs(12), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "daemon must reach READY (got {ready:?})"
        );

        // Wait for at least TWO periodic runs over the SAME stored wrap — the re-run path.
        let mut runs = 0usize;
        let two_runs = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match backfill_rx.recv().await {
                    Some(count) => {
                        assert!(count >= 1, "each periodic run must re-see the stored wrap");
                        runs += 1;
                        if runs >= 2 {
                            break true;
                        }
                    }
                    None => break false,
                }
            }
        })
        .await;
        assert!(
            matches!(two_runs, Ok(true)),
            "the periodic timer must fire repeatedly over the same wrap (got {two_runs:?}, runs={runs})"
        );

        // Still healthy after repeated backfills: a fresh targeted (live) offer is claimed without
        // a restart — the periodic runs never block the event loop.
        let buyer = Keys::generate();
        let live_id = publish_offer(&seeder, &buyer, &offer_draft(Some(&seller_pk.to_hex())), None)
            .await
            .to_hex();
        let claimed = wait_until(Duration::from_secs(12), || {
            claims_contain(&claims, &live_id, "processing")
        })
        .await;
        assert!(
            claimed,
            "seller must still CLAIM a live offer after repeated periodic backfills (e={live_id})"
        );

        unsafe {
            std::env::remove_var(WRAP_BACKFILL_INTERVAL_ENV);
        }
        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    // ── D: relay restricts the broad open-pool filter → seller degrades to targeted ──────────

    /// A [`QueryPolicy`] that rejects a REQ carrying a broad offer filter (offer-kind, no authors,
    /// no `#p`) — i.e. the un-pinned open-pool filter — while allowing the targeted `#p==self`
    /// filter. Models a relay that refuses to serve the open-pool firehose.
    #[derive(Debug)]
    struct RejectBroadOfferQueries;

    impl QueryPolicy for RejectBroadOfferQueries {
        fn admit_query<'a>(
            &'a self,
            query: &'a Filter,
            _addr: &'a SocketAddr,
        ) -> BoxedFuture<'a, PolicyResult> {
            Box::pin(async move {
                let is_offer_kind = query
                    .kinds
                    .as_ref()
                    .is_some_and(|ks| ks.contains(&Kind::Custom(gateway::JOB_OFFER_KIND)));
                let has_authors = query.authors.as_ref().is_some_and(|a| !a.is_empty());
                let has_p_tag = query
                    .generic_tags
                    .contains_key(&SingleLetterTag::lowercase(Alphabet::P));
                if is_offer_kind && !has_authors && !has_p_tag {
                    PolicyResult::Reject("blocked: broad open-pool offer filter".to_string())
                } else {
                    PolicyResult::Accept
                }
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn relay_restricts_broad_filter_seller_degrades_to_targeted() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) =
            start_relay(RelayBuilder::default().query_policy(RejectBroadOfferQueries)).await;

        let home = seller_home(&unique_root("fallback"), &relay_url, true, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");
        let seller_hex = daemon.seller_pubkey().to_string();

        let observer = connect_client(&relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let _daemon = spawn_daemon_thread(daemon, ready_tx);

        // D-1: the daemon still reaches READY even though the relay CLOSES the broad grouped offer
        // subscription (ready fires after subscribe; the CLOSED is handled in the loop after).
        let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "D-1: daemon must reach READY despite the broad-filter CLOSE (got {ready:?})"
        );

        // Give the CLOSED → targeted-only fallback subscribe time to land.
        tokio::time::sleep(Duration::from_secs(1)).await;

        // D-2: publish a FRESH TARGETED offer; assert it is CLAIMED via the targeted-only
        // fallback — proving the seller degraded to targeted-alive instead of going silently deaf.
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        let targeted_id = publish_offer(&seeder, &buyer, &offer_draft(Some(&seller_hex)), None)
            .await
            .to_hex();
        let claimed = wait_until(Duration::from_secs(10), || {
            claims_contain(&claims, &targeted_id, "processing")
        })
        .await;
        assert!(
            claimed,
            "D-2: after the broad-filter CLOSE, the seller must still CLAIM a fresh TARGETED offer \
             (claim-kind processing e={targeted_id}) within 10s via the targeted-only fallback"
        );

        // Quiesce: let execution finish so single-flight is released before the guard drops.
        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    // ── Backfill window: a daemon started AFTER an offer was posted ──────────────────────────

    /// Publish a claim-kind `status=processing` claim for `offer_id` signed by `seller` — models
    /// a DIFFERENT seller having already claimed the offer. Reuses the daemon's own claim draft.
    async fn publish_claim(
        client: &Client,
        seller: &Keys,
        offer_id: &str,
        buyer_pubkey: &str,
    ) -> nostr_sdk::EventId {
        let seller_hex = seller.public_key().to_hex();
        let creq = gateway::creq::build_seller_creq(
            offer_id,
            1,
            "sat",
            &[crate::home::DEFAULT_MINT_URL.to_string()],
            &seller_hex,
        )
        .expect("build claim creq");
        let draft = gateway::claim_draft(offer_id, buyer_pubkey, &seller_hex, &creq);
        let event = gateway::nostr::event_builder(&draft)
            .expect("claim event builder")
            .sign_with_keys(seller)
            .expect("sign claim");
        let id = event.id;
        client.send_event(&event).await.expect("publish claim");
        id
    }

    /// Boot a seller daemon (own thread + readiness hook) with a feedback-kind claim collector.
    /// Returns the live observer client (KEEP it bound — dropping it ends the collector task),
    /// the collected-claims handle, and the daemon join handle. Asserts READY within 10s.
    async fn start_collected_seller(
        label: &str,
        relay_url: &str,
        claim_open_pool: bool,
        offer_backfill_secs: u64,
    ) -> (
        Client,
        Arc<Mutex<Vec<(String, String)>>>,
        std::thread::JoinHandle<()>,
    ) {
        let home = seller_home(&unique_root(label), relay_url, claim_open_pool, offer_backfill_secs);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");

        let observer = connect_client(relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = spawn_daemon_thread(daemon, ready_tx);
        let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "daemon must reach READY within 10s (got {ready:?})"
        );
        (observer, claims, handle)
    }

    /// THE acceptance fixture: post an OPEN-POOL offer, THEN start the daemon, and the daemon
    /// BACKFILLS + CLAIMS it (feedback-kind `processing` e=<offer id>) within the poll interval.
    ///
    /// RED-ON-REVERT (the since-window mechanic): restore `since(now)` / `limit(0)` in
    /// `offer_subscription_filters` (ignore `backfill_secs`) and this fixture goes RED — the
    /// pre-posted offer is never delivered, so never claimed (assert fails, rc=101).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn backfilled_in_window_offer_is_claimed_after_daemon_start() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        // Publish an OPEN-POOL offer BEFORE the daemon exists, dated 30s in the PAST so it is
        // unambiguously earlier than the daemon's subscribe time (deterministic: on the
        // reverted `since(now)` filter it is excluded; within the 20-min window it backfills).
        // Future deadline so it is not expiry-refused. Open-pool is the real field gap.
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        let recent = Timestamp::from(Timestamp::now().as_secs().saturating_sub(30));
        let offer_id = publish_offer(&seeder, &buyer, &offer_draft(None), Some(recent))
            .await
            .to_hex();

        // Start the daemon AFTER the offer is on the relay, with a 20-min backfill window.
        let (_observer, claims, _daemon) =
            start_collected_seller("backfill-in", &relay_url, true, 1200).await;

        let claimed = wait_until(Duration::from_secs(10), || {
            claims_contain(&claims, &offer_id, "processing")
        })
        .await;
        assert!(
            claimed,
            "a daemon started AFTER the offer was posted must BACKFILL + CLAIM it (feedback-kind \
             processing e={offer_id}) within 10s — the start-after-post delivery proof"
        );

        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    /// An offer OLDER than the backfill window is NOT delivered (the relay's `since(now-window)`
    /// bound excludes it) and therefore never claimed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn out_of_window_offer_is_not_claimed() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        // Offer dated an hour ago (future deadline, so ONLY the window — not expiry — excludes it).
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        let past = Timestamp::from(Timestamp::now().as_secs().saturating_sub(3_600));
        let offer_id = publish_offer(&seeder, &buyer, &offer_draft(None), Some(past))
            .await
            .to_hex();

        // 60s window ≪ 1h age ⇒ out of window.
        let (_observer, claims, _daemon) =
            start_collected_seller("backfill-out", &relay_url, true, 60).await;

        let claimed = wait_until(Duration::from_secs(6), || {
            claims_contain(&claims, &offer_id, "processing")
        })
        .await;
        assert!(
            !claimed,
            "an offer older than the backfill window MUST NOT be delivered or claimed (e={offer_id})"
        );

        relay.shutdown();
    }

    /// Money-safety guard #a end-to-end: a backfilled offer WITHIN the window but PAST its own
    /// deadline is delivered, then REFUSED by the offer-freshness gate — never claimed, never
    /// resurrected with a fresh deadline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn backfilled_expired_offer_is_refused_never_claimed() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        let now = Timestamp::now().as_secs();
        // created_at 30s ago (well inside a 300s window) but deadline already lapsed 5s ago.
        let recent = Timestamp::from(now.saturating_sub(30));
        let draft = offer_draft_with_deadline(None, now.saturating_sub(5));
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        let offer_id = publish_offer(&seeder, &buyer, &draft, Some(recent))
            .await
            .to_hex();

        let (_observer, claims, _daemon) =
            start_collected_seller("backfill-exp", &relay_url, true, 300).await;

        let claimed = wait_until(Duration::from_secs(6), || {
            claims_contain(&claims, &offer_id, "processing")
        })
        .await;
        assert!(
            !claimed,
            "a backfilled offer past its deadline MUST be refused (never claimed/resurrected) e={offer_id}"
        );

        relay.shutdown();
    }

    /// Money-safety guard #d end-to-end: a backfilled offer already LIVE-CLAIMED by ANOTHER
    /// seller is skipped — the daemon does not stomp an in-flight trade.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn backfilled_offer_live_claimed_by_another_seller_is_skipped() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        let now = Timestamp::now().as_secs();
        let recent = Timestamp::from(now.saturating_sub(30));
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        // Offer within the window, future deadline (so the foreign claim reads LIVE, not expired).
        let offer_id = publish_offer(&seeder, &buyer, &offer_draft(None), Some(recent))
            .await
            .to_hex();
        // A DIFFERENT seller has already claimed it (live claim-kind processing).
        let foreign_seller = Keys::generate();
        publish_claim(&seeder, &foreign_seller, &offer_id, &buyer.public_key().to_hex()).await;

        let (_observer, claims, _daemon) =
            start_collected_seller("backfill-claimed", &relay_url, true, 300).await;

        // OUR seller (the collector filters to our pubkey) must publish NO processing claim.
        let stomped = wait_until(Duration::from_secs(6), || {
            claims_contain(&claims, &offer_id, "processing")
        })
        .await;
        assert!(
            !stomped,
            "must NOT claim an offer already live-claimed by another seller (no stomping) e={offer_id}"
        );

        relay.shutdown();
    }

    /// Publish a TARGETED contribution offer (job-class + target-repo pin + base + accepts)
    /// signed by `buyer`, returning its event id.
    async fn publish_contribution_offer(
        client: &Client,
        buyer: &Keys,
        seller_hex: &str,
    ) -> nostr_sdk::EventId {
        let offer = OfferDraft::new(
            "improve the forge repo",
            "text/plain",
            10,
            now_unix() + 3_600,
            seller_hex,
        );
        let mut draft = offer.to_event_draft();
        let contribution = crate::contribution::ContributionOffer {
            target: crate::contribution::TargetRepoPin::new(
                "aa".repeat(32),
                "https://mobee-relay.orveth.dev/git/forge/repo.git",
            )
            .unwrap(),
            base: crate::contribution::ContributionBase::new("main", "77".repeat(20)).unwrap(),
            accepts: vec!["fork".into()],
        };
        draft
            .tags
            .extend(crate::contribution::contribution_offer_tags(&contribution));
        let event = gateway::nostr::event_builder(&draft)
            .expect("contribution offer builder")
            .sign_with_keys(buyer)
            .expect("sign contribution offer");
        let id = event.id;
        client.send_event(&event).await.expect("publish contribution offer");
        id
    }

    // ── A CONTRIBUTION offer round-trips over a real relay and the seller recognises it,
    //    claiming it (claim-kind processing) — the offer→claim leg of the ONE state machine live. ──
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn contribution_offer_round_trips_to_claim_over_local_relay() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;
        let home = seller_home(&unique_root("contrib"), &relay_url, false, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");
        let seller_hex = daemon.seller_pubkey().to_string();

        let observer = connect_client(&relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let _daemon = spawn_daemon_thread(daemon, ready_tx);
        let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx.recv()).await;
        assert!(matches!(ready, Ok(Some(()))), "daemon must reach READY (got {ready:?})");

        // Post a TARGETED contribution offer; the seller must recognise the class + pins and CLAIM
        // it (claim-kind processing) — proving the additive offer round-trips over a live relay.
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        let offer_id = publish_contribution_offer(&seeder, &buyer, &seller_hex).await.to_hex();
        let claimed = wait_until(Duration::from_secs(10), || {
            claims_contain(&claims, &offer_id, "processing")
        })
        .await;
        assert!(
            claimed,
            "seller must CLAIM a fresh contribution offer (claim-kind processing e={offer_id}) within 10s"
        );

        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }
}
