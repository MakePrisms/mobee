use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::delivery::{CommitOid, DeliveryError, DeliveryVerifier, GitDelivery, VerifiedDelivery};

/// Real git-backed verifier that retains fetched objects in a buyer-owned repository.
pub struct GitDeliveryVerifier {
    repository: PathBuf,
}

impl GitDeliveryVerifier {
    /// Creates a verifier whose fetched object database lives at `repository`.
    pub fn new(repository: impl Into<PathBuf>) -> Self {
        Self {
            repository: repository.into(),
        }
    }

    /// Returns the local repository that holds verified delivery objects.
    pub fn repository(&self) -> &Path {
        &self.repository
    }

    fn ensure_repository(&self, oid: &CommitOid) -> Result<(), DeliveryError> {
        if self.repository.join("HEAD").is_file() {
            let output = git_output([
                OsStr::new("-C"),
                self.repository.as_os_str(),
                OsStr::new("rev-parse"),
                OsStr::new("--show-object-format"),
            ])?;
            if !output.status.success() {
                return Err(DeliveryError::GitCommandFailed("object-format"));
            }
            let actual = String::from_utf8_lossy(&output.stdout);
            let expected = if oid.as_str().len() == 64 {
                "sha256"
            } else {
                "sha1"
            };
            if actual.trim() != expected {
                return Err(DeliveryError::GitCommandFailed("object-format"));
            }
            return Ok(());
        }
        if let Some(parent) = self.repository.parent() {
            fs::create_dir_all(parent).map_err(|_| DeliveryError::GitCommandFailed("init"))?;
        }
        let mut args = vec![OsStr::new("init"), OsStr::new("--bare")];
        if oid.as_str().len() == 64 {
            args.push(OsStr::new("--object-format=sha256"));
        }
        args.push(OsStr::new("--"));
        args.push(self.repository.as_os_str());
        let output = git_output(args)?;
        require_success("init", output)
    }

    fn check_branch(&self, branch: &str) -> Result<(), DeliveryError> {
        let output = git_output([
            OsStr::new("check-ref-format"),
            OsStr::new("--branch"),
            OsStr::new(branch),
        ])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(DeliveryError::InvalidBranch)
        }
    }

    fn fetch(&self, delivery: &GitDelivery) -> Result<CommitOid, DeliveryError> {
        let fetched_ref = format!("refs/mobee/deliveries/{}", delivery.commit_oid().as_str());
        let refspec = format!("+refs/heads/{}:{fetched_ref}", delivery.branch());
        let output = git_output([
            OsStr::new("-C"),
            self.repository.as_os_str(),
            OsStr::new("fetch"),
            OsStr::new("--no-tags"),
            OsStr::new("--force"),
            OsStr::new("--end-of-options"),
            OsStr::new(delivery.repo()),
            OsStr::new(&refspec),
        ])?;
        require_success("fetch", output)?;

        let fetched_object = format!("{fetched_ref}^{{commit}}");
        let output = git_output([
            OsStr::new("-C"),
            self.repository.as_os_str(),
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new(&fetched_object),
        ])?;
        if !output.status.success() {
            return Err(DeliveryError::MissingFetchedTip);
        }
        let oid = String::from_utf8(output.stdout)
            .map_err(|_| DeliveryError::MissingFetchedTip)?
            .trim()
            .to_owned();
        CommitOid::parse(oid).map_err(|_| DeliveryError::MissingFetchedTip)
    }

    fn require_local_object(&self, oid: &CommitOid) -> Result<(), DeliveryError> {
        let object = format!("{}^{{commit}}", oid.as_str());
        let output = git_output([
            OsStr::new("-C"),
            self.repository.as_os_str(),
            OsStr::new("cat-file"),
            OsStr::new("-e"),
            OsStr::new(&object),
        ])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(DeliveryError::MissingCommitObject)
        }
    }
}

impl DeliveryVerifier for GitDeliveryVerifier {
    fn verify(&mut self, delivery: &GitDelivery) -> Result<VerifiedDelivery, DeliveryError> {
        self.check_branch(delivery.branch())?;
        self.ensure_repository(delivery.commit_oid())?;
        let fetched_tip = self.fetch(delivery)?;
        let verified = VerifiedDelivery::from_fetched_tip(delivery, fetched_tip)?;
        self.require_local_object(verified.commit_oid())?;
        Ok(verified)
    }
}

fn git_output<I, S>(args: I) -> Result<Output, DeliveryError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .output()
        .map_err(|_| DeliveryError::GitUnavailable)
}

fn require_success(operation: &'static str, output: Output) -> Result<(), DeliveryError> {
    if output.status.success() {
        Ok(())
    } else {
        Err(DeliveryError::GitCommandFailed(operation))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_REPO: AtomicU64 = AtomicU64::new(1);

    struct Fixture {
        root: PathBuf,
        work: PathBuf,
        remote: PathBuf,
        custody: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let id = NEXT_REPO.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("mobee-delivery-{}-{id}", std::process::id()));
            let work = root.join("work");
            let remote = root.join("remote.git");
            let custody = root.join("custody.git");
            fs::create_dir_all(&work).expect("create fixture");
            run(["init", "--initial-branch=main"], Some(&work));
            run(["config", "user.name", "Mobee Test"], Some(&work));
            run(
                ["config", "user.email", "mobee@example.invalid"],
                Some(&work),
            );
            fs::write(work.join("delivery.txt"), "one\n").expect("write delivery");
            run(["add", "delivery.txt"], Some(&work));
            run(["commit", "-m", "delivery one"], Some(&work));
            run(
                ["init", "--bare", remote.to_str().expect("remote path")],
                None,
            );
            run(
                [
                    "remote",
                    "add",
                    "origin",
                    remote.to_str().expect("remote path"),
                ],
                Some(&work),
            );
            run(["push", "origin", "main"], Some(&work));
            Self {
                root,
                work,
                remote,
                custody,
            }
        }

        fn head(&self) -> CommitOid {
            let output = Command::new("git")
                .args([
                    "-C",
                    self.work.to_str().expect("work path"),
                    "rev-parse",
                    "HEAD",
                ])
                .output()
                .expect("rev-parse");
            assert!(output.status.success());
            CommitOid::parse(String::from_utf8(output.stdout).expect("utf8").trim())
                .expect("head oid")
        }

        fn delivery(&self, oid: CommitOid) -> GitDelivery {
            GitDelivery::new(self.remote.to_str().expect("remote path"), "main", oid)
                .expect("delivery")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn run<const N: usize>(args: [&str; N], cwd: Option<&Path>) {
        let mut command = Command::new("git");
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command.output().expect("run git fixture command");
        assert!(
            output.status.success(),
            "git fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn fetch_tip_match_returns_custodied_commit() {
        let fixture = Fixture::new();
        let advertised = fixture.delivery(fixture.head());
        let mut verifier = GitDeliveryVerifier::new(&fixture.custody);

        let verified = verifier.verify(&advertised).expect("verify delivery");

        assert_eq!(verified.commit_oid(), advertised.commit_oid());
        let object = format!("{}^{{commit}}", verified.commit_oid());
        let status = Command::new("git")
            .args([
                "-C",
                fixture.custody.to_str().expect("custody path"),
                "cat-file",
                "-e",
                &object,
            ])
            .status()
            .expect("inspect custody");
        assert!(status.success());
    }

    #[test]
    fn moved_tip_refuses_the_stale_advertised_oid() {
        let fixture = Fixture::new();
        let advertised = fixture.delivery(fixture.head());
        let mut verifier = GitDeliveryVerifier::new(&fixture.custody);
        verifier
            .verify(&advertised)
            .expect("initial delivery verifies");
        fs::write(fixture.work.join("delivery.txt"), "two\n").expect("advance delivery");
        run(["add", "delivery.txt"], Some(&fixture.work));
        run(["commit", "-m", "delivery two"], Some(&fixture.work));
        run(["push", "origin", "main"], Some(&fixture.work));

        assert!(matches!(
            verifier.verify(&advertised),
            Err(DeliveryError::TipMismatch { .. })
        ));
    }

    #[test]
    fn malformed_branch_refuses_before_creating_the_custody_repo() {
        let fixture = Fixture::new();
        let delivery = GitDelivery::new(
            fixture.remote.to_str().expect("remote path"),
            "bad..branch",
            fixture.head(),
        )
        .expect("delivery shape");
        let mut verifier = GitDeliveryVerifier::new(&fixture.custody);

        assert_eq!(
            verifier.verify(&delivery),
            Err(DeliveryError::InvalidBranch)
        );
        assert!(!fixture.custody.exists());
    }
}
