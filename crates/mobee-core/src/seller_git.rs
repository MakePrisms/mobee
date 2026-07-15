//! Seller-side git push: transport allowlist + ambient scrub + fail-closed auth.
//!
//! Does NOT modify buyer [`crate::delivery_git::PayPathDeliveryVerifier`]. Seller-local
//! Command wrapper mirrors the buyer's `GIT_TERMINAL_PROMPT=0` policy and additionally
//! strips ambient `GIT_SSH*` / `insteadOf` override env that could bypass the allowlist.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output, Stdio};

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

/// Push `branch` from `workdir` to `remote_url` (allowlisted https / relay-git only).
///
/// Returns the pushed commit OID (full hex). Unauthenticated / prompt-needing remotes
/// fail closed (no hang) via `GIT_TERMINAL_PROMPT=0` + scrubbed ambient SSH/insteadOf.
pub fn push_branch(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
) -> Result<String, SellerGitError> {
    assert_allowed_repo_locator(remote_url)?;
    if branch.trim().is_empty() {
        return Err(SellerGitError::Io("branch must be non-empty".into()));
    }

    // Ensure origin points at the allowlisted remote for this push only.
    let _ = scrubbed_git(workdir, ["remote", "remove", "origin"]);
    run_ok(
        "remote-add",
        scrubbed_git(workdir, ["remote", "add", "origin", remote_url])?,
    )?;

    let push = scrubbed_git(workdir, ["push", "-u", "origin", branch])?;
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

    let rev = scrubbed_git(workdir, ["rev-parse", "HEAD"])?;
    run_ok("rev-parse", rev.clone())?;
    let oid = String::from_utf8_lossy(&rev.stdout).trim().to_owned();
    if oid.len() < 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SellerGitError::Io(format!(
            "unexpected commit oid {oid:?}"
        )));
    }
    Ok(oid)
}

fn scrubbed_git<I, S>(workdir: &Path, args: I) -> Result<Output, SellerGitError>
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
        // Scrub ambient SSH / insteadOf override vectors.
        .env_remove("GIT_SSH")
        .env_remove("GIT_SSH_COMMAND")
        .env_remove("SSH_ASKPASS")
        .env_remove("GIT_ASKPASS")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_CONFIG_COUNT")
        .env("GIT_CONFIG_NOSYSTEM", "1");

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
        let err = push_branch(&root, &format!("file://{}/remote.git", root.display()), "main")
            .expect_err("file refused");
        assert!(matches!(err, SellerGitError::Transport(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn allowlist_https_accepted_by_locator_gate() {
        assert_allowed_repo_locator("https://example.invalid/git/owner/repo.git").unwrap();
        assert_allowed_repo_locator("https://example.invalid/repo.git").unwrap();
    }
}
