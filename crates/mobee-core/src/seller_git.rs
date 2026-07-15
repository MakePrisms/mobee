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
/// Used by gate #10 (delivery attribution): deliver only if the agent advanced `HEAD`.
pub fn try_head_oid(workdir: &Path, seller_home: &Path) -> Option<String> {
    let rev = scrubbed_git(workdir, seller_home, ["rev-parse", "HEAD"]).ok()?;
    if !rev.status.success() {
        return None;
    }
    let oid = String::from_utf8_lossy(&rev.stdout).trim().to_owned();
    if oid.len() < 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(oid.to_ascii_lowercase())
}

/// Gate #10: refuse deliver unless agent advanced `HEAD` (never harness-authored fallback).
pub fn require_agent_advanced_head(
    before: Option<&str>,
    after: Option<&str>,
) -> Result<String, SellerGitError> {
    let after = after.ok_or_else(|| {
        SellerGitError::Io(
            "delivery refused: agent left no commit (HEAD missing) — no harness fallback".into(),
        )
    })?;
    if let Some(before) = before {
        if before.eq_ignore_ascii_case(after) {
            return Err(SellerGitError::Io(
                "delivery refused: HEAD did not advance after agent run (no harness fallback)"
                    .into(),
            ));
        }
    }
    Ok(after.to_owned())
}

/// Push `branch` from `workdir` to `remote_url` (allowlisted https / relay-git only).
///
/// `seller_home` is the seller's packaged home root: scrubbed git sets `HOME` to this
/// path so libcurl can read a seller-owned `.netrc` (mode `0600`) without rewriting
/// the daemon process environment (nostr/TLS keep ambient HOME).
///
/// Returns the pushed commit OID (full hex). Unauthenticated / prompt-needing remotes
/// fail closed (no hang) via `GIT_TERMINAL_PROMPT=0` + scrubbed ambient SSH/insteadOf.
pub fn push_branch(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    seller_home: &Path,
) -> Result<String, SellerGitError> {
    assert_allowed_repo_locator(remote_url)?;
    if branch.trim().is_empty() {
        return Err(SellerGitError::Io("branch must be non-empty".into()));
    }

    // Ensure origin points at the allowlisted remote for this push only.
    let _ = scrubbed_git(workdir, seller_home, ["remote", "remove", "origin"]);
    run_ok(
        "remote-add",
        scrubbed_git(workdir, seller_home, ["remote", "add", "origin", remote_url])?,
    )?;

    let push = scrubbed_git(workdir, seller_home, ["push", "-u", "origin", branch])?;
    if !push.status.success() {
        let stderr = String::from_utf8_lossy(&push.stderr).to_lowercase();
        if stderr.contains("authentication")
            || stderr.contains("could not read username")
            || stderr.contains("permission denied")
            || stderr.contains("403")
            || stderr.contains("401")
            || stderr.contains("terminal prompts disabled")
        {
            return Err(SellerGitError::AuthFailed(
                "unauthenticated or prompt-required remote (fail-closed)".into(),
            ));
        }
        return Err(SellerGitError::CommandFailed("push"));
    }

    let rev = scrubbed_git(workdir, seller_home, ["rev-parse", "HEAD"])?;
    run_ok("rev-parse", rev.clone())?;
    let oid = String::from_utf8_lossy(&rev.stdout).trim().to_owned();
    if oid.len() < 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SellerGitError::Io(format!(
            "unexpected commit oid {oid:?}"
        )));
    }
    Ok(oid)
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
    let mut cmd = Command::new("git");
    cmd.current_dir(workdir)
        .args(args)
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
    fn gate10_refuses_unchanged_head_and_missing_after() {
        let oid = "a".repeat(40);
        let err = require_agent_advanced_head(Some(&oid), Some(&oid)).expect_err("same head");
        assert!(err.to_string().contains("did not advance"), "{err}");
        let err = require_agent_advanced_head(Some(&oid), None).expect_err("missing after");
        assert!(err.to_string().contains("no commit"), "{err}");
        let advanced = "b".repeat(40);
        let ok = require_agent_advanced_head(Some(&oid), Some(&advanced)).expect("advanced");
        assert_eq!(ok, advanced);
        let first = require_agent_advanced_head(None, Some(&advanced)).expect("first commit");
        assert_eq!(first, advanced);
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
}
