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

/// Seller push failure (maps to kind-7000 error in the daemon).
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
    // Full-depth fetch of the base branch from the pinned target into a local ref.
    let refspec = format!("+refs/heads/{base_branch}:refs/mobee/base");
    run_ok(
        "fetch-base",
        scrubbed_git(
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
        )?,
    )?;
    // Check out base_oid onto the per-job unique branch (the fork tip the agent extends).
    run_ok(
        "checkout-base",
        scrubbed_git(
            workdir,
            seller_home,
            ["checkout", "-B", branch, "--", base_oid],
        )?,
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
    let mut helper_cfg: Option<String> = None;
    if auth.is_some() {
        let helper = resolve_git_credential_nostr().ok_or_else(|| {
            SellerGitError::AuthFailed(
                "git-credential-nostr not found (set MOBEE_GIT_CREDENTIAL_NOSTR or install helper)"
                    .into(),
            )
        })?;
        helper_cfg = Some(format!("credential.helper={}", helper.to_string_lossy()));
    }
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
    if let Some(helper) = resolve_git_credential_nostr() {
        if let Some(dir) = helper.parent() {
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
    }

    cmd.output().map_err(|_| SellerGitError::Unavailable)
}

fn run_ok(op: &'static str, output: Output) -> Result<(), SellerGitError> {
    if output.status.success() {
        Ok(())
    } else {
        Err(SellerGitError::CommandFailed(op))
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
}
