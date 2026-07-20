//! Buyer job lifecycle over the mobee relay (kinds 5109 / 7000 / 6109).
//!
//! - [`post_job`] publishes a real kind-5109 offer (targeted p-tag = documented default).
//! - [`get_job`] reads claim/result state from relay events (not local invent).
//! - [`accept_claim`] publishes kind-7000 `accepted` and records a local pay-bind for
//!   [`authorize_pay`](crate::authorize_pay) (seller / result / commit). Claims/results
//!   themselves remain relay-truth.
//!
//! Local bind under `~/.mobee/jobs/<job_id>.json` is accept-state only — Gate D / D2.

use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::gateway::{
    self, accept_draft, parse_git_result_delivery, parse_offer, EventDraft, OfferDraft, TagSpec,
    JOB_FEEDBACK_KIND, JOB_OFFER_KIND, JOB_RESULT_KIND,
};
use crate::home::{self, HomeError, MobeeHome};
#[cfg(feature = "wallet")]
use crate::{buyer_fund, payment_wallet};

const JOBS_DIR: &str = "jobs";
/// Per-relay-fetch budget. Kept well under [`WAIT_FOR_CAP_SECS`] / MCP tool deadline.
const DEFAULT_FETCH_TIMEOUT_SECS: u64 = 5;
/// Cap for `get_job(wait_for=…)` long-poll. Must stay < MCP tool deadline (~15s) so
/// cap-hit returns PENDING for re-poll instead of starving the client read-timeout (~60s).
const WAIT_FOR_CAP_SECS: u64 = 10;
const DEFAULT_DEADLINE_SECS: u64 = 3_600;
/// Derived claim status surfaced when a `processing` claim is past its offer deadline.
/// Never a relay status value — it is computed by [`derive_claim_liveness`] from `now`.
pub const CLAIM_STATUS_EXPIRED: &str = "expired";

/// Inputs for posting a kind-5109 offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostJobRequest {
    pub task: String,
    pub output: String,
    pub amount_sats: u64,
    /// Targeted seller hex pubkey. Required unless `untargeted` is true.
    pub seller_pubkey: Option<String>,
    /// When true, omit the p-tag (open offer). Documented default is targeted.
    pub untargeted: bool,
    pub deadline_unix: Option<u64>,
    /// Optional git delivery bind tags on the offer (repo + branch).
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// Piece-10 contribution-offer params (§ Offer shape). OPTIONAL and **ALL-OR-NOTHING**: if any
    /// of these is present this is a `job-class=contribution` offer and `target_repo_owner`,
    /// `target_repo_url`, `base_branch`, `base_oid` are ALL required; absent entirely ⇒ from-scratch
    /// (no additive tags, byte-identical to a pre-contribution offer). See
    /// [`contribution_offer_from_request`].
    pub target_repo_owner: Option<String>,
    pub target_repo_url: Option<String>,
    pub base_branch: Option<String>,
    pub base_oid: Option<String>,
    /// Positional `accepts` values; defaults to `["fork"]` when contribution params are present.
    /// v1 supports fork only, so a supplied `accepts` MUST include `"fork"`.
    pub accepts: Option<Vec<String>>,
}

/// Outcome of a successful `post_job`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PostJobOutcome {
    pub job_id: String,
    pub job_hash: String,
    pub offer_kind: u16,
    pub targeted: bool,
    pub seller_pubkey: Option<String>,
    pub amount_sats: u64,
    pub relay_url: String,
    pub task: String,
    pub output: String,
}

/// Inputs for reading job state from the relay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetJobRequest {
    pub job_id: String,
    /// Optional long-poll: `claim` or `result`. Preference — not required for freeze.
    pub wait_for: Option<WaitFor>,
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitFor {
    Claim,
    Result,
}

impl WaitFor {
    pub fn parse(raw: &str) -> Result<Self, JobLifecycleError> {
        match raw {
            "claim" => Ok(Self::Claim),
            "result" => Ok(Self::Result),
            other => Err(JobLifecycleError::Input(format!(
                "wait_for must be claim|result, got {other:?}"
            ))),
        }
    }
}

/// Relay-truth view of a job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JobView {
    pub job_id: String,
    pub offer: Option<OfferView>,
    pub claims: Vec<ClaimView>,
    pub results: Vec<ResultView>,
    pub live_claim_id: Option<String>,
    pub accepted: Option<AcceptedBind>,
    /// True when `wait_for` was set and the wait cap hit before the condition —
    /// buyer should re-poll (PENDING), not treat as failure.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OfferView {
    pub event_id: String,
    pub created_at: u64,
    pub author_pubkey: String,
    /// Cosmetic kind-0 `name` for `author_pubkey` (untrusted; never replaces hex).
    pub author_display_name: Option<String>,
    pub task: String,
    pub output: String,
    pub amount_sats: u64,
    pub deadline_unix: u64,
    pub mint_url: String,
    pub seller_pubkey: Option<String>,
    /// Cosmetic kind-0 `name` for targeted `seller_pubkey` (untrusted; never replaces hex).
    pub seller_display_name: Option<String>,
    pub targeted: bool,
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// Raw `job-class` tag value (piece-10). `Some("contribution")` ⇒ a contribution offer; absent
    /// ⇒ from-scratch. Carried raw so a `contribution`-class offer whose pins failed to parse is
    /// visible as `job_class=Some, contribution=None` and REFUSED at accept (never run from-scratch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_class: Option<String>,
    /// Parsed, well-formed contribution pins (target + base + accepts). `None` when not a
    /// contribution OR when a `contribution`-class offer's pins were malformed (fail-closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution: Option<ContributionOfferView>,
}

/// Serializable view of a well-formed contribution offer's pins (piece-10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContributionOfferView {
    pub target_owner_pubkey: String,
    pub target_clone_url: String,
    pub base_branch: String,
    pub base_oid: String,
    pub accepts: Vec<String>,
}

/// Serializable view of a seller result's contribution echo + authorship signature (piece-10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContributionResultView {
    pub target_owner_pubkey: String,
    pub target_clone_url: String,
    pub base_branch: String,
    pub base_oid: String,
    /// Seller schnorr signature (hex) over the signed-6109 authorship tuple (`sig/seller-contribution`).
    pub tuple_signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClaimView {
    pub claim_id: String,
    pub created_at: u64,
    pub seller_pubkey: String,
    /// Cosmetic kind-0 `name` for this claim's `seller_pubkey` (untrusted).
    pub display_name: Option<String>,
    pub status: String,
    pub live: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResultView {
    pub result_id: String,
    pub created_at: u64,
    pub seller_pubkey: String,
    /// Cosmetic kind-0 `name` for this result's `seller_pubkey` (untrusted).
    pub display_name: Option<String>,
    pub job_hash: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub commit_oid: Option<String>,
    pub amount_sats: Option<u64>,
    /// Seller schnorr signature (hex) from the result's `["sig","seller",..]` tag — the
    /// buyer counter-signs the same piece-9 receipt preimage to co-sign the kind-3400.
    pub seller_signature: Option<String>,
    /// Piece-10 contribution echo + authorship signature. `Some` iff the result carries a
    /// well-formed `job-class=contribution` echo AND a `sig/seller-contribution` tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution: Option<ContributionResultView>,
}

/// Local accept-bind recorded by [`accept_claim`] for authorize_pay Gate D / D2.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedBind {
    pub job_id: String,
    pub claim_id: String,
    pub result_id: String,
    pub seller_pubkey: String,
    pub commit_oid: String,
    pub repo: String,
    pub branch: String,
    pub job_hash: String,
    pub amount_sats: u64,
    pub accept_event_id: String,
    pub accepted_at: u64,
    /// Seller schnorr signature (hex) over the receipt preimage, captured from the
    /// accepted result's `sig/seller` tag (piece-9). Empty for legacy/pre-piece-9 results.
    #[serde(default)]
    pub seller_signature: String,
    /// Piece-10 contribution binds, recorded at accept when the OFFER is a contribution (authority
    /// = the buyer's signed offer; the result echo is equality-checked, never trusted). Absent ⇒
    /// from-scratch (EXACTLY today's path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution: Option<AcceptedContribution>,
}

/// Contribution binds captured in the accept-bind (piece-10). `target_*` / `base_*` come from the
/// buyer's SIGNED offer (authority); `tuple_signature` is the seller's signed-6109 authorship sig
/// from the accepted result; `custody_local_ref` is the buyer-controlled ref the fork tip is
/// retained under (MUST-6 custody-retention; merge uses THIS, never the live fork branch).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedContribution {
    pub target_owner_pubkey: String,
    pub target_clone_url: String,
    pub base_branch: String,
    pub base_oid: String,
    pub tuple_signature: String,
    pub custody_local_ref: String,
}

/// Inputs for accepting a seller claim (and binding the matching result).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptClaimRequest {
    pub job_id: String,
    pub claim_id: String,
    /// Optional explicit result id; otherwise the newest git result from the claim seller.
    pub result_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AcceptClaimOutcome {
    pub accept_event_id: String,
    pub bind: AcceptedBind,
}

#[derive(Debug)]
pub enum JobLifecycleError {
    Input(String),
    Home(HomeError),
    Relay(String),
    NotFound(String),
    Targeting(String),
    Io(String),
}

impl fmt::Display for JobLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => write!(formatter, "job lifecycle input: {message}"),
            Self::Home(error) => write!(formatter, "{error}"),
            Self::Relay(message) => write!(formatter, "job lifecycle relay: {message}"),
            Self::NotFound(message) => write!(formatter, "job lifecycle not found: {message}"),
            Self::Targeting(message) => write!(formatter, "job lifecycle targeting: {message}"),
            Self::Io(message) => write!(formatter, "job lifecycle io: {message}"),
        }
    }
}

impl std::error::Error for JobLifecycleError {}

impl From<HomeError> for JobLifecycleError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}

/// Publish a kind-5109 offer to the configured relay. Returns the offer event id as `job_id`.
/// Sync entry for CLI/tests — nested call fails fast; MCP uses [`post_job_async`].
pub fn post_job(home: &MobeeHome, request: PostJobRequest) -> Result<PostJobOutcome, JobLifecycleError> {
    crate::runtime_guard::refuse_nested_block_on("post_job")
        .map_err(JobLifecycleError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JobLifecycleError::Relay(error.to_string()))?;
    runtime.block_on(post_job_async(home, request))
}

/// Async `post_job` for callers already on a Tokio runtime (MCP dispatch).
/// Avoids nested `block_on` when publishing the offer over the relay.
pub async fn post_job_async(
    home: &MobeeHome,
    request: PostJobRequest,
) -> Result<PostJobOutcome, JobLifecycleError> {
    if request.task.trim().is_empty() {
        return Err(JobLifecycleError::Input("task must be non-empty".into()));
    }
    if request.output.trim().is_empty() {
        return Err(JobLifecycleError::Input("output must be non-empty".into()));
    }
    if request.untargeted && request.seller_pubkey.is_some() {
        return Err(JobLifecycleError::Input(
            "untargeted=true cannot also set seller_pubkey".into(),
        ));
    }
    if !request.untargeted && request.seller_pubkey.as_ref().map(|s| s.trim().is_empty()).unwrap_or(true)
    {
        return Err(JobLifecycleError::Input(
            "post_job requires seller_pubkey (targeted default) or untargeted=true".into(),
        ));
    }
    match (&request.repo, &request.branch) {
        (Some(_), None) | (None, Some(_)) => {
            return Err(JobLifecycleError::Input(
                "repo and branch must be supplied together".into(),
            ));
        }
        _ => {}
    }
    // Piece-10: validate the optional contribution params up front (fail-closed, before the wallet
    // opens). `None` ⇒ from-scratch (no additive tags). Emission happens in `build_offer_draft`.
    let contribution = contribution_offer_from_request(&request)?;
    let deadline_unix = resolve_post_deadline(request.deadline_unix, now_unix_secs()?)?;

    // F2 buyer-fix: refuse a post whose amount exceeds the per-job budget cap AT POST — a job you
    // can post but can never pay (authorize_pay refuses at the SAME cap) is a UX trap. Read the
    // cap from the SAME config the budget gate uses (`home.config.per_job_budget_sats`).
    assert_amount_within_budget_cap(request.amount_sats, home.config.per_job_budget_sats)?;

    // Dust guard: live keyset N=1 floor, fail-closed (no hardcoded fee=1).
    #[cfg(feature = "wallet")]
    {
        let wallet = buyer_fund::open_testnut_wallet_async(home)
            .await
            .map_err(|error| JobLifecycleError::Input(error.to_string()))?;
        payment_wallet::require_fee_safe_amount(&wallet, cashu::Amount::from(request.amount_sats))
            .await
            .map_err(|error| JobLifecycleError::Input(error.to_string()))?;
    }

    let draft = build_offer_draft(
        &request,
        &home.config.mint_url,
        deadline_unix,
        contribution.as_ref(),
    )?;

    let keys = buyer_keys(home)?;
    let event_id = publish_draft_async(home, &keys, &draft).await?;
    let job_hash = job_hash_for_offer(&event_id, &request.task, request.amount_sats);

    Ok(PostJobOutcome {
        job_id: event_id,
        job_hash,
        offer_kind: JOB_OFFER_KIND,
        targeted: !request.untargeted,
        seller_pubkey: request.seller_pubkey,
        amount_sats: request.amount_sats,
        relay_url: home.config.relay_url.clone(),
        task: request.task,
        output: request.output,
    })
}

fn now_unix_secs() -> Result<u64, JobLifecycleError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| JobLifecycleError::Input(format!("current unix time unavailable: {error}")))
}

fn resolve_post_deadline(
    deadline_unix: Option<u64>,
    now_unix: u64,
) -> Result<u64, JobLifecycleError> {
    match deadline_unix {
        Some(given) if given <= now_unix => Err(JobLifecycleError::Input(format!(
            "post_job refused: deadline_unix must be greater than current unix time; given={given}, current={now_unix}"
        ))),
        Some(given) => Ok(given),
        None => Ok(now_unix.saturating_add(DEFAULT_DEADLINE_SECS)),
    }
}

/// F2 buyer-fix: refuse a post whose `amount_sats` exceeds the per-job budget cap. Mirrors the
/// budget gate's refuse condition (`amount > per_job_cap`; see [`crate::budget`]) at POST time so a
/// buyer never posts a job that can never be paid. At-cap and under-cap pass unchanged. The message
/// NAMES the config key + both numbers + the remedy; it never auto-raises — the cap is a safety
/// control.
fn assert_amount_within_budget_cap(
    amount_sats: u64,
    per_job_cap: u64,
) -> Result<(), JobLifecycleError> {
    if amount_sats > per_job_cap {
        return Err(JobLifecycleError::Input(format!(
            "post_job refused: amount {amount_sats} sat exceeds the per-job budget cap \
             {per_job_cap} sat (config key `per_job_budget_sats`). A job posted over the cap can \
             never be paid — authorize_pay refuses at the same cap. Raise `per_job_budget_sats` in \
             config.toml and RESTART the process (config is read at startup); the cap is a safety \
             control and is never auto-raised."
        )));
    }
    Ok(())
}

/// Build the kind-5109 offer event draft. The optional git-delivery tags **and** the piece-10
/// contribution tags are emitted HERE so the post path and its round-trip test share ONE
/// tag-emission seam (pure — no publish, no wallet). `contribution` is the pre-validated canonical
/// offer from [`contribution_offer_from_request`]; `None` ⇒ from-scratch, so NO additive
/// contribution tags are emitted (byte-identical to a pre-contribution offer).
fn build_offer_draft(
    request: &PostJobRequest,
    mint_url: &str,
    deadline_unix: u64,
    contribution: Option<&crate::contribution::ContributionOffer>,
) -> Result<EventDraft, JobLifecycleError> {
    let offer = if request.untargeted {
        OfferDraft::untargeted(
            request.task.clone(),
            request.output.clone(),
            request.amount_sats,
            deadline_unix,
            mint_url.to_owned(),
        )
    } else {
        OfferDraft::new(
            request.task.clone(),
            request.output.clone(),
            request.amount_sats,
            deadline_unix,
            mint_url.to_owned(),
            request.seller_pubkey.clone().ok_or_else(|| {
                JobLifecycleError::Input(
                    "post_job requires seller_pubkey (targeted default) or untargeted=true".into(),
                )
            })?,
        )
    };

    let mut draft = offer.to_event_draft();
    if let (Some(repo), Some(branch)) = (&request.repo, &request.branch) {
        draft.tags.push(TagSpec::new(["delivery", "git"]));
        draft.tags.push(TagSpec::new(["repo", repo]));
        draft.tags.push(TagSpec::new(["branch", branch]));
    }
    // Emit contribution tags via the CANONICAL constructor (never hand-rolled) — the buyer offer
    // and the seller echo therefore serialize the same shape.
    if let Some(contribution) = contribution {
        for tag in crate::contribution::contribution_offer_tags(contribution) {
            draft.tags.push(tag);
        }
    }
    Ok(draft)
}

/// Validate the OPTIONAL piece-10 contribution params on a [`PostJobRequest`] and build the
/// canonical [`ContributionOffer`](crate::contribution::ContributionOffer) (§ Offer shape).
///
/// - **Absent entirely** (no owner / url / branch / oid / accepts) ⇒ `Ok(None)` — from-scratch.
/// - **ALL-OR-NOTHING:** if ANY contribution field is present, ALL required (owner, url, branch,
///   oid) MUST be present; a partial set is REFUSED fail-closed, naming every missing field.
/// - When present: owner (64-hex) + branch/oid are validated by the canonical constructors
///   ([`TargetRepoPin`](crate::contribution::TargetRepoPin) /
///   [`ContributionBase`](crate::contribution::ContributionBase)), and the clone URL additionally
///   passes the SAME transport allowlist the pay path fetches under — `ext::`/file/ssh are refused
///   at POST time so a buyer never publishes an offer nobody can safely verify. `accepts` defaults
///   to `["fork"]` and MUST include `"fork"` (v1 fork-only).
fn contribution_offer_from_request(
    request: &PostJobRequest,
) -> Result<Option<crate::contribution::ContributionOffer>, JobLifecycleError> {
    use crate::contribution::{ContributionBase, ContributionOffer, TargetRepoPin, ACCEPTS_FORK};

    let opt_trimmed = |value: &Option<String>| -> Option<String> {
        value
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let owner = opt_trimmed(&request.target_repo_owner);
    let url = opt_trimmed(&request.target_repo_url);
    let branch = opt_trimmed(&request.base_branch);
    let oid = opt_trimmed(&request.base_oid);
    let accepts: Vec<String> = request
        .accepts
        .as_ref()
        .map(|values| {
            values
                .iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // Absent entirely ⇒ from-scratch (byte-identical, no additive tags).
    if owner.is_none() && url.is_none() && branch.is_none() && oid.is_none() && accepts.is_empty() {
        return Ok(None);
    }

    // All-or-nothing: name EVERY missing required field so a partial set fails closed clearly.
    let mut missing = Vec::new();
    if owner.is_none() {
        missing.push("target_repo_owner");
    }
    if url.is_none() {
        missing.push("target_repo_url");
    }
    if branch.is_none() {
        missing.push("base_branch");
    }
    if oid.is_none() {
        missing.push("base_oid");
    }
    if !missing.is_empty() {
        return Err(JobLifecycleError::Input(format!(
            "contribution offer is all-or-nothing: missing required field(s) [{}] (required: \
             target_repo_owner, target_repo_url, base_branch, base_oid)",
            missing.join(", ")
        )));
    }
    let owner = owner.expect("checked present");
    let url = url.expect("checked present");
    let branch = branch.expect("checked present");
    let oid = oid.expect("checked present");

    // Transport allowlist at POST time (https + relay-git only; ext::/file/ssh/local refused) —
    // don't let a buyer publish an offer nobody can safely verify. The pay-path verifier re-checks
    // under the SAME allowlist (defense in depth).
    crate::delivery_transport::assert_allowed_repo_locator(&url).map_err(|refusal| {
        JobLifecycleError::Input(format!("contribution target_repo_url refused: {refusal}"))
    })?;

    let target =
        TargetRepoPin::new(owner, url).map_err(|e| JobLifecycleError::Input(e.to_string()))?;
    let base =
        ContributionBase::new(branch, oid).map_err(|e| JobLifecycleError::Input(e.to_string()))?;
    let accepts = if accepts.is_empty() {
        vec![ACCEPTS_FORK.to_owned()]
    } else {
        accepts
    };
    if !accepts.iter().any(|a| a == ACCEPTS_FORK) {
        return Err(JobLifecycleError::Input(format!(
            "contribution accepts must include \"fork\" (v1 supports fork only); got {accepts:?}"
        )));
    }

    Ok(Some(ContributionOffer {
        target,
        base,
        accepts,
    }))
}

/// Read offer / claims / results from the relay. Local accept-bind is attached if present.
/// Sync entry for CLI/tests — nested call fails fast; MCP uses [`get_job_async`].
pub fn get_job(home: &MobeeHome, request: GetJobRequest) -> Result<JobView, JobLifecycleError> {
    crate::runtime_guard::refuse_nested_block_on("get_job")
        .map_err(JobLifecycleError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JobLifecycleError::Relay(error.to_string()))?;
    runtime.block_on(get_job_async(home, request))
}

/// Async `get_job` for callers already on a Tokio runtime (MCP dispatch).
///
/// `wait_for` is capped at [`WAIT_FOR_CAP_SECS`]. Cap-hit with condition unmet returns
/// `pending: true` (re-poll) — never an error.
pub async fn get_job_async(
    home: &MobeeHome,
    request: GetJobRequest,
) -> Result<JobView, JobLifecycleError> {
    let keys = buyer_keys(home)?;
    let fetch_timeout = Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS);

    let Some(wait_for) = request.wait_for else {
        let mut view =
            fetch_job_view_async(home, &keys, &request.job_id, fetch_timeout, now_unix()).await?;
        view.pending = false;
        return Ok(view);
    };

    let wait_cap_secs = request
        .timeout_secs
        .unwrap_or(WAIT_FOR_CAP_SECS)
        .min(WAIT_FOR_CAP_SECS);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_cap_secs);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let mut view =
                fetch_job_view_async(home, &keys, &request.job_id, fetch_timeout, now_unix())
                    .await?;
            let ready = match wait_for {
                WaitFor::Claim => view.live_claim_id.is_some(),
                WaitFor::Result => !view.results.is_empty(),
            };
            view.pending = !ready;
            return Ok(view);
        }
        let this_fetch = fetch_timeout.min(remaining);
        let mut view =
            fetch_job_view_async(home, &keys, &request.job_id, this_fetch, now_unix()).await?;
        let ready = match wait_for {
            WaitFor::Claim => view.live_claim_id.is_some(),
            WaitFor::Result => !view.results.is_empty(),
        };
        if ready {
            view.pending = false;
            return Ok(view);
        }
        if tokio::time::Instant::now() >= deadline {
            view.pending = true;
            return Ok(view);
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Accept a live claim: publish kind-7000 `accepted` and persist the pay-bind (Gate D).
/// Sync entry for CLI/tests — nested call fails fast; MCP uses [`accept_claim_async`].
pub fn accept_claim(
    home: &MobeeHome,
    request: AcceptClaimRequest,
) -> Result<AcceptClaimOutcome, JobLifecycleError> {
    crate::runtime_guard::refuse_nested_block_on("accept_claim")
        .map_err(JobLifecycleError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JobLifecycleError::Relay(error.to_string()))?;
    runtime.block_on(accept_claim_async(home, request))
}

/// Async `accept_claim` for callers already on a Tokio runtime (MCP dispatch).
pub async fn accept_claim_async(
    home: &MobeeHome,
    request: AcceptClaimRequest,
) -> Result<AcceptClaimOutcome, JobLifecycleError> {
    let timeout = Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS);
    let keys = buyer_keys(home)?;
    // Injected `now` derives expiry: an expired claim surfaces as non-processing and is
    // refused below (cannot accept a claim past its deadline).
    let view = fetch_job_view_async(home, &keys, &request.job_id, timeout, now_unix()).await?;
    let offer = view
        .offer
        .as_ref()
        .ok_or_else(|| JobLifecycleError::NotFound(format!("offer {}", request.job_id)))?;

    let claim = view
        .claims
        .iter()
        .find(|claim| claim.claim_id == request.claim_id)
        .ok_or_else(|| JobLifecycleError::NotFound(format!("claim {}", request.claim_id)))?;
    if claim.status != "processing" {
        return Err(JobLifecycleError::Input(format!(
            "claim {} status is {}, expected processing",
            claim.claim_id, claim.status
        )));
    }

    if let Some(target) = &offer.seller_pubkey {
        if target != &claim.seller_pubkey {
            return Err(JobLifecycleError::Targeting(format!(
                "offer targets seller {target}, claim seller is {}",
                claim.seller_pubkey
            )));
        }
    }

    let result = select_result(&view.results, &claim.seller_pubkey, request.result_id.as_deref())?;
    let repo = result
        .repo
        .clone()
        .ok_or_else(|| JobLifecycleError::Input("result missing repo".into()))?;
    let branch = result
        .branch
        .clone()
        .ok_or_else(|| JobLifecycleError::Input("result missing branch".into()))?;
    let commit_oid = result
        .commit_oid
        .clone()
        .ok_or_else(|| JobLifecycleError::Input("result missing commit_oid".into()))?;
    let job_hash = result
        .job_hash
        .clone()
        .ok_or_else(|| JobLifecycleError::Input("result missing job-hash".into()))?;
    let amount_sats = result.amount_sats.unwrap_or(offer.amount_sats);

    // Piece-10: resolve contribution binds from the buyer's SIGNED OFFER (authority), refusing a
    // malformed contribution offer and equality-checking the seller echo (MUST-4) — never trusting
    // the echo. From-scratch offers (no `job-class=contribution`) leave `contribution = None`.
    let contribution = resolve_accepted_contribution(offer, result, &commit_oid)?;

    let buyer_pubkey = keys.public_key().to_hex();
    let draft = accept_draft(
        &request.job_id,
        &request.claim_id,
        &buyer_pubkey,
        &claim.seller_pubkey,
    );
    let accept_event_id = publish_draft_async(home, &keys, &draft).await?;
    let accepted_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let bind = AcceptedBind {
        job_id: request.job_id.clone(),
        claim_id: request.claim_id.clone(),
        result_id: result.result_id.clone(),
        seller_pubkey: claim.seller_pubkey.clone(),
        commit_oid,
        repo,
        branch,
        job_hash,
        amount_sats,
        accept_event_id: accept_event_id.clone(),
        accepted_at,
        // Capture sig/seller so authorize_pay can co-sign the receipt preimage (piece-9).
        seller_signature: result.seller_signature.clone().unwrap_or_default(),
        contribution,
    };
    write_accepted_bind(home, &bind)?;

    Ok(AcceptClaimOutcome {
        accept_event_id,
        bind,
    })
}

/// Piece-10 accept-time contribution resolution (MUST-4). Authority is the buyer's SIGNED OFFER:
/// - not a contribution offer (`job_class != contribution`) ⇒ `Ok(None)` (from-scratch);
/// - a `contribution`-class offer whose pins failed to parse ⇒ REFUSE (fail-closed — never
///   silently run from-scratch);
/// - a contribution offer whose accepted result carries no valid echo+sig ⇒ REFUSE;
/// - the seller-echoed `{target_repo, base_oid}` are EQUALITY-CHECKED against the offer (a
///   cross-check input, never authority) — a mismatch REFUSES.
///
/// The recorded binds (`target_*`, `base_*`) come from the OFFER; the fork is the result's
/// repo/branch; `custody_local_ref` is derived from the fork-tip `commit_oid`.
fn resolve_accepted_contribution(
    offer: &OfferView,
    result: &ResultView,
    commit_oid: &str,
) -> Result<Option<AcceptedContribution>, JobLifecycleError> {
    use crate::contribution::JOB_CLASS_CONTRIBUTION;
    let offer_contribution = match &offer.contribution {
        Some(c) => c,
        None => {
            // Fail-closed: a contribution-class offer whose pins didn't parse must NOT run as
            // from-scratch. Only a genuinely non-contribution offer resolves to None.
            if offer.job_class.as_deref() == Some(JOB_CLASS_CONTRIBUTION) {
                return Err(JobLifecycleError::Input(
                    "offer is job-class=contribution but its target-repo/base pins are malformed — \
                     refused (a malformed contribution offer is never run as from-scratch)"
                        .into(),
                ));
            }
            return Ok(None);
        }
    };
    let echo = result.contribution.as_ref().ok_or_else(|| {
        JobLifecycleError::Input(
            "contribution offer requires a contribution result (job-class echo + \
             sig/seller-contribution); the accepted result carries none — refused"
                .into(),
        )
    })?;
    // MUST-4 equality-check: seller-echoed target/base MUST equal the buyer's signed offer.
    if echo.target_owner_pubkey != offer_contribution.target_owner_pubkey
        || echo.target_clone_url != offer_contribution.target_clone_url
    {
        return Err(JobLifecycleError::Targeting(format!(
            "contribution result echoes target-repo (owner {}, {}) but the signed offer pins \
             (owner {}, {}) — echo mismatch refused (base/target resolved from the PIN, never the echo)",
            echo.target_owner_pubkey,
            echo.target_clone_url,
            offer_contribution.target_owner_pubkey,
            offer_contribution.target_clone_url
        )));
    }
    if echo.base_branch != offer_contribution.base_branch
        || echo.base_oid != offer_contribution.base_oid
    {
        return Err(JobLifecycleError::Targeting(format!(
            "contribution result echoes base ({}, {}) but the signed offer pins ({}, {}) — echo \
             mismatch refused",
            echo.base_branch, echo.base_oid, offer_contribution.base_branch, offer_contribution.base_oid
        )));
    }
    Ok(Some(AcceptedContribution {
        // Authority = the OFFER (buyer-signed), never the echo.
        target_owner_pubkey: offer_contribution.target_owner_pubkey.clone(),
        target_clone_url: offer_contribution.target_clone_url.clone(),
        base_branch: offer_contribution.base_branch.clone(),
        base_oid: offer_contribution.base_oid.clone(),
        tuple_signature: echo.tuple_signature.clone(),
        custody_local_ref: crate::delivery_git::PayPathDeliveryVerifier::custody_ref_for(commit_oid),
    }))
}

/// Load the local accept-bind for a job, if any.
pub fn load_accepted_bind(
    home: &MobeeHome,
    job_id: &str,
) -> Result<Option<AcceptedBind>, JobLifecycleError> {
    let path = bind_path(home, job_id);
    if !path.is_file() {
        return Ok(None);
    }
    let mut file = File::open(&path).map_err(|error| JobLifecycleError::Io(error.to_string()))?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|error| JobLifecycleError::Io(error.to_string()))?;
    let bind: AcceptedBind = serde_json::from_str(&raw)
        .map_err(|error| JobLifecycleError::Io(format!("accept bind parse: {error}")))?;
    Ok(Some(bind))
}

/// Refuse authorize_pay fields that disagree with a recorded accept-bind (Gate D).
pub fn assert_authorize_matches_bind(
    bind: &AcceptedBind,
    seller_pubkey: &str,
    result_id: &str,
    commit_oid: &str,
) -> Result<(), JobLifecycleError> {
    if seller_pubkey != bind.seller_pubkey {
        return Err(JobLifecycleError::Targeting(format!(
            "authorize_pay seller_pubkey {} does not match accepted seller {}",
            seller_pubkey, bind.seller_pubkey
        )));
    }
    if result_id != bind.result_id {
        return Err(JobLifecycleError::Targeting(format!(
            "authorize_pay result_id {} does not match accepted result {}",
            result_id, bind.result_id
        )));
    }
    if commit_oid != bind.commit_oid {
        return Err(JobLifecycleError::Targeting(format!(
            "authorize_pay commit_oid {} does not match accepted commit {}",
            commit_oid, bind.commit_oid
        )));
    }
    Ok(())
}

/// Build an [`AuthorizePayRequest`](crate::authorize_pay::AuthorizePayRequest) from the
/// accept-bind + buyer-supplied tip-match (D2).
///
/// D2 rules:
/// - `delivery_integrity_hash` is a **required** buyer arg (never defaulted/derived from
///   claim 7000 or result 6109 oid).
/// - Compare it to the seller's advertised `commit_oid` and **refuse on mismatch**.
/// - Matching is fine when the buyer independently tip-matched the same oid; auto-fill
///   from the seller advertisement is the circular-bind failure mode.
pub fn authorize_request_from_bind(
    bind: &AcceptedBind,
    amount_sats: u64,
    delivery_integrity_hash: String,
) -> Result<crate::authorize_pay::AuthorizePayRequest, JobLifecycleError> {
    if delivery_integrity_hash.trim().is_empty() {
        return Err(JobLifecycleError::Input(
            "authorize_pay(job_id) requires buyer-supplied delivery_integrity_hash (tip-match); never auto-filled from claim oid".into(),
        ));
    }
    if delivery_integrity_hash != bind.commit_oid {
        return Err(JobLifecycleError::Targeting(format!(
            "authorize_pay(job_id) delivery_integrity_hash {} does not match accepted seller commit_oid {} (buyer tip-match required; refuse mismatch)",
            delivery_integrity_hash, bind.commit_oid
        )));
    }
    Ok(crate::authorize_pay::AuthorizePayRequest {
        job_id: bind.job_id.clone(),
        result_id: bind.result_id.clone(),
        delivery_integrity_hash,
        job_hash: bind.job_hash.clone(),
        seller_pubkey: bind.seller_pubkey.clone(),
        amount_sats,
        repo: bind.repo.clone(),
        branch: bind.branch.clone(),
        commit_oid: bind.commit_oid.clone(),
        seller_signature: bind.seller_signature.clone(),
        // Piece-10: thread the contribution binds so authorize_pay runs the contribution
        // verify-path + authorship seam. `None` ⇒ from-scratch (today's path).
        contribution: bind.contribution.as_ref().map(|c| {
            crate::authorize_pay::ContributionPayBinds {
                target_owner_pubkey: c.target_owner_pubkey.clone(),
                target_clone_url: c.target_clone_url.clone(),
                base_branch: c.base_branch.clone(),
                base_oid: c.base_oid.clone(),
                tuple_signature: c.tuple_signature.clone(),
            }
        }),
    })
}

fn write_accepted_bind(home: &MobeeHome, bind: &AcceptedBind) -> Result<(), JobLifecycleError> {
    let dir = home.root.join(JOBS_DIR);
    fs::create_dir_all(&dir).map_err(|error| JobLifecycleError::Io(error.to_string()))?;
    let path = bind_path(home, &bind.job_id);
    let raw = serde_json::to_string_pretty(bind)
        .map_err(|error| JobLifecycleError::Io(format!("accept bind encode: {error}")))?;
    let mut file = File::create(&path).map_err(|error| JobLifecycleError::Io(error.to_string()))?;
    file.write_all(raw.as_bytes())
        .map_err(|error| JobLifecycleError::Io(error.to_string()))?;
    Ok(())
}

fn bind_path(home: &MobeeHome, job_id: &str) -> PathBuf {
    // Event ids are hex — safe as a single path segment.
    home.root.join(JOBS_DIR).join(format!("{job_id}.json"))
}

/// Canonical job-hash for offer/result signing (buyer + seller share this).
pub fn job_hash_for_offer(job_id: &str, task: &str, amount_sats: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(job_id.as_bytes());
    hasher.update(b"|");
    hasher.update(task.as_bytes());
    hasher.update(b"|");
    hasher.update(amount_sats.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

fn buyer_keys(home: &MobeeHome) -> Result<nostr_sdk::Keys, JobLifecycleError> {
    let secret = home::read_secret_key_hex(home)?;
    nostr_sdk::Keys::parse(&secret)
        .map_err(|error| JobLifecycleError::Home(HomeError::Key(format!("buyer key parse: {error}"))))
}

#[allow(dead_code)] // guarded sync twin for non-async callers; MCP uses `_async`
fn publish_draft(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    draft: &EventDraft,
) -> Result<String, JobLifecycleError> {
    crate::runtime_guard::refuse_nested_block_on("publish_draft")
        .map_err(JobLifecycleError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JobLifecycleError::Relay(error.to_string()))?;
    runtime.block_on(publish_draft_async(home, keys, draft))
}

async fn publish_draft_async(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    draft: &EventDraft,
) -> Result<String, JobLifecycleError> {
    use nostr_sdk::prelude::{Client, Kind};

    let builder = gateway::nostr::event_builder(draft)
        .map_err(|error| JobLifecycleError::Relay(format!("event builder: {error}")))?;
    let event = builder
        .sign_with_keys(keys)
        .map_err(|error| JobLifecycleError::Relay(format!("sign offer: {error}")))?;
    // Keep Kind::Custom visible for readers of the draft path.
    let _ = Kind::Custom(draft.kind);

    let client = Client::new(keys.clone());
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("add relay: {error}")))?;
    client.connect().await;
    let output = client
        .send_event_to([&home.config.relay_url], &event)
        .await;
    client.disconnect().await;
    let output = output.map_err(|error| JobLifecycleError::Relay(format!("send event: {error}")))?;
    if output.success.is_empty() {
        let failed: Vec<String> = output
            .failed
            .into_iter()
            .map(|(url, err)| format!("{url}: {err}"))
            .collect();
        return Err(JobLifecycleError::Relay(format!(
            "no relay accepted event ({})",
            failed.join("; ")
        )));
    }
    Ok(output.val.to_hex())
}

#[allow(dead_code)] // guarded sync twin for non-async callers; MCP uses `_async`
fn fetch_job_view(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    job_id: &str,
    timeout: Duration,
) -> Result<JobView, JobLifecycleError> {
    crate::runtime_guard::refuse_nested_block_on("fetch_job_view")
        .map_err(JobLifecycleError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JobLifecycleError::Relay(error.to_string()))?;
    runtime.block_on(fetch_job_view_async(home, keys, job_id, timeout, now_unix()))
}

/// Current unix time (seconds). Wall-clock lives ONLY at call sites; the derivation
/// ([`derive_claim_liveness`]) takes `now` as input so the pure path stays testable.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Derive claim liveness from status + offer deadline + the injected `now`.
///
/// A `processing` claim whose offer deadline has passed (`now > deadline`) is EXPIRED:
/// its status is surfaced as [`CLAIM_STATUS_EXPIRED`], it is marked `live = false`, and it
/// is excluded from `live_claim_id`. Expiry is DERIVED — never stored, never read from the
/// wall clock inside this function (tests pass a fixed `now`). `claims` must be pre-sorted
/// newest-first; the newest still-`processing` claim becomes the live one.
///
/// `offer_deadline_unix == None` (offer not yet on the relay) means expiry cannot be derived,
/// so status-based liveness is preserved unchanged.
fn derive_claim_liveness(
    claims: &mut [ClaimView],
    offer_deadline_unix: Option<u64>,
    now: u64,
) -> Option<String> {
    if let Some(deadline) = offer_deadline_unix {
        for claim in claims.iter_mut() {
            if claim.status == "processing" && now > deadline {
                claim.status = CLAIM_STATUS_EXPIRED.to_string();
            }
        }
    }
    let live_claim_id = claims
        .iter()
        .find(|claim| claim.status == "processing")
        .map(|claim| claim.claim_id.clone());
    for claim in claims.iter_mut() {
        claim.live = live_claim_id.as_deref() == Some(claim.claim_id.as_str());
    }
    live_claim_id
}

/// Read one job's offer + claims + results from the relay, with claim liveness derived
/// against `now` (a `processing` claim past the offer deadline is EXPIRED, not live). Exposed
/// `pub(crate)` so the seller daemon can run the backfill money-safety pre-claim check
/// (already-delivered / live-claimed-by-another) without duplicating the relay read.
pub(crate) async fn fetch_job_view_async(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    job_id: &str,
    timeout: Duration,
    now: u64,
) -> Result<JobView, JobLifecycleError> {
    use nostr_sdk::prelude::{Client, EventId, Filter, Kind};

    let offer_id = EventId::from_hex(job_id)
        .map_err(|error| JobLifecycleError::Input(format!("job_id: {error}")))?;

    let client = Client::new(keys.clone());
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("add relay: {error}")))?;
    client.connect().await;

    let offer_filter = Filter::new().id(offer_id).kind(Kind::Custom(JOB_OFFER_KIND));
    // Kind + #e is enough to gather marketplace traffic; t=mobee is on drafts we publish.
    let feedback_filter = Filter::new()
        .kind(Kind::Custom(JOB_FEEDBACK_KIND))
        .event(offer_id);
    let result_filter = Filter::new()
        .kind(Kind::Custom(JOB_RESULT_KIND))
        .event(offer_id);

    let offer_events = client
        .fetch_events(offer_filter, timeout)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("fetch offer: {error}")))?;
    let feedback_events = client
        .fetch_events(feedback_filter, timeout)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("fetch feedback: {error}")))?;
    let result_events = client
        .fetch_events(result_filter, timeout)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("fetch results: {error}")))?;
    client.disconnect().await;

    let offer = offer_events.into_iter().next().map(|event| {
        let draft = event_to_draft(&event);
        let parsed = parse_offer(&draft).ok();
        OfferView {
            event_id: event.id.to_hex(),
            created_at: event.created_at.as_secs(),
            author_pubkey: event.pubkey.to_hex(),
            author_display_name: None,
            task: parsed
                .as_ref()
                .map(|p| p.task.clone())
                .unwrap_or_default(),
            output: parsed
                .as_ref()
                .map(|p| p.output.clone())
                .unwrap_or_default(),
            amount_sats: parsed.as_ref().map(|p| p.amount).unwrap_or(0),
            deadline_unix: parsed.as_ref().map(|p| p.deadline_unix).unwrap_or(0),
            mint_url: parsed
                .as_ref()
                .map(|p| p.mint_url.clone())
                .unwrap_or_default(),
            seller_pubkey: parsed.as_ref().and_then(|p| p.seller_pubkey.clone()),
            seller_display_name: None,
            targeted: parsed.as_ref().map(|p| p.is_targeted()).unwrap_or(false),
            repo: first_tag_value(&draft.tags, "repo").map(str::to_owned),
            branch: first_tag_value(&draft.tags, "branch").map(str::to_owned),
            // Piece-10: raw job-class + parsed pins. A malformed contribution offer parses to
            // `contribution=None` while `job_class=Some("contribution")` — accept refuses it
            // (fail-closed; never silently from-scratch).
            job_class: first_tag_value(&draft.tags, crate::contribution::TAG_JOB_CLASS)
                .map(str::to_owned),
            contribution: contribution_offer_view(&draft.tags),
        }
    });

    let mut claims = Vec::new();
    for event in feedback_events {
        let draft = event_to_draft(&event);
        let status = first_tag_value(&draft.tags, "status")
            .unwrap_or("")
            .to_owned();
        if status != "processing" && status != "error" {
            // accepts are buyer-authored; skip for claim list
            continue;
        }
        claims.push(ClaimView {
            claim_id: event.id.to_hex(),
            created_at: event.created_at.as_secs(),
            seller_pubkey: event.pubkey.to_hex(),
            display_name: None,
            status,
            live: false,
        });
    }
    claims.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    // Liveness is DERIVED from `now` vs the offer deadline — a processing claim past its
    // deadline is EXPIRED and must not read live (piece-11 behavior 3; job 0867a213).
    let offer_deadline_unix = offer.as_ref().map(|o| o.deadline_unix);
    let live_claim_id = derive_claim_liveness(&mut claims, offer_deadline_unix, now);

    let mut results = Vec::new();
    for event in result_events {
        let draft = event_to_draft(&event);
        let delivery = parse_git_result_delivery(&draft).ok();
        let amount_sats = first_tag(&draft.tags, "amount")
            .and_then(|tag| tag.0.get(1))
            .and_then(|value| value.parse().ok());
        results.push(ResultView {
            result_id: event.id.to_hex(),
            created_at: event.created_at.as_secs(),
            seller_pubkey: event.pubkey.to_hex(),
            display_name: None,
            job_hash: first_tag_value(&draft.tags, "job-hash").map(str::to_owned),
            repo: delivery.as_ref().map(|d| d.repo().to_owned()),
            branch: delivery.as_ref().map(|d| d.branch().to_owned()),
            commit_oid: delivery
                .as_ref()
                .map(|d| d.commit_oid().as_str().to_owned()),
            amount_sats,
            seller_signature: sig_seller_value(&draft.tags),
            contribution: contribution_result_view(&draft.tags),
        });
    }
    results.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    let accepted = load_accepted_bind(home, job_id)?;

    let mut view = JobView {
        job_id: job_id.to_owned(),
        offer,
        claims,
        results,
        live_claim_id,
        accepted,
        pending: false,
    };
    attach_display_names_async(home, &mut view).await;
    Ok(view)
}

/// Collect hex pubkeys for cosmetic kind-0 enrichment (never for pay/targeting).
fn display_name_pubkeys(view: &JobView) -> Vec<String> {
    let mut pubkeys: Vec<String> = Vec::new();
    if let Some(offer) = &view.offer {
        pubkeys.push(offer.author_pubkey.clone());
        if let Some(seller) = &offer.seller_pubkey {
            pubkeys.push(seller.clone());
        }
    }
    for claim in &view.claims {
        pubkeys.push(claim.seller_pubkey.clone());
    }
    for result in &view.results {
        pubkeys.push(result.seller_pubkey.clone());
    }
    pubkeys
}

fn apply_display_names(view: &mut JobView, names: &std::collections::HashMap<String, Option<String>>) {
    let lookup = |hex: &str| -> Option<String> {
        names
            .get(&hex.to_ascii_lowercase())
            .and_then(|value| value.clone())
    };

    if let Some(offer) = &mut view.offer {
        offer.author_display_name = lookup(&offer.author_pubkey);
        offer.seller_display_name = offer
            .seller_pubkey
            .as_ref()
            .and_then(|seller| lookup(seller));
    }
    for claim in &mut view.claims {
        claim.display_name = lookup(&claim.seller_pubkey);
    }
    for result in &mut view.results {
        result.display_name = lookup(&result.seller_pubkey);
    }
}

/// Cosmetic kind-0 enrichment only — never feeds accept-bind / targeting / pay.
/// Async so `get_job`'s existing runtime does not nest `block_on` (panic).
async fn attach_display_names_async(home: &MobeeHome, view: &mut JobView) {
    let pubkeys = display_name_pubkeys(view);
    let mut unique = std::collections::HashSet::new();
    for key in pubkeys {
        let hex = key.trim().to_ascii_lowercase();
        if hex.len() == 64 && hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
            unique.insert(hex);
        }
    }
    if unique.is_empty() {
        return;
    }
    let names = match crate::profile::fetch_names_async(home, &unique).await {
        Ok(map) => map,
        Err(_) => unique.into_iter().map(|k| (k, None)).collect(),
    };
    apply_display_names(view, &names);
}

fn select_result<'a>(
    results: &'a [ResultView],
    seller_pubkey: &str,
    result_id: Option<&str>,
) -> Result<&'a ResultView, JobLifecycleError> {
    if let Some(id) = result_id {
        let result = results
            .iter()
            .find(|result| result.result_id == id)
            .ok_or_else(|| JobLifecycleError::NotFound(format!("result {id}")))?;
        // CROSS-BIND TOOTH: a result is bindable to this claim ONLY if the result's author
        // (its kind-6109 event pubkey) IS the claim seller. NEVER trust an operator-supplied
        // `result_id` to override this — accepting seller A's claim with seller B's result is
        // the live 21-sat cross-bind (the buyer pays A, who is p2pk-locked into the token, for
        // B's artifact). The `result_id == None` branch below already author-filters; this
        // closes the explicit-id hole. Refuse naming BOTH public keys (public keys only).
        if result.seller_pubkey != seller_pubkey {
            return Err(JobLifecycleError::Targeting(format!(
                "result {id} is authored by seller {} but the accepted claim's seller is {} — \
                 cross-authored result refused (the buyer must not pay one seller for another \
                 seller's result)",
                result.seller_pubkey, seller_pubkey
            )));
        }
        return Ok(result);
    }
    results
        .iter()
        .find(|result| result.seller_pubkey == seller_pubkey && result.commit_oid.is_some())
        .ok_or_else(|| {
            JobLifecycleError::NotFound(format!(
                "no git result from seller {seller_pubkey} for this job"
            ))
        })
}

/// Convert a relay event into an [`EventDraft`] (tag/content only — no secrets).
pub fn event_to_draft(event: &nostr_sdk::Event) -> EventDraft {
    let tags = event
        .tags
        .iter()
        .map(|tag| TagSpec(tag.as_slice().to_vec()))
        .collect();
    EventDraft::new(event.kind.as_u16(), tags, event.content.clone())
}

fn first_tag<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a TagSpec> {
    tags.iter()
        .find(|tag| tag.0.first().map(String::as_str) == Some(name))
}

fn first_tag_value<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a str> {
    first_tag(tags, name).and_then(TagSpec::value)
}

/// Parse a well-formed contribution offer's pins into a serializable view. A malformed
/// `contribution`-class offer yields `None` (surfaced as `job_class=Some, contribution=None`, which
/// accept refuses — fail-closed, never run from-scratch).
fn contribution_offer_view(tags: &[TagSpec]) -> Option<ContributionOfferView> {
    match crate::contribution::parse_contribution_offer(tags) {
        Ok(Some(offer)) => Some(ContributionOfferView {
            target_owner_pubkey: offer.target.owner_pubkey().to_owned(),
            target_clone_url: offer.target.clone_url().to_owned(),
            base_branch: offer.base.branch().to_owned(),
            base_oid: offer.base.oid().to_owned(),
            accepts: offer.accepts,
        }),
        _ => None,
    }
}

/// Parse a seller result's contribution echo + authorship signature into a serializable view.
fn contribution_result_view(tags: &[TagSpec]) -> Option<ContributionResultView> {
    match crate::contribution::parse_contribution_result_echo(tags) {
        Ok(Some((echo, tuple_signature))) => Some(ContributionResultView {
            target_owner_pubkey: echo.target.owner_pubkey().to_owned(),
            target_clone_url: echo.target.clone_url().to_owned(),
            base_branch: echo.base.branch().to_owned(),
            base_oid: echo.base.oid().to_owned(),
            tuple_signature,
        }),
        _ => None,
    }
}

/// Value of the `["sig","seller",<hex>]` tag, if present (piece-9 co-signature capture).
fn sig_seller_value(tags: &[TagSpec]) -> Option<String> {
    tags.iter()
        .find(|tag| {
            tag.0.first().map(String::as_str) == Some("sig")
                && tag.0.get(1).map(String::as_str) == Some("seller")
        })
        .and_then(|tag| tag.0.get(2))
        .map(String::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home;

    #[test]
    fn accept_bind_round_trips_on_disk() {
        let root = std::env::temp_dir().join(format!(
            "mobee-jobs-bind-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 1,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            contribution: None,
        };
        write_accepted_bind(&home, &bind).expect("write");
        let loaded = load_accepted_bind(&home, &bind.job_id)
            .expect("load")
            .expect("present");
        assert_eq!(loaded, bind);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn authorize_from_bind_requires_buyer_tip_match_hash() {
        let bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 1,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            contribution: None,
        };
        let err = authorize_request_from_bind(&bind, 1, String::new()).expect_err("empty hash");
        assert!(err.to_string().contains("delivery_integrity_hash"));

        // D2: buyer-supplied hash that disagrees with seller advertised commit_oid → refuse.
        let mismatch =
            authorize_request_from_bind(&bind, 1, "aa".repeat(20)).expect_err("mismatch");
        assert!(
            mismatch.to_string().contains("does not match accepted seller commit_oid"),
            "got: {mismatch}"
        );

        // Matching is allowed only when the buyer independently supplies that oid.
        let req = authorize_request_from_bind(&bind, 1, bind.commit_oid.clone()).expect("ok");
        assert_eq!(req.delivery_integrity_hash, bind.commit_oid);
        assert_eq!(req.seller_pubkey, bind.seller_pubkey);
        assert_eq!(req.commit_oid, bind.commit_oid);
    }

    #[test]
    fn assert_authorize_matches_bind_refuses_seller_mismatch() {
        let bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 1,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            contribution: None,
        };
        let bad_seller = "00".repeat(32);
        let err = assert_authorize_matches_bind(&bind, &bad_seller, &bind.result_id, &bind.commit_oid)
            .expect_err("mismatch");
        assert!(err.to_string().contains("seller"));
    }

    fn result_view(result_id: &str, seller_pubkey: &str) -> ResultView {
        ResultView {
            result_id: result_id.to_owned(),
            created_at: 100,
            seller_pubkey: seller_pubkey.to_owned(),
            display_name: None,
            job_hash: Some("ff".repeat(32)),
            repo: Some("https://github.com/bitcoin/bips.git".into()),
            branch: Some("master".into()),
            commit_oid: Some("ee".repeat(20)),
            amount_sats: Some(1),
            seller_signature: Some("ab".repeat(64)),
            contribution: None,
        }
    }

    // CROSS-BIND TOOTH (accept path): an explicit `result_id` authored by a DIFFERENT seller
    // than the accepted claim's seller is REFUSED (the tool must not trust operator input) —
    // the live 21-sat cross-bind fixture shape (claim A + result B). An own-authored result,
    // selected explicitly OR auto, is unchanged.
    #[test]
    fn select_result_refuses_cross_authored_explicit_result_id() {
        let seller_a = "aa".repeat(32);
        let seller_b = "bb".repeat(32);
        let results = vec![
            result_view("result-b", &seller_b),
            result_view("result-a", &seller_a),
        ];

        // Claim seller A, explicit result authored by B → refuse, naming BOTH pubkeys.
        let err = select_result(&results, &seller_a, Some("result-b"))
            .expect_err("cross-authored explicit result_id must refuse");
        let message = err.to_string();
        assert!(
            message.contains(&seller_a) && message.contains(&seller_b),
            "refusal must name both the claim seller and the result author: {message}"
        );
        assert!(
            message.contains("cross-authored"),
            "refusal must be a clear cross-authored refusal: {message}"
        );

        // A's own result, selected explicitly → accepted unchanged.
        let own = select_result(&results, &seller_a, Some("result-a")).expect("own result ok");
        assert_eq!(own.result_id, "result-a");
        assert_eq!(own.seller_pubkey, seller_a);

        // Auto-select (no explicit id) → author-filtered to A, unchanged.
        let auto = select_result(&results, &seller_a, None).expect("auto own result ok");
        assert_eq!(auto.seller_pubkey, seller_a);
    }

    fn claim_view(claim_id: &str, created_at: u64, status: &str) -> ClaimView {
        ClaimView {
            claim_id: claim_id.to_owned(),
            created_at,
            seller_pubkey: "dd".repeat(32),
            display_name: None,
            status: status.to_owned(),
            live: false,
        }
    }

    // Behavior 3: a processing claim past its offer deadline surfaces as EXPIRED and is not
    // live. REAL claim/deadline path — a fixed `now` (injected), no relay, no wall-clock.
    #[test]
    fn processing_claim_past_deadline_is_expired_not_live() {
        let deadline = 1_700_000_000u64;
        let mut claims = vec![claim_view("orphan-claim", 100, "processing")];
        // now well past the deadline (the 0867a213 shape: still "processing" 25 min later).
        let live = derive_claim_liveness(&mut claims, Some(deadline), deadline + 1_500);
        assert_eq!(live, None, "an expired claim must never be the live claim");
        assert_eq!(
            claims[0].status, CLAIM_STATUS_EXPIRED,
            "past-deadline processing claim must surface as EXPIRED"
        );
        assert!(!claims[0].live, "expired claim must not read live/processing");
    }

    #[test]
    fn processing_claim_before_deadline_is_live_newest_wins() {
        let deadline = 1_700_000_000u64;
        let mut claims = vec![
            claim_view("newest", 200, "processing"),
            claim_view("older", 100, "processing"),
        ];
        let live = derive_claim_liveness(&mut claims, Some(deadline), deadline - 10);
        assert_eq!(live.as_deref(), Some("newest"), "newest processing claim is live");
        assert!(claims[0].live && !claims[1].live);
        assert_eq!(claims[0].status, "processing", "not expired before the deadline");
    }

    // The SAME fixture flips live→expired purely by advancing the injected `now` — proves
    // expiry is derived from `now`, never stored (and that `now` is load-bearing input).
    #[test]
    fn liveness_flips_with_injected_now_only() {
        let deadline = 1_700_000_000u64;
        let make = || vec![claim_view("c1", 100, "processing")];

        let mut before = make();
        let live_before = derive_claim_liveness(&mut before, Some(deadline), deadline - 1);
        assert_eq!(live_before.as_deref(), Some("c1"));
        assert!(before[0].live && before[0].status == "processing");

        let mut after = make();
        let live_after = derive_claim_liveness(&mut after, Some(deadline), deadline + 1);
        assert_eq!(live_after, None);
        assert!(!after[0].live && after[0].status == CLAIM_STATUS_EXPIRED);
    }

    // No offer deadline known ⇒ expiry cannot be derived; status-based liveness preserved.
    // An `error` claim is never live regardless.
    #[test]
    fn no_deadline_preserves_status_and_error_never_live() {
        let mut claims = vec![
            claim_view("proc", 200, "processing"),
            claim_view("err", 100, "error"),
        ];
        let live = derive_claim_liveness(&mut claims, None, 9_999_999_999);
        assert_eq!(live.as_deref(), Some("proc"));
        assert!(claims[0].live, "processing claim stays live when no deadline is known");
        assert!(!claims[1].live, "error claim is never live");
        assert_eq!(claims[0].status, "processing");
    }

    #[test]
    fn post_job_refuses_missing_seller_without_untargeted() {
        let root = std::env::temp_dir().join(format!(
            "mobee-jobs-post-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let err = post_job(
            &home,
            PostJobRequest {
                task: "t".into(),
                output: "text/plain".into(),
                amount_sats: 1,
                seller_pubkey: None,
                untargeted: false,
                deadline_unix: Some(1_800_000_000),
                repo: None,
                branch: None,
                target_repo_owner: None,
                target_repo_url: None,
                base_branch: None,
                base_oid: None,
                accepts: None,
            },
        )
        .expect_err("seller required");
        assert!(err.to_string().contains("seller_pubkey"));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn temp_job_home(label: &str) -> (std::path::PathBuf, crate::home::MobeeHome) {
        let root = std::env::temp_dir().join(format!(
            "mobee-jobs-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        (root, home)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_job_sync_refuses_inside_runtime() {
        let (root, home) = temp_job_home("nested-post");
        let err = post_job(
            &home,
            PostJobRequest {
                task: "t".into(),
                output: "text/plain".into(),
                amount_sats: 1,
                seller_pubkey: Some("aa".repeat(32)),
                untargeted: false,
                deadline_unix: Some(1_800_000_000),
                repo: None,
                branch: None,
                target_repo_owner: None,
                target_repo_url: None,
                base_branch: None,
                base_oid: None,
                accepts: None,
            },
        )
        .expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_job_sync_refuses_inside_runtime() {
        let (root, home) = temp_job_home("nested-get");
        let err = get_job(
            &home,
            GetJobRequest {
                job_id: "aa".repeat(32),
                wait_for: None,
                timeout_secs: None,
            },
        )
        .expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accept_claim_sync_refuses_inside_runtime() {
        let (root, home) = temp_job_home("nested-accept");
        let err = accept_claim(
            &home,
            AcceptClaimRequest {
                job_id: "aa".repeat(32),
                claim_id: "bb".repeat(32),
                result_id: None,
            },
        )
        .expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publish_draft_sync_refuses_inside_runtime() {
        let (root, home) = temp_job_home("nested-publish-draft");
        let keys = nostr_sdk::Keys::generate();
        let draft = EventDraft::new(5109, Vec::new(), "nested-guard");
        let err = publish_draft(&home, &keys, &draft).expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        assert!(
            err.to_string().contains("publish_draft"),
            "op name missing: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_job_view_sync_refuses_inside_runtime() {
        let (root, home) = temp_job_home("nested-fetch-job");
        let keys = nostr_sdk::Keys::generate();
        let err = fetch_job_view(&home, &keys, &"aa".repeat(32), Duration::from_secs(1))
            .expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        assert!(
            err.to_string().contains("fetch_job_view"),
            "op name missing: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Piece-10 accept-path: contribution resolution (MUST-4 echo-equality + fail-closed) ─────
    fn offer_view_contribution(owner: &str, url: &str, base_branch: &str, base_oid: &str) -> OfferView {
        OfferView {
            event_id: "of".repeat(16),
            created_at: 1,
            author_pubkey: "aa".repeat(32),
            author_display_name: None,
            task: "t".into(),
            output: "o".into(),
            amount_sats: 1,
            deadline_unix: 10,
            mint_url: "https://testnut.cashudevkit.org".into(),
            seller_pubkey: None,
            seller_display_name: None,
            targeted: false,
            repo: None,
            branch: None,
            job_class: Some(crate::contribution::JOB_CLASS_CONTRIBUTION.to_owned()),
            contribution: Some(ContributionOfferView {
                target_owner_pubkey: owner.to_owned(),
                target_clone_url: url.to_owned(),
                base_branch: base_branch.to_owned(),
                base_oid: base_oid.to_owned(),
                accepts: vec!["fork".into()],
            }),
        }
    }

    fn result_view_contribution(owner: &str, url: &str, base_branch: &str, base_oid: &str, sig: &str) -> ResultView {
        let mut r = result_view(&"cc".repeat(32), &"dd".repeat(32));
        r.contribution = Some(ContributionResultView {
            target_owner_pubkey: owner.to_owned(),
            target_clone_url: url.to_owned(),
            base_branch: base_branch.to_owned(),
            base_oid: base_oid.to_owned(),
            tuple_signature: sig.to_owned(),
        });
        r
    }

    #[test]
    fn accept_contribution_records_offer_authority_and_custody_ref() {
        let owner = "aa".repeat(32);
        let url = "https://mobee-relay.orveth.dev/git/owner/repo.git";
        let base_oid = "77".repeat(20);
        let offer = offer_view_contribution(&owner, url, "main", &base_oid);
        let result = result_view_contribution(&owner, url, "main", &base_oid, "sigbytes");
        let bind = resolve_accepted_contribution(&offer, &result, &"ee".repeat(20))
            .expect("resolve ok")
            .expect("is contribution");
        // Authority = the OFFER, not the echo.
        assert_eq!(bind.target_owner_pubkey, owner);
        assert_eq!(bind.base_oid, base_oid);
        assert_eq!(bind.tuple_signature, "sigbytes");
        assert_eq!(
            bind.custody_local_ref,
            crate::delivery_git::PayPathDeliveryVerifier::custody_ref_for(&"ee".repeat(20))
        );
    }

    #[test]
    fn accept_contribution_refuses_echo_target_mismatch() {
        let url = "https://mobee-relay.orveth.dev/git/owner/repo.git";
        let base_oid = "77".repeat(20);
        let offer = offer_view_contribution(&"aa".repeat(32), url, "main", &base_oid);
        // Result echoes a DIFFERENT target owner — MUST-4 equality-check refuses.
        let result = result_view_contribution(&"bb".repeat(32), url, "main", &base_oid, "s");
        let err = resolve_accepted_contribution(&offer, &result, &"ee".repeat(20))
            .expect_err("echo target mismatch must refuse");
        assert!(err.to_string().contains("echo mismatch"), "got {err}");
    }

    #[test]
    fn accept_contribution_refuses_echo_base_mismatch() {
        let owner = "aa".repeat(32);
        let url = "https://mobee-relay.orveth.dev/git/owner/repo.git";
        let offer = offer_view_contribution(&owner, url, "main", &"77".repeat(20));
        // Result echoes a DIFFERENT base_oid.
        let result = result_view_contribution(&owner, url, "main", &"88".repeat(20), "s");
        let err = resolve_accepted_contribution(&offer, &result, &"ee".repeat(20))
            .expect_err("echo base mismatch must refuse");
        assert!(err.to_string().contains("echo mismatch"), "got {err}");
    }

    #[test]
    fn accept_malformed_contribution_offer_fails_closed_not_from_scratch() {
        // job_class=contribution but pins failed to parse (contribution=None) ⇒ REFUSE.
        let mut offer = offer_view_contribution(&"aa".repeat(32), "https://x/r.git", "main", &"77".repeat(20));
        offer.contribution = None; // simulate a malformed contribution offer
        let result = result_view(&"cc".repeat(32), &"dd".repeat(32));
        let err = resolve_accepted_contribution(&offer, &result, &"ee".repeat(20))
            .expect_err("malformed contribution offer must refuse (fail-closed)");
        assert!(err.to_string().contains("malformed"), "got {err}");
    }

    #[test]
    fn accept_contribution_requires_a_contribution_result() {
        let owner = "aa".repeat(32);
        let url = "https://mobee-relay.orveth.dev/git/owner/repo.git";
        let offer = offer_view_contribution(&owner, url, "main", &"77".repeat(20));
        // A from-scratch result (no contribution echo) against a contribution offer ⇒ refuse.
        let result = result_view(&"cc".repeat(32), &"dd".repeat(32));
        assert!(resolve_accepted_contribution(&offer, &result, &"ee".repeat(20)).is_err());
    }

    #[test]
    fn from_scratch_offer_resolves_to_no_contribution() {
        let mut offer = offer_view_contribution(&"aa".repeat(32), "https://x/r.git", "main", &"77".repeat(20));
        offer.job_class = None;
        offer.contribution = None;
        let result = result_view(&"cc".repeat(32), &"dd".repeat(32));
        assert_eq!(
            resolve_accepted_contribution(&offer, &result, &"ee".repeat(20)).expect("ok"),
            None
        );
    }

    #[test]
    fn authorize_request_from_bind_threads_contribution() {
        let mut bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://mobee-relay.orveth.dev/git/seller/fork.git".into(),
            branch: "mobee/contribution/x".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 1,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            contribution: Some(AcceptedContribution {
                target_owner_pubkey: "aa".repeat(32),
                target_clone_url: "https://mobee-relay.orveth.dev/git/owner/repo.git".into(),
                base_branch: "main".into(),
                base_oid: "77".repeat(20),
                tuple_signature: "cafe".into(),
                custody_local_ref: "refs/mobee/deliveries/eeee".into(),
            }),
        };
        let req = authorize_request_from_bind(&bind, 1, bind.commit_oid.clone()).expect("ok");
        let c = req.contribution.expect("threaded");
        assert_eq!(c.target_owner_pubkey, "aa".repeat(32));
        assert_eq!(c.base_oid, "77".repeat(20));
        assert_eq!(c.tuple_signature, "cafe");
        // From-scratch bind ⇒ None threaded.
        bind.contribution = None;
        let req2 = authorize_request_from_bind(&bind, 1, bind.commit_oid.clone()).expect("ok");
        assert!(req2.contribution.is_none());
    }

    #[test]
    fn contribution_offer_view_parses_pins_and_malformed_is_none() {
        // A well-formed contribution offer's tags parse into the view.
        let offer = crate::contribution::ContributionOffer {
            target: crate::contribution::TargetRepoPin::new(
                "aa".repeat(32),
                "https://mobee-relay.orveth.dev/git/owner/repo.git",
            )
            .unwrap(),
            base: crate::contribution::ContributionBase::new("main", "77".repeat(20)).unwrap(),
            accepts: vec!["fork".into()],
        };
        let tags = crate::contribution::contribution_offer_tags(&offer);
        let view = contribution_offer_view(&tags).expect("parsed");
        assert_eq!(view.target_owner_pubkey, "aa".repeat(32));
        assert_eq!(view.base_oid, "77".repeat(20));
        // A contribution offer missing the base tag ⇒ view None (surfaced as job_class-present +
        // contribution-None, which accept refuses).
        let malformed = vec![crate::gateway::TagSpec::new([
            crate::contribution::TAG_JOB_CLASS,
            crate::contribution::JOB_CLASS_CONTRIBUTION,
        ])];
        assert!(contribution_offer_view(&malformed).is_none());
    }

    // ── Piece-10 buyer POST-path: contribution offer params (all-or-nothing + tag emission) ─────
    fn contribution_post_request(
        owner: Option<&str>,
        url: Option<&str>,
        branch: Option<&str>,
        oid: Option<&str>,
        accepts: Option<Vec<String>>,
    ) -> PostJobRequest {
        PostJobRequest {
            task: "t".into(),
            output: "text/plain".into(),
            amount_sats: 1,
            seller_pubkey: None,
            untargeted: true,
            deadline_unix: Some(10),
            repo: None,
            branch: None,
            target_repo_owner: owner.map(str::to_owned),
            target_repo_url: url.map(str::to_owned),
            base_branch: branch.map(str::to_owned),
            base_oid: oid.map(str::to_owned),
            accepts,
        }
    }

    #[test]
    fn post_job_contribution_round_trip_offer_tags_bind_to_offer_values() {
        // The load-bearing round-trip: post_job contribution params -> BUILT event tags ->
        // parse_contribution_offer yields exactly {owner,url,branch,oid} -> emitted tags ARE the
        // canonical constructor output (no drift) -> the accept-path binds to the OFFER's values.
        let owner = "aa".repeat(32);
        let url = "https://mobee-relay.orveth.dev/git/owner/repo.git";
        let base_oid = "77".repeat(20);
        let request =
            contribution_post_request(Some(&owner), Some(url), Some("main"), Some(&base_oid), None);

        let contribution = contribution_offer_from_request(&request)
            .expect("valid contribution params")
            .expect("is a contribution offer");
        let draft =
            build_offer_draft(&request, "https://testnut.cashudevkit.org", 10, Some(&contribution))
                .expect("draft built");

        // (a) canonical parse of the BUILT tags yields exactly the pinned values.
        let parsed = crate::contribution::parse_contribution_offer(&draft.tags)
            .expect("parse ok")
            .expect("is a contribution");
        assert_eq!(parsed.target.owner_pubkey(), owner);
        assert_eq!(parsed.target.clone_url(), url);
        assert_eq!(parsed.base.branch(), "main");
        assert_eq!(parsed.base.oid(), base_oid);
        assert!(parsed.accepts_fork());

        // (b) emitted tags ARE the canonical constructor output (no drift).
        let expected_tags = crate::contribution::contribution_offer_tags(&contribution);
        assert!(
            draft.tags.ends_with(&expected_tags),
            "emitted contribution tags must equal the canonical constructor output"
        );

        // (c) the accept-path binds to the OFFER's values, threaded from the EMITTED tags.
        let mut offer_view = offer_view_contribution(&owner, url, "main", &base_oid);
        offer_view.contribution = contribution_offer_view(&draft.tags);
        let result = result_view_contribution(&owner, url, "main", &base_oid, "sigbytes");
        let bind = resolve_accepted_contribution(&offer_view, &result, &"ee".repeat(20))
            .expect("resolve ok")
            .expect("is a contribution");
        assert_eq!(bind.target_owner_pubkey, owner);
        assert_eq!(bind.target_clone_url, url);
        assert_eq!(bind.base_branch, "main");
        assert_eq!(bind.base_oid, base_oid);
    }

    #[test]
    fn post_job_from_scratch_emits_byte_identical_tags() {
        // No contribution params ⇒ Ok(None) ⇒ built tags are byte-identical to the bare offer.
        let request = PostJobRequest {
            task: "t".into(),
            output: "text/plain".into(),
            amount_sats: 3,
            seller_pubkey: Some("bb".repeat(32)),
            untargeted: false,
            deadline_unix: Some(10),
            repo: None,
            branch: None,
            target_repo_owner: None,
            target_repo_url: None,
            base_branch: None,
            base_oid: None,
            accepts: None,
        };
        let contribution = contribution_offer_from_request(&request).expect("ok");
        assert!(contribution.is_none(), "no params ⇒ from-scratch");
        let draft = build_offer_draft(
            &request,
            "https://testnut.cashudevkit.org",
            10,
            contribution.as_ref(),
        )
        .expect("draft");
        let expected = OfferDraft::new(
            "t",
            "text/plain",
            3,
            10,
            "https://testnut.cashudevkit.org",
            "bb".repeat(32),
        )
        .to_event_draft();
        assert_eq!(draft, expected, "from-scratch draft must be byte-identical");
        assert!(!crate::contribution::is_contribution_tags(&draft.tags));
        // F2: the budget guard fires ONLY over-cap and does NOT touch tag emission — a normal
        // within-cap post (amount 3 <= default cap 21) passes the guard, so emitted tags for a
        // normal post are unchanged (byte-identical, asserted above).
        assert!(
            assert_amount_within_budget_cap(3, crate::home::DEFAULT_PER_JOB_BUDGET_SATS).is_ok(),
            "a within-cap post must pass the budget guard"
        );
    }

    #[test]
    fn post_job_contribution_partial_params_refuse_naming_missing_fields() {
        let owner = "aa".repeat(32);
        let url = "https://mobee-relay.orveth.dev/git/owner/repo.git";
        let base_oid = "77".repeat(20);
        // owner alone ⇒ refuse, naming the three missing required fields.
        let err = contribution_offer_from_request(&contribution_post_request(
            Some(&owner),
            None,
            None,
            None,
            None,
        ))
        .expect_err("partial refused");
        let msg = err.to_string();
        assert!(msg.contains("target_repo_url"), "{msg}");
        assert!(msg.contains("base_branch"), "{msg}");
        assert!(msg.contains("base_oid"), "{msg}");
        // missing only owner ⇒ names owner.
        let err = contribution_offer_from_request(&contribution_post_request(
            None,
            Some(url),
            Some("main"),
            Some(&base_oid),
            None,
        ))
        .expect_err("partial refused");
        assert!(err.to_string().contains("target_repo_owner"), "{err}");
        // accepts alone (no pins) is still a contribution param present ⇒ refuse, naming the pins.
        let err = contribution_offer_from_request(&contribution_post_request(
            None,
            None,
            None,
            None,
            Some(vec!["fork".into()]),
        ))
        .expect_err("accepts-only refused");
        let msg = err.to_string();
        assert!(
            msg.contains("target_repo_owner") && msg.contains("base_oid"),
            "{msg}"
        );
    }

    #[test]
    fn post_job_contribution_bad_fields_refuse() {
        let owner = "aa".repeat(32);
        let url = "https://mobee-relay.orveth.dev/git/owner/repo.git";
        let oid = "77".repeat(20);
        // bad owner (not 64-hex)
        assert!(contribution_offer_from_request(&contribution_post_request(
            Some("nothex"),
            Some(url),
            Some("main"),
            Some(&oid),
            None
        ))
        .is_err());
        // bad oid (not 40/64-hex)
        assert!(contribution_offer_from_request(&contribution_post_request(
            Some(&owner),
            Some(url),
            Some("main"),
            Some("xyz"),
            None
        ))
        .is_err());
        // bad base branch (leading dash)
        assert!(contribution_offer_from_request(&contribution_post_request(
            Some(&owner),
            Some(url),
            Some("-x"),
            Some(&oid),
            None
        ))
        .is_err());
        // bad url (forbidden scheme via the transport allowlist)
        assert!(contribution_offer_from_request(&contribution_post_request(
            Some(&owner),
            Some("file:///tmp/repo.git"),
            Some("main"),
            Some(&oid),
            None
        ))
        .is_err());
        // accepts present but without "fork" (v1 fork-only) ⇒ refuse.
        assert!(contribution_offer_from_request(&contribution_post_request(
            Some(&owner),
            Some(url),
            Some("main"),
            Some(&oid),
            Some(vec!["patch".into()])
        ))
        .is_err());
    }

    #[test]
    fn post_job_contribution_refuses_ext_url_at_post() {
        // ext:: clone URL refused at POST time — a buyer must not publish an unverifiable offer.
        let owner = "aa".repeat(32);
        let oid = "77".repeat(20);
        let err = contribution_offer_from_request(&contribution_post_request(
            Some(&owner),
            Some("ext::sh -c evil"),
            Some("main"),
            Some(&oid),
            None,
        ))
        .expect_err("ext refused at post");
        assert!(err.to_string().contains("refused"), "{err}");
    }

    // ── F2 buyer-fix: post-time per-job budget-cap validation ───────────────────────────────────
    #[test]
    fn budget_cap_guard_over_cap_refuses_at_and_under_cap_pass() {
        // over-cap ⇒ refuse, naming the config key + BOTH numbers + the restart remedy.
        let err = assert_amount_within_budget_cap(40, 21).expect_err("over-cap refused");
        let msg = err.to_string();
        assert!(msg.contains("per_job_budget_sats"), "names the config key: {msg}");
        assert!(msg.contains("40"), "names the amount: {msg}");
        assert!(msg.contains("21"), "names the cap: {msg}");
        assert!(msg.contains("RESTART"), "names the remedy: {msg}");
        // at-cap ⇒ passes (mirrors the budget gate's `amount > cap` refuse condition).
        assert!(assert_amount_within_budget_cap(21, 21).is_ok(), "at-cap must pass");
        // under-cap ⇒ passes, unchanged.
        assert!(assert_amount_within_budget_cap(20, 21).is_ok(), "under-cap must pass");
    }

    #[test]
    fn post_job_deadline_past_refused_names_field_and_values() {
        let err = resolve_post_deadline(Some(1_700_000_000), 1_700_000_001)
            .expect_err("past deadline must refuse");
        let msg = err.to_string();
        assert!(msg.contains("deadline_unix"), "names the field: {msg}");
        assert!(msg.contains("given=1700000000"), "shows given value: {msg}");
        assert!(msg.contains("current=1700000001"), "shows current value: {msg}");
    }

    #[test]
    fn post_job_deadline_zero_refused() {
        let err = resolve_post_deadline(Some(0), 1_700_000_001)
            .expect_err("zero deadline must refuse");
        let msg = err.to_string();
        assert!(msg.contains("deadline_unix"), "{msg}");
        assert!(msg.contains("given=0"), "{msg}");
        assert!(msg.contains("current=1700000001"), "{msg}");
    }

    #[test]
    fn post_job_deadline_omitted_defaults_to_one_hour_from_now() {
        assert_eq!(
            resolve_post_deadline(None, 1_700_000_001).expect("omitted deadline defaults"),
            1_700_003_601
        );
    }

    #[test]
    fn post_job_deadline_future_accepted() {
        assert_eq!(
            resolve_post_deadline(Some(1_700_000_002), 1_700_000_001)
                .expect("future deadline accepted"),
            1_700_000_002
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_job_over_budget_cap_refused_before_wallet_or_publish() {
        // An over-cap post refuses AT POST (before the wallet opens / anything publishes), so this
        // runs fully offline. Field case: amount 40 with per_job_budget_sats = 21.
        let (root, mut home) = temp_job_home("over-cap");
        home.config.per_job_budget_sats = 21;
        let err = post_job_async(
            &home,
            PostJobRequest {
                task: "t".into(),
                output: "text/plain".into(),
                amount_sats: 40,
                seller_pubkey: Some("aa".repeat(32)),
                untargeted: false,
                deadline_unix: Some(1_800_000_000),
                repo: None,
                branch: None,
                target_repo_owner: None,
                target_repo_url: None,
                base_branch: None,
                base_oid: None,
                accepts: None,
            },
        )
        .await
        .expect_err("over-cap post must refuse");
        let msg = err.to_string();
        assert!(msg.contains("per_job_budget_sats"), "{msg}");
        assert!(msg.contains("40") && msg.contains("21"), "{msg}");
        assert!(msg.contains("RESTART"), "{msg}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
