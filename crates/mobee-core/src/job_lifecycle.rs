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

    let deadline_unix = request.deadline_unix.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() + DEFAULT_DEADLINE_SECS)
            .unwrap_or(DEFAULT_DEADLINE_SECS)
    });

    let offer = if request.untargeted {
        OfferDraft::untargeted(
            request.task.clone(),
            request.output.clone(),
            request.amount_sats,
            deadline_unix,
            home.config.mint_url.clone(),
        )
    } else {
        OfferDraft::new(
            request.task.clone(),
            request.output.clone(),
            request.amount_sats,
            deadline_unix,
            home.config.mint_url.clone(),
            request.seller_pubkey.clone().expect("checked"),
        )
    };

    let mut draft = offer.to_event_draft();
    if let (Some(repo), Some(branch)) = (&request.repo, &request.branch) {
        draft.tags.push(TagSpec::new(["delivery", "git"]));
        draft.tags.push(TagSpec::new(["repo", repo]));
        draft.tags.push(TagSpec::new(["branch", branch]));
    }

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
    };
    write_accepted_bind(home, &bind)?;

    Ok(AcceptClaimOutcome {
        accept_event_id,
        bind,
    })
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

async fn fetch_job_view_async(
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
        return results
            .iter()
            .find(|result| result.result_id == id)
            .ok_or_else(|| JobLifecycleError::NotFound(format!("result {id}")));
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
        };
        let bad_seller = "00".repeat(32);
        let err = assert_authorize_matches_bind(&bind, &bad_seller, &bind.result_id, &bind.commit_oid)
            .expect_err("mismatch");
        assert!(err.to_string().contains("seller"));
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
}
