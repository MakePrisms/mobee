//! Seller-side git — ALL in-process via libgit2 (issue #55; NO system `git` on any product path).
//!
//! Base fetch, fork checkout, and delivery push run through [`crate::git_transport`]'s rustls
//! smart-HTTP subtransport, which injects the seller's NIP-98 `Authorization` on relay-git requests.
//! The authorship / non-empty-tree delivery gates read the workdir's object database directly with
//! git2 (no `git log` / `git ls-tree` subprocess).
//!
//! ## Why the old scrub machinery is gone (and this is safe)
//! The previous implementation shelled out to `git` and had to defend against ambient config: empty
//! `GIT_CONFIG_GLOBAL`/`XDG_CONFIG_HOME`, `GIT_CONFIG_NOSYSTEM`, `protocol.*.allow=never`, scrubbed
//! `GIT_SSH*`/`insteadOf`. In-process git2 needs NONE of that:
//! - **`insteadOf` immunity is structural:** every remote is [`Repository::remote_anonymous`], which
//!   uses the literal URL and applies NO `url.*.insteadOf` config rewrite — an agent-planted
//!   `.git/config` (or poisoned `$HOME/.gitconfig`) can never redirect an allowlisted `https` push
//!   onto `ssh`/`file`/`ext`.
//! - **Transport allowlist:** every entry asserts [`assert_allowed_repo_locator`] and only `https`
//!   is registered as a subtransport — `ext:`/`file:`/`ssh:` are refused before any remote exists.
//! - **Key hygiene:** the seller secret signs the NIP-98 event in-process only — never on argv,
//!   never in child env, no subprocess.

use std::path::{Path, PathBuf};

use git2::build::CheckoutBuilder;
use git2::{Direction, ObjectType, Oid, Repository, TreeWalkMode, TreeWalkResult};

use crate::delivery_transport::{assert_allowed_repo_locator, TransportRefuse};
use crate::git_transport::{self, TransportError};

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

impl From<TransportError> for SellerGitError {
    fn from(value: TransportError) -> Self {
        match value {
            TransportError::Transport(m) => Self::Transport(m),
            // A rejected ref or an auth/permission signal is a fail-closed auth failure (parity with
            // the old system-git path, which mapped both to AuthFailed).
            TransportError::Auth(m) | TransportError::Rejected(m) => Self::AuthFailed(m),
            TransportError::Io(m) => Self::Io(m),
        }
    }
}

/// Best-effort `HEAD` OID in `workdir`. `None` when the tree has no commits yet.
///
/// Used by gate #10 (delivery attribution): deliver only agent-authored, non-empty trees.
pub fn try_head_oid(workdir: &Path) -> Option<String> {
    rev_parse_oid(workdir, "HEAD")
}

fn rev_parse_oid(workdir: &Path, rev: &str) -> Option<String> {
    let repo = Repository::open(workdir).ok()?;
    let commit = repo.revparse_single(rev).ok()?.peel_to_commit().ok()?;
    let oid = commit.id().to_string();
    if oid.len() < 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(oid)
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

    /// Env that overrides ambient git identity for commits made during the agent run (the AGENT
    /// process makes those commits with its own git — out of scope for the seller daemon's git2).
    pub fn git_env(&self) -> Vec<(String, String)> {
        vec![
            ("GIT_AUTHOR_NAME".into(), self.name.clone()),
            ("GIT_AUTHOR_EMAIL".into(), self.email.clone()),
            ("GIT_COMMITTER_NAME".into(), self.name.clone()),
            ("GIT_COMMITTER_EMAIL".into(), self.email.clone()),
        ]
    }
}

/// Initialise `workdir` as a fresh repo with the stamped identity in its `.git/config` (so the
/// AGENT's later commits carry it) and `main` as the initial branch. **No harness commit.**
pub fn init_empty_delivery_workdir(
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
) -> Result<(), SellerGitError> {
    let repo = init_repo_with_identity(workdir, identity)?;
    drop(repo);
    Ok(())
}

/// `git init --initial-branch=main` + `git config user.name/email` via git2.
fn init_repo_with_identity(
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
) -> Result<Repository, SellerGitError> {
    if !workdir.exists() {
        std::fs::create_dir_all(workdir).map_err(|error| SellerGitError::Io(error.to_string()))?;
    }
    let mut opts = git2::RepositoryInitOptions::new();
    opts.initial_head("main");
    let repo = Repository::init_opts(workdir, &opts)
        .map_err(|error| SellerGitError::Io(format!("init: {error}")))?;
    {
        let mut cfg = repo
            .config()
            .map_err(|error| SellerGitError::Io(format!("open config: {error}")))?;
        cfg.set_str("user.name", &identity.name)
            .map_err(|_| SellerGitError::CommandFailed("config-user-name"))?;
        cfg.set_str("user.email", &identity.email)
            .map_err(|_| SellerGitError::CommandFailed("config-user-email"))?;
    }
    Ok(repo)
}

/// Contribution (piece-10) fork-from-base: initialise `workdir` as a working clone of the PINNED
/// target `base_clone_url` at `base_oid`, on a per-job unique `branch` carrying the FULL job_id
/// (MUST-6). The agent then commits its work on top; the daemon pushes `branch` to the seller's OWN
/// relay-git namespace. Transport-allowlisted (https + relay-git; `ext::`/file/ssh refused).
///
/// FULL-depth fetch (no depth limit) so the fork carries `base_oid` + ancestry — a shallow fork
/// would make the BUYER's descendant gate false-refuse an honest contribution. The base history is
/// foreign (the target's authors); only the commits ADDED on top are the seller's, so the daemon
/// scopes its authorship gate to `base_oid..HEAD` (see [`require_agent_authored_contribution`]).
pub fn init_contribution_workdir(
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
    base_clone_url: &str,
    base_branch: &str,
    base_oid: &str,
    branch: &str,
    auth: Option<&PushAuth>,
) -> Result<(), SellerGitError> {
    assert_allowed_repo_locator(base_clone_url)?;
    let repo = init_repo_with_identity(workdir, identity)?;
    // Full-depth fetch of the base branch from the pinned target into a local ref. mobee relay-git
    // requires NIP-98 auth for READS, so present the seller secret for relay-git bases; public /
    // anonymous https bases fetch without it (git_transport gates the header on is_relay_git).
    let refspec = format!("+refs/heads/{base_branch}:refs/mobee/base");
    git_transport::fetch_refspecs(
        &repo,
        base_clone_url,
        &[&refspec],
        auth.map(|a| a.secret_key_hex.as_str()),
        false,
    )?;
    drop(repo);
    // Check out base_oid onto the per-job unique branch (the fork tip the agent extends).
    checkout_base_branch(workdir, branch, base_oid)
}

/// `git checkout -B <branch> <base_oid>` via git2 — the fork tip the agent extends. Force-creates
/// the branch at `base_oid`, checks out its tree, and points HEAD at it.
fn checkout_base_branch(
    workdir: &Path,
    branch: &str,
    base_oid: &str,
) -> Result<(), SellerGitError> {
    let repo =
        Repository::open(workdir).map_err(|error| SellerGitError::Io(format!("open: {error}")))?;
    let oid = Oid::from_str(base_oid).map_err(|_| SellerGitError::CommandFailed("checkout-base"))?;
    let commit = repo
        .find_commit(oid)
        .map_err(|_| SellerGitError::CommandFailed("checkout-base"))?;
    repo.branch(branch, &commit, true)
        .map_err(|_| SellerGitError::CommandFailed("checkout-base"))?;
    repo.checkout_tree(commit.as_object(), Some(CheckoutBuilder::new().force()))
        .map_err(|_| SellerGitError::CommandFailed("checkout-base"))?;
    repo.set_head(&format!("refs/heads/{branch}"))
        .map_err(|_| SellerGitError::CommandFailed("checkout-base"))?;
    Ok(())
}

/// `git checkout -B <branch>` at the current HEAD (from-scratch delivery: name the agent's commits
/// onto the per-job branch so the daemon can push it). Best-effort — points the branch ref at HEAD
/// and moves HEAD to it (HEAD's tree is unchanged, so no working-tree checkout is needed).
pub fn point_branch_at_head(workdir: &Path, branch: &str) -> Result<(), SellerGitError> {
    let repo =
        Repository::open(workdir).map_err(|error| SellerGitError::Io(format!("open: {error}")))?;
    let head_commit = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map_err(|_| SellerGitError::CommandFailed("point-branch-head"))?;
    repo.branch(branch, &head_commit, true)
        .map_err(|_| SellerGitError::CommandFailed("point-branch-head"))?;
    repo.set_head(&format!("refs/heads/{branch}"))
        .map_err(|_| SellerGitError::CommandFailed("point-branch-head"))?;
    Ok(())
}

/// Contribution authorship gate (piece-10): deliver IFF every commit in `base_oid..HEAD` is
/// agent-authored, HEAD advanced past `base_oid`, and the HEAD tree is non-empty. Returns the HEAD
/// oid on success.
pub fn require_agent_authored_contribution(
    workdir: &Path,
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
    require_range_agent_authored(workdir, identity, base_oid, after)?;
    require_head_tree_nonempty(workdir, after)?;
    Ok(after.to_owned())
}

/// Every commit reachable from `after` but not from `base_oid` must match the stamped identity
/// (author AND committer). Fail-closed on any read error.
fn require_range_agent_authored(
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
    base_oid: &str,
    after: &str,
) -> Result<(), SellerGitError> {
    let repo = Repository::open(workdir).map_err(|_| {
        SellerGitError::Io("contribution refused: cannot open workdir — fail-closed".into())
    })?;
    let after_oid = Oid::from_str(after)
        .map_err(|_| SellerGitError::Io("contribution refused: bad HEAD oid".into()))?;
    let base = Oid::from_str(base_oid)
        .map_err(|_| SellerGitError::Io("contribution refused: bad base oid".into()))?;
    let mut walk = repo.revwalk().map_err(|_| {
        SellerGitError::Io("contribution refused: cannot read commit authors — fail-closed".into())
    })?;
    walk.push(after_oid).map_err(|_| {
        SellerGitError::Io("contribution refused: cannot read commit authors — fail-closed".into())
    })?;
    // `base_oid..after` — hide base and its ancestors (the foreign base history).
    walk.hide(base).map_err(|_| {
        SellerGitError::Io("contribution refused: cannot read commit authors — fail-closed".into())
    })?;

    let mut saw_commit = false;
    for oid in walk {
        let oid = oid.map_err(|_| {
            SellerGitError::Io(
                "contribution refused: cannot read commit authors — fail-closed".into(),
            )
        })?;
        let commit = repo.find_commit(oid).map_err(|_| {
            SellerGitError::Io(
                "contribution refused: cannot read commit authors — fail-closed".into(),
            )
        })?;
        saw_commit = true;
        if !commit_matches_identity(&commit, identity) {
            let author = commit.author();
            return Err(SellerGitError::Io(format!(
                "contribution refused: commit on top of base not agent-authored (author={} <{}>) — expected {} <{}>",
                author.name().unwrap_or(""),
                author.email().unwrap_or(""),
                identity.name,
                identity.email
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
pub fn require_agent_authored_delivery(
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
    after: Option<&str>,
) -> Result<String, SellerGitError> {
    let after = after.ok_or_else(|| {
        SellerGitError::Io(
            "delivery refused: agent left no commit (HEAD missing) — no harness fallback".into(),
        )
    })?;
    require_all_commits_agent_authored(workdir, identity)?;
    require_head_tree_nonempty(workdir, after)?;
    Ok(after.to_owned())
}

/// Every commit reachable from HEAD must match the stamped identity.
fn require_all_commits_agent_authored(
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
) -> Result<(), SellerGitError> {
    let repo = Repository::open(workdir).map_err(|_| {
        SellerGitError::Io(
            "delivery refused: cannot open workdir — fail-closed (no harness fallback)".into(),
        )
    })?;
    let mut walk = repo.revwalk().map_err(|_| {
        SellerGitError::Io(
            "delivery refused: cannot read commit authors — fail-closed (no harness fallback)"
                .into(),
        )
    })?;
    walk.push_head().map_err(|_| {
        SellerGitError::Io(
            "delivery refused: agent left no commit (HEAD missing) — no harness fallback".into(),
        )
    })?;

    let mut saw_commit = false;
    for oid in walk {
        let oid = oid.map_err(|_| {
            SellerGitError::Io(
                "delivery refused: cannot read commit authors — fail-closed (no harness fallback)"
                    .into(),
            )
        })?;
        let commit = repo.find_commit(oid).map_err(|_| {
            SellerGitError::Io(
                "delivery refused: cannot read commit authors — fail-closed (no harness fallback)"
                    .into(),
            )
        })?;
        saw_commit = true;
        if !commit_matches_identity(&commit, identity) {
            let author = commit.author();
            let committer = commit.committer();
            return Err(SellerGitError::Io(format!(
                "delivery refused: commit not agent-authored (author={} <{}>, committer={} <{}>; expected {} <{}>) — clone-only / foreign history",
                author.name().unwrap_or(""),
                author.email().unwrap_or(""),
                committer.name().unwrap_or(""),
                committer.email().unwrap_or(""),
                identity.name,
                identity.email
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

/// True IFF the commit's author AND committer both match the stamped identity (name and email).
fn commit_matches_identity(commit: &git2::Commit, identity: &DeliveryAgentIdentity) -> bool {
    let author = commit.author();
    let committer = commit.committer();
    author.name() == Some(identity.name.as_str())
        && author.email() == Some(identity.email.as_str())
        && committer.name() == Some(identity.name.as_str())
        && committer.email() == Some(identity.email.as_str())
}

/// Refuse a delivery whose HEAD tree contains no file (parity with `git ls-tree -r` being empty).
fn require_head_tree_nonempty(workdir: &Path, after: &str) -> Result<(), SellerGitError> {
    let repo = Repository::open(workdir).map_err(|_| {
        SellerGitError::Io(
            "delivery refused: delivery tree unknown — fail-closed (no harness fallback)".into(),
        )
    })?;
    let oid = Oid::from_str(after).map_err(|_| {
        SellerGitError::Io(
            "delivery refused: delivery tree unknown — fail-closed (no harness fallback)".into(),
        )
    })?;
    let tree = repo
        .find_commit(oid)
        .and_then(|commit| commit.tree())
        .map_err(|_| {
            SellerGitError::Io(
                "delivery refused: delivery tree unknown — fail-closed (no harness fallback)".into(),
            )
        })?;
    let mut has_file = false;
    tree.walk(TreeWalkMode::PreOrder, |_, entry| {
        if entry.kind() == Some(ObjectType::Blob) {
            has_file = true;
            TreeWalkResult::Abort
        } else {
            TreeWalkResult::Ok
        }
    })
    .map_err(|_| {
        SellerGitError::Io(
            "delivery refused: delivery tree unknown — fail-closed (no harness fallback)".into(),
        )
    })?;
    if !has_file {
        return Err(SellerGitError::Io(
            "delivery refused: empty tree (no substantive files) — no harness fallback".into(),
        ));
    }
    Ok(())
}

/// Optional NIP-98 auth for relay-git push/fetch (key never logged / never on argv).
///
/// `secret_key_hex` signs the NIP-98 event in-process for relay-git remotes. Public / anonymous
/// https remotes present no auth. Callers must not print it.
#[derive(Debug, Clone)]
pub struct PushAuth {
    pub secret_key_hex: String,
}

/// Push `branch` from `workdir` to `remote_url` (allowlisted https / relay-git only).
///
/// Returns the pushed commit OID (full hex). Unauthenticated / prompt-needing remotes fail closed.
pub fn push_branch(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
) -> Result<String, SellerGitError> {
    push_branch_with_auth(workdir, remote_url, branch, None)
}

/// Like [`push_branch`], with optional NIP-98 auth for relay-git. Always in-process libgit2 — there
/// is no system-git fallback (issue #55).
pub fn push_branch_with_auth(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    auth: Option<&PushAuth>,
) -> Result<String, SellerGitError> {
    assert_allowed_repo_locator(remote_url)?;
    if branch.trim().is_empty() {
        return Err(SellerGitError::Io("branch must be non-empty".into()));
    }
    let oid =
        git_transport::push_branch(workdir, remote_url, branch, auth.map(|a| a.secret_key_hex.as_str()))?;
    eprintln!("seller push path=inprocess remote={remote_url} branch={branch} ok");
    Ok(oid)
}

/// Boot-time WRITE-auth probe: connect to `remote_url` in the PUSH direction and read the
/// receive-pack ref advertisement (the auth-gated leg) WITHOUT transferring a pack or mutating the
/// remote. Surfaces a broken write path — missing/invalid credential, unannounced/unreachable
/// relay-git repo, write-scoped auth failure — at daemon boot instead of at job-delivery time.
///
/// git2 has no `push --dry-run`; `connect(Push) + list` is the faithful equivalent (it performs
/// exactly the receive-pack advertisement mobee-relay NIP-98 auth-gates, then stops). Allowlisted
/// (https + relay-git; `ext::`/file/ssh refused).
pub fn preflight_push_probe(
    remote_url: &str,
    auth: Option<&PushAuth>,
) -> Result<(), SellerGitError> {
    assert_allowed_repo_locator(remote_url)?;
    git_transport::list_remote(
        remote_url,
        auth.map(|a| a.secret_key_hex.as_str()),
        Direction::Push,
    )
    .map(|_| ())
    .map_err(SellerGitError::from)
}

/// Resolve `git-credential-nostr` absolute path (`MOBEE_GIT_CREDENTIAL_NOSTR` override, then PATH).
///
/// Retained for `mobee doctor`'s informational check only — the seller itself no longer needs the
/// helper (all git legs are in-process libgit2 with NIP-98 signed in this process).
pub fn resolve_git_credential_nostr() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("MOBEE_GIT_CREDENTIAL_NOSTR") {
        let path = PathBuf::from(override_path);
        if path.is_file() {
            return Some(path);
        }
    }
    which_bin("git-credential-nostr").ok()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
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
        let _ = fs::remove_dir_all(&root);
        init_repo(&root);
        assert!(matches!(
            push_branch(&root, "git@example.invalid:repo.git", "main"),
            Err(SellerGitError::Transport(_))
        ));
        assert!(matches!(
            push_branch(&root, "/tmp/local.git", "main"),
            Err(SellerGitError::Transport(_))
        ));
        assert!(matches!(
            push_branch(&root, "ssh://example.invalid/repo.git", "main"),
            Err(SellerGitError::Transport(_))
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn push_to_local_https_style_bare_via_file_is_refused() {
        // file:// is not on allowlist — fail closed even for fixtures.
        let root = temp("file-refuse");
        let _ = fs::remove_dir_all(&root);
        init_repo(&root);
        let err = push_branch(
            &root,
            &format!("file://{}/remote.git", root.display()),
            "main",
        )
        .expect_err("file refused");
        assert!(matches!(err, SellerGitError::Transport(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn preflight_push_probe_refuses_non_allowlisted_remote() {
        for bad in [
            "git@example.invalid:repo.git",
            "ssh://example.invalid/repo.git",
            "/tmp/local.git",
            "ext::sh -c evil",
        ] {
            assert!(
                matches!(
                    preflight_push_probe(bad, None),
                    Err(SellerGitError::Transport(_))
                ),
                "expected transport refuse for {bad}"
            );
        }
    }

    #[test]
    fn preflight_push_probe_fails_closed_on_unreachable_https_remote() {
        // No mutation, no hang: an allowlisted-but-unresolvable https remote must fail closed
        // rather than block boot. In-process libgit2 surfaces the DNS/connect failure as an Io
        // transport error (the reqwest client's connect_timeout bounds it) — never a success.
        let err = preflight_push_probe("https://mobee-preflight.invalid/git/owner/repo.git", None)
        .expect_err("unreachable remote must fail closed");
        assert!(
            matches!(
                err,
                SellerGitError::AuthFailed(_)
                    | SellerGitError::CommandFailed(_)
                    | SellerGitError::Io(_)
            ),
            "got {err:?}"
        );
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
        let foreign = temp("gate10-foreign-src");
        let _ = fs::remove_dir_all(&workdir);
        let _ = fs::remove_dir_all(&foreign);

        let pk = "11".repeat(32);
        let identity = DeliveryAgentIdentity::for_seller(&pk);
        init_empty_delivery_workdir(&workdir, &identity).expect("stamp init");

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

        let after = try_head_oid(&workdir).expect("clone left a HEAD");
        let err = require_agent_authored_delivery(&workdir, &identity, Some(&after))
            .expect_err("clone-then-no-work must refuse");
        assert!(
            err.to_string().contains("not agent-authored")
                || err.to_string().contains("foreign"),
            "expected authorship refuse, got: {err}"
        );

        let _ = fs::remove_dir_all(&workdir);
        let _ = fs::remove_dir_all(&foreign);
    }

    #[test]
    fn gate10_wired_agent_authored_nonempty_accepts() {
        // (b) Legit empty-base agent work under the stamp → ACCEPT.
        let workdir = temp("gate10-ok");
        let _ = fs::remove_dir_all(&workdir);

        let pk = "22".repeat(32);
        let identity = DeliveryAgentIdentity::for_seller(&pk);
        init_empty_delivery_workdir(&workdir, &identity).expect("stamp init");
        // Empty base: no HEAD yet.
        assert!(try_head_oid(&workdir).is_none());

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

        let after = try_head_oid(&workdir).expect("after");
        let accepted =
            require_agent_authored_delivery(&workdir, &identity, Some(&after))
                .expect("legit agent work must accept");
        assert_eq!(accepted, after);

        let _ = fs::remove_dir_all(&workdir);
    }

    #[test]
    fn gate10_wired_empty_tree_refuses() {
        let workdir = temp("gate10-empty-tree");
        let _ = fs::remove_dir_all(&workdir);

        let pk = "33".repeat(32);
        let identity = DeliveryAgentIdentity::for_seller(&pk);
        init_empty_delivery_workdir(&workdir, &identity).expect("stamp init");
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
        let after = try_head_oid(&workdir).expect("empty commit has HEAD");
        let err = require_agent_authored_delivery(&workdir, &identity, Some(&after))
            .expect_err("empty tree must refuse");
        assert!(
            err.to_string().contains("empty tree"),
            "expected empty-tree refuse, got: {err}"
        );

        let _ = fs::remove_dir_all(&workdir);
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
        let identity = DeliveryAgentIdentity::for_seller(&"aa".repeat(32));
        let err = init_contribution_workdir(
            &root,
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
    }

    #[test]
    fn require_agent_authored_contribution_scopes_gate_to_range() {
        let root = temp("contrib-range");
        let identity = DeliveryAgentIdentity::for_seller(&"bb".repeat(32));
        init_repo(&root); // base commit authored by "Mobee Seller Test" (foreign to the stamp)
        let base_oid = {
            let out = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&root).output().unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        // Agent commit ON TOP of base, authored by the STAMPED identity.
        let after = commit_as(&root, &identity.name, &identity.email, "src/x.rs", "work\n", "agent work");
        // Range gate passes (base is foreign, but base_oid..HEAD is all agent-authored).
        require_agent_authored_contribution(&root, &identity, &base_oid, Some(&after))
            .expect("agent-authored range must pass despite foreign base");

        // A foreign commit on top of base ⇒ refuse.
        let foreign = commit_as(&root, "Stranger", "x@evil.invalid", "src/y.rs", "sneaky\n", "foreign");
        assert!(require_agent_authored_contribution(&root, &identity, &base_oid, Some(&foreign)).is_err());

        // HEAD == base (no advancement) ⇒ refuse.
        assert!(require_agent_authored_contribution(&root, &identity, &base_oid, Some(&base_oid)).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    /// PATH-stripped proof (#55): drive the seller's LOCAL git legs (init, authorship gate,
    /// non-empty-tree gate, branch-at-head) with git2 ONLY — the delivery commit is created with
    /// git2, never `Command`. Run with `git` absent from PATH to prove these legs have no shell-out.
    #[test]
    fn seller_gates_without_system_git() {
        use git2::{Repository, Signature};
        let workdir = temp("nogit-seller");
        let _ = fs::remove_dir_all(&workdir);

        let pk = "55".repeat(32);
        let identity = DeliveryAgentIdentity::for_seller(&pk);
        init_empty_delivery_workdir(&workdir, &identity).expect("init");

        // Agent-authored commit via git2 (no system git), stamped as the seller identity.
        let repo = Repository::open(&workdir).expect("open");
        fs::write(workdir.join("out.txt"), "work\n").expect("write");
        let mut index = repo.index().expect("index");
        index.add_path(Path::new("out.txt")).expect("add");
        index.write().expect("index write");
        let tree = repo.find_tree(index.write_tree().expect("write tree")).expect("tree");
        let sig = Signature::now(&identity.name, &identity.email).expect("sig");
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, "agent work", &tree, &[])
            .expect("git2 commit");
        let after = oid.to_string();

        require_agent_authored_delivery(&workdir, &identity, Some(&after))
            .expect("agent-authored delivery gate must pass under git2");
        point_branch_at_head(&workdir, "mobee/job").expect("branch-at-head");
        assert_eq!(try_head_oid(&workdir).expect("head"), after);

        let _ = fs::remove_dir_all(&workdir);
    }

    #[test]
    fn checkout_base_branch_from_oid_creates_fork_tip() {
        // Bug-1 regression: `git checkout -B <branch> <oid>` must NOT pass `--` before the
        // oid (that turns the oid into a pathspec → "not a commit ... cannot be created").
        // RED at 8253b70 (with the `--`); GREEN after removal.
        let root = temp("checkout-base");
        let _ = fs::remove_dir_all(&root);
        init_repo(&root); // leaves a commit whose oid is present in root's object db
        let base_oid = {
            let out = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&root)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };

        checkout_base_branch(&root, "mobee/contribution/job", &base_oid)
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
    }

}
