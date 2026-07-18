use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt;

use crate::delivery::{CommitOid, DeliveryError, DeliveryVerifier, GitDelivery, VerifiedDelivery};
use crate::delivery_transport::AllowlistedDeliveryVerifier;

/// Hard cap for pay-path `git fetch` — timeout fails CLOSED (no pay / zero burn).
/// Kept under MCP tool deadline (15s) and Claude-Code client read-timeout (~60s).
const GIT_FETCH_TIMEOUT_SECS: u64 = 10;

/// Real git-backed verifier that retains fetched objects in a buyer-owned repository.
///
/// Crate-private on purpose: bare git verify can fetch `ext::` (RCE). The pay path
/// must use [`PayPathDeliveryVerifier`] — the only public factory that hands out a
/// fetch-capable verifier, with the transport allowlist sealed in.
pub(crate) struct GitDeliveryVerifier {
    repository: PathBuf,
}

impl GitDeliveryVerifier {
    /// Creates a verifier whose fetched object database lives at `repository`.
    pub(crate) fn new(repository: impl Into<PathBuf>) -> Self {
        Self {
            repository: repository.into(),
        }
    }

    /// Returns the local repository that holds verified delivery objects.
    pub(crate) fn repository(&self) -> &Path {
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
        // Timed: a hung fetch must not own the MCP stdio loop past the client timeout,
        // and must fail CLOSED before authorize_pay burns budget (verify-before-pay).
        let output = git_output_timed(
            [
                OsStr::new("-C"),
                self.repository.as_os_str(),
                OsStr::new("fetch"),
                OsStr::new("--no-tags"),
                OsStr::new("--force"),
                OsStr::new("--end-of-options"),
                OsStr::new(delivery.repo()),
                OsStr::new(&refspec),
            ],
            Duration::from_secs(GIT_FETCH_TIMEOUT_SECS),
        )?;
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

    /// Contribution (MUST-2): fetch `base_oid` from the **pinned target** clone URL into the SAME
    /// custody odb, then prove `base_oid` is present in that target — fail-closed if absent.
    ///
    /// FETCH DEPTH (build-item i): this is a FULL fetch of the base branch (no `--depth`), so
    /// `base_oid` and the ancestry chain up to the fork tip are all in custody. A shallow /
    /// tip-only fetch would make the later `merge-base --is-ancestor` FALSE-REFUSE an honest deep
    /// contribution (fail-closed but broken); full depth is required for correctness.
    fn fetch_base(
        &self,
        base_clone_url: &str,
        base_branch: &str,
        base_oid: &CommitOid,
    ) -> Result<(), DeliveryError> {
        self.check_branch(base_branch)?;
        let fetched_ref = format!("refs/mobee/bases/{}", base_oid.as_str());
        let refspec = format!("+refs/heads/{base_branch}:{fetched_ref}");
        let output = git_output_timed(
            [
                OsStr::new("-C"),
                self.repository.as_os_str(),
                OsStr::new("fetch"),
                OsStr::new("--no-tags"),
                OsStr::new("--force"),
                OsStr::new("--end-of-options"),
                OsStr::new(base_clone_url),
                OsStr::new(&refspec),
            ],
            Duration::from_secs(GIT_FETCH_TIMEOUT_SECS),
        )?;
        require_success("fetch-base", output)?;
        // The pinned target MUST actually contain base_oid — resolve it as a commit in custody.
        let object = format!("{}^{{commit}}", base_oid.as_str());
        let output = git_output([
            OsStr::new("-C"),
            self.repository.as_os_str(),
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new(&object),
        ])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(DeliveryError::MissingBaseObject)
        }
    }

    /// Contribution (MUST-2): refuse unless `commit_oid` descends from `base_oid`, in-process via
    /// `git merge-base --is-ancestor`. Exit 0 = ancestor (pass); exit 1 = NOT ancestor (refuse);
    /// any other exit = fail-closed. Both oids must already be in custody.
    fn assert_descendant(
        &self,
        base_oid: &CommitOid,
        commit_oid: &CommitOid,
    ) -> Result<(), DeliveryError> {
        let output = git_output([
            OsStr::new("-C"),
            self.repository.as_os_str(),
            OsStr::new("merge-base"),
            OsStr::new("--is-ancestor"),
            OsStr::new(base_oid.as_str()),
            OsStr::new(commit_oid.as_str()),
        ])?;
        match output.status.code() {
            Some(0) => Ok(()),
            Some(1) => Err(DeliveryError::NotDescendant {
                base_oid: base_oid.as_str().to_owned(),
                commit_oid: commit_oid.as_str().to_owned(),
            }),
            _ => Err(DeliveryError::GitCommandFailed("merge-base")),
        }
    }

    /// Contribution (MUST-5): the changed paths of the fork-vs-base diff, computed by the BUYER in
    /// custody (never trusted from the seller). `bytes` per path is the numstat churn
    /// (added+deleted lines; binary files count as 1) — a deterministic size proxy for the policy
    /// cap. An empty result means an empty diff (the content-gate floor refuses it).
    fn changed_paths(
        &self,
        base_oid: &CommitOid,
        commit_oid: &CommitOid,
    ) -> Result<Vec<crate::contribution::ChangedPath>, DeliveryError> {
        let output = git_output([
            OsStr::new("-C"),
            self.repository.as_os_str(),
            OsStr::new("diff"),
            OsStr::new("--numstat"),
            OsStr::new("--no-renames"),
            OsStr::new(base_oid.as_str()),
            OsStr::new(commit_oid.as_str()),
            OsStr::new("--"),
        ])?;
        if !output.status.success() {
            return Err(DeliveryError::GitCommandFailed("diff-numstat"));
        }
        let text = String::from_utf8(output.stdout).map_err(|_| DeliveryError::GitCommandFailed("diff-numstat"))?;
        let mut changed = Vec::new();
        for line in text.lines() {
            let mut cols = line.splitn(3, '\t');
            let added = cols.next().unwrap_or("");
            let deleted = cols.next().unwrap_or("");
            let path = match cols.next() {
                Some(p) if !p.is_empty() => p.to_owned(),
                _ => continue,
            };
            // Binary files report `-` for both counts; register them as churn 1 so a binary-only
            // change is neither invisible (empty-diff false-pass) nor unbounded.
            let churn = added.parse::<u64>().unwrap_or(0) + deleted.parse::<u64>().unwrap_or(0);
            let churn = if added == "-" || deleted == "-" { churn.max(1) } else { churn };
            changed.push(crate::contribution::ChangedPath { path, bytes: churn });
        }
        Ok(changed)
    }

    /// Contribution buyer verify-path orchestration (the ONE state machine, all pre-pay). Bare —
    /// the transport allowlist is applied by [`PayPathDeliveryVerifier::verify_contribution`] BEFORE
    /// this runs. In order: custody-fetch the fork tip + tip-match, base-from-pin, descendant gate,
    /// content gate. Returns the custody proof + local ref for the later merge.
    fn contribution_verify(
        &mut self,
        fork: &GitDelivery,
        base_clone_url: &str,
        base_branch: &str,
        base_oid: &CommitOid,
        policy: &crate::contribution::ContentPolicy,
    ) -> Result<VerifiedContribution, DeliveryError> {
        // 1. custody fetch fork tip + tip-match (retains the object under refs/mobee/deliveries/…).
        let verified = self.verify(fork)?;
        // 2. base-from-pin into the same custody odb (fail-closed if absent from the pinned target).
        self.fetch_base(base_clone_url, base_branch, base_oid)?;
        // 3. descendant gate.
        self.assert_descendant(base_oid, verified.commit_oid())?;
        // 4. content gate + buyer policy hook.
        let changed = self.changed_paths(base_oid, verified.commit_oid())?;
        policy
            .evaluate(&changed)
            .map_err(|refusal| DeliveryError::ContentRefused(refusal.to_string()))?;
        Ok(VerifiedContribution {
            custody_local_ref: PayPathDeliveryVerifier::custody_ref_for(verified.commit_oid().as_str()),
            verified,
            changed_paths: changed,
        })
    }
}

/// Proof that a contribution fork tip is in buyer custody AND descends from the pinned base, with
/// the content gate satisfied. Carries the LOCAL custody ref so the merge step operates on the
/// custodied oid, never the live fork branch (custody-retention, MUST-6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedContribution {
    verified: VerifiedDelivery,
    custody_local_ref: String,
    changed_paths: Vec<crate::contribution::ChangedPath>,
}

impl VerifiedContribution {
    /// The verified fork-tip delivery (the object the money path binds — unchanged `Commit`).
    pub fn verified(&self) -> &VerifiedDelivery {
        &self.verified
    }

    /// The BUYER-CONTROLLED local ref holding the custodied fork tip (recorded in the accept-bind;
    /// merge uses THIS, never the live fork branch name).
    pub fn custody_local_ref(&self) -> &str {
        &self.custody_local_ref
    }

    /// The changed paths the content gate evaluated (buyer-computed in custody).
    pub fn changed_paths(&self) -> &[crate::contribution::ChangedPath] {
        &self.changed_paths
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

/// Pay-path delivery verifier: transport allowlist sealed around git verify/fetch.
///
/// This is the only public constructor that yields a fetch-capable verifier for
/// `authorize_pay` / MCP. There is no peel API — the bare git verifier cannot be
/// reached from outside `mobee-core`.
pub struct PayPathDeliveryVerifier {
    inner: AllowlistedDeliveryVerifier<GitDeliveryVerifier>,
}

impl PayPathDeliveryVerifier {
    /// Build the allowlisted git verifier used on the authorize_pay path.
    pub fn new(repository: impl Into<PathBuf>) -> Self {
        Self {
            inner: AllowlistedDeliveryVerifier::new(GitDeliveryVerifier::new(repository)),
        }
    }

    /// Returns the local repository that holds verified delivery objects.
    pub fn repository(&self) -> &Path {
        self.inner.inner().repository()
    }

    /// The buyer-controlled custody ref a fork tip is retained under (MUST-6 custody-retention).
    pub fn custody_ref_for(commit_oid: &str) -> String {
        format!("refs/mobee/deliveries/{commit_oid}")
    }

    /// Contribution (piece-10) buyer verify-path — the ONE state machine, ALL pre-pay, ALL against
    /// BUYER-CONTROLLED inputs (`fork` = the delivered object; `base_*` come from the buyer's
    /// SIGNED offer pin, NEVER the seller echo). In order:
    ///   1. custody fetch the fork tip + tip-match (existing `Delivery::Commit` verify, allowlisted);
    ///   2. base-from-pin: fetch `base_oid` from the PINNED target clone URL into the same custody
    ///      odb (allowlisted) — fail-closed if absent from the pinned target;
    ///   3. descendant gate: `merge-base --is-ancestor base_oid commit_oid`;
    ///   4. content gate + policy hook: refuse an empty / out-of-scope / forbidden / too-large diff.
    /// Returns the custody proof + LOCAL custody ref for the later merge. Authorship (the seller's
    /// signed tuple) + echo-equality are checked by the caller at the pre-pay seam; pay then binds
    /// the fork-tip `commit_oid` via the unchanged money path.
    pub fn verify_contribution(
        &mut self,
        fork: &GitDelivery,
        base_clone_url: &str,
        base_branch: &str,
        base_oid: &CommitOid,
        policy: &crate::contribution::ContentPolicy,
    ) -> Result<VerifiedContribution, DeliveryError> {
        // BOTH the fork fetch AND the base-from-pin fetch go through the SAME transport allowlist
        // (https + relay-git; `ext::`/file/ssh refused) — defense-in-depth even though the base pin
        // is buyer-signed. Enforced here BEFORE the bare orchestration touches git.
        crate::delivery_transport::assert_allowed_repo_locator(fork.repo())?;
        crate::delivery_transport::assert_allowed_repo_locator(base_clone_url)?;
        self.inner
            .inner_mut()
            .contribution_verify(fork, base_clone_url, base_branch, base_oid, policy)
    }

    /// Merge the CUSTODIED local `commit_oid` into a buyer-owned target working clone, FF-preferred
    /// ("accept the PR"). Buyer-custody action (NOT what payment binds): it fetches the object from
    /// the buyer's own custody odb by its LOCAL ref and `merge --ff-only`s it — so a seller that
    /// deletes or moves the fork AFTER pay cannot strand the buyer (custody-retention). No transport
    /// allowlist applies: both custody and the target clone are buyer-owned local repos.
    pub fn merge_custodied_commit(
        &self,
        target_workdir: &Path,
        commit_oid: &CommitOid,
    ) -> Result<(), DeliveryError> {
        let custody = self.repository();
        let local_ref = Self::custody_ref_for(commit_oid.as_str());
        let fetch_spec = format!("+{local_ref}:refs/mobee/merge/{}", commit_oid.as_str());
        let output = git_output([
            OsStr::new("-C"),
            target_workdir.as_os_str(),
            OsStr::new("fetch"),
            OsStr::new("--no-tags"),
            OsStr::new("--end-of-options"),
            custody.as_os_str(),
            OsStr::new(&fetch_spec),
        ])?;
        require_success_merge("fetch-from-custody", output)?;
        let output = git_output([
            OsStr::new("-C"),
            target_workdir.as_os_str(),
            OsStr::new("merge"),
            OsStr::new("--ff-only"),
            OsStr::new("--end-of-options"),
            OsStr::new(commit_oid.as_str()),
        ])?;
        require_success_merge("ff-only", output)
    }
}

fn require_success_merge(operation: &'static str, output: Output) -> Result<(), DeliveryError> {
    if output.status.success() {
        Ok(())
    } else {
        Err(DeliveryError::MergeFailed(operation))
    }
}

impl DeliveryVerifier for PayPathDeliveryVerifier {
    fn verify(&mut self, delivery: &GitDelivery) -> Result<VerifiedDelivery, DeliveryError> {
        self.inner.verify(delivery)
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
        .map_err(|e| DeliveryError::GitSpawnFailed {
            program: "git",
            kind: e.kind(),
        })
}

/// Like [`git_output`], but bounds the child by `timeout` **in-process** and fails CLOSED.
///
/// Spawns `git` directly (no external `timeout(1)` — macOS ships none, and BusyBox's exit
/// codes differ) and waits with [`wait_timeout`](wait_timeout::ChildExt::wait_timeout):
/// - the child exits in-window → return its real [`Output`] so `require_success` sees the
///   true status (a failing fetch stays a failure; there is no fail-OPEN exit-code class);
/// - the timeout expires → kill + reap the child and return `GitCommandFailed("fetch-timeout")`,
///   so a hung fetch never owns the MCP stdio loop and never yields a verified delivery.
///
/// stdout/stderr are drained on reader threads while we wait: a fetch chatty enough to fill
/// the ~64KB pipe buffer would otherwise block on write and never exit, and a genuinely
/// successful large fetch must still be able to complete within the window.
fn git_output_timed<I, S>(args: I, timeout: Duration) -> Result<Output, DeliveryError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut child = Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| DeliveryError::GitSpawnFailed {
            program: "git",
            kind: e.kind(),
        })?;

    // Drain both pipes concurrently so a chatty fetch can't deadlock on a full pipe buffer
    // before it exits (or before the timeout fires).
    let stdout = child.stdout.take().expect("piped stdout is present");
    let stderr = child.stderr.take().expect("piped stderr is present");
    let stdout_reader = thread::spawn(move || drain_to_end(stdout));
    let stderr_reader = thread::spawn(move || drain_to_end(stderr));

    match child
        .wait_timeout(timeout)
        .map_err(|e| DeliveryError::GitSpawnFailed {
            program: "git",
            kind: e.kind(),
        })? {
        Some(status) => {
            // Child exited on its own → its pipe write ends are closed, so the readers
            // hit EOF and join promptly with the full output.
            let stdout = stdout_reader.join().unwrap_or_default();
            let stderr = stderr_reader.join().unwrap_or_default();
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
        None => {
            // Fail CLOSED: kill the hung fetch and reap it, then report a timeout WITHOUT
            // blocking on the readers. Joining here could re-hang if an orphaned transport
            // helper still held a pipe open, which would defeat the whole point of the
            // timeout (a hung fetch must not own the MCP stdio loop). We discard output on
            // the timeout path anyway; the detached readers exit once the pipes close.
            let _ = child.kill();
            let _ = child.wait();
            Err(DeliveryError::GitCommandFailed("fetch-timeout"))
        }
    }
}

/// Reads a child pipe to EOF, discarding any read error (best-effort output capture).
fn drain_to_end<R: Read>(mut reader: R) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = reader.read_to_end(&mut buffer);
    buffer
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

    #[test]
    fn pay_path_verifier_refuses_ext_before_fetch() {
        use crate::delivery_transport::TransportRefuse;

        let fixture = Fixture::new();
        let delivery = GitDelivery::new(
            "ext::sh -c evil",
            "main",
            fixture.head(),
        )
        .expect("delivery shape");
        let mut verifier = PayPathDeliveryVerifier::new(&fixture.custody);
        let err = verifier.verify(&delivery).expect_err("refuse ext");
        assert!(matches!(
            err,
            DeliveryError::Transport(TransportRefuse::ForbiddenScheme(_))
        ));
        assert!(!fixture.custody.exists());
    }

    #[test]
    fn pay_path_verifier_allows_local_path_only_via_bare_inner_in_tests() {
        // Hermetic tip-match tests still use bare GitDeliveryVerifier (pub(crate)).
        // Pay path must not be that type — factory always allowlists first.
        let fixture = Fixture::new();
        let advertised = fixture.delivery(fixture.head());
        let mut pay_path = PayPathDeliveryVerifier::new(&fixture.custody);
        let err = pay_path.verify(&advertised).expect_err("local path refused");
        assert!(matches!(
            err,
            DeliveryError::Transport(crate::delivery_transport::TransportRefuse::LocalPath)
        ));
    }

    #[test]
    fn hanging_remote_fetch_fails_closed_via_in_process_timeout() {
        use std::net::TcpListener;
        use std::time::Instant;

        // Deterministic "remote hangs" reproduction: a local listener that accepts git's
        // connection, consumes its request, then never answers the ref advertisement — so
        // git blocks reading. git:// uses git's built-in transport (no helper subprocess),
        // so killing the child fully tears the fetch down.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind hang listener");
        let port = listener.local_addr().expect("listener addr").port();
        let accepter = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut scratch = [0u8; 256];
                let _ = stream.read(&mut scratch);
                // Hold the socket open so git keeps blocking. Bounded so that if the
                // in-process kill were removed (red-on-revert), git eventually errors
                // rather than hanging the suite forever.
                thread::sleep(Duration::from_secs(10));
                drop(stream);
            }
        });
        // Detach: the fixed path kills git well before this sleep ends; joining would make
        // the passing test wait out the full hold.
        drop(accepter);

        // git fetch needs a real local repo to fetch *into* before it reaches the transport.
        let fixture = Fixture::new();
        run(
            ["init", "--bare", fixture.custody.to_str().expect("custody path")],
            None,
        );

        let url = format!("git://127.0.0.1:{port}/hang.git");
        let refspec = "+refs/heads/main:refs/mobee/deliveries/hang";
        let timeout = Duration::from_secs(1);

        let started = Instant::now();
        let result = git_output_timed(
            [
                OsStr::new("-C"),
                fixture.custody.as_os_str(),
                OsStr::new("fetch"),
                OsStr::new("--no-tags"),
                OsStr::new("--force"),
                OsStr::new("--end-of-options"),
                OsStr::new(&url),
                OsStr::new(refspec),
            ],
            timeout,
        );
        let elapsed = started.elapsed();

        // FAIL-CLOSED: an unresponsive fetch surfaces as a timeout error, never as a success
        // `Output` that could slip through `require_success` into a paid delivery.
        assert_eq!(
            result,
            Err(DeliveryError::GitCommandFailed("fetch-timeout")),
            "hung fetch must fail closed as a timeout"
        );
        // PROMPT: killed near the 1s window by the in-process timeout, not left to hang on
        // git's (indefinite) native-protocol read. Generous epsilon keeps this non-flaky.
        assert!(
            elapsed < Duration::from_secs(8),
            "hung fetch must be killed promptly; took {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod contribution_tests {
    use super::*;
    use crate::contribution::{ChangedPath, ContentPolicy};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(1);

    fn git<const N: usize>(args: [&str; N], cwd: &Path) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.invalid")
            .output()
            .expect("git")
    }

    fn ok<const N: usize>(args: [&str; N], cwd: &Path) {
        let out = git(args, cwd);
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn oid(cwd: &Path, rev: &str) -> CommitOid {
        let out = git(["rev-parse", rev], cwd);
        assert!(out.status.success(), "rev-parse {rev}");
        CommitOid::parse(String::from_utf8(out.stdout).unwrap().trim()).expect("oid")
    }

    fn commit(cwd: &Path, path: &str, content: &str, msg: &str) {
        let full = cwd.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&full, content).expect("write");
        ok(["add", "-A"], cwd);
        ok(["commit", "-m", msg], cwd);
    }

    /// A contribution scenario: a PINNED target repo (base branch with `base_oid` as an ancestor of
    /// its tip) + a seller FORK descending from `base_oid` with `depth` agent commits on top.
    struct Fx {
        root: PathBuf,
        target_git: PathBuf,
        fork_git: PathBuf,
        custody: PathBuf,
        base_oid: CommitOid,
        fork_tip: CommitOid,
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn scenario(depth: usize, change_path: &str) -> Fx {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("mobee-contrib-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let target_work = root.join("target_work");
        let target_git = root.join("target.git");
        let fork_work = root.join("fork_work");
        let fork_git = root.join("fork.git");
        let custody = root.join("custody.git");
        fs::create_dir_all(&target_work).expect("mk");
        ok(["init", "--initial-branch=main"], &target_work);
        commit(&target_work, "README.md", "base one\n", "base one");
        let base_oid = oid(&target_work, "HEAD");
        // Advance main PAST base_oid so base_oid is an ANCESTOR (base-from-pin must find it in history).
        commit(&target_work, "README.md", "base two\n", "base two");
        ok(["init", "--bare", target_git.to_str().unwrap()], &root);
        ok(["remote", "add", "origin", target_git.to_str().unwrap()], &target_work);
        ok(["push", "origin", "main"], &target_work);

        // Fork: clone target, check out base_oid, add `depth` agent commits on top, push a branch.
        ok(["clone", target_git.to_str().unwrap(), fork_work.to_str().unwrap()], &root);
        ok(["checkout", "-B", "contribution", base_oid.as_str()], &fork_work);
        for i in 0..depth.max(1) {
            commit(&fork_work, change_path, &format!("agent work {i}\n"), &format!("agent {i}"));
        }
        let fork_tip = oid(&fork_work, "HEAD");
        ok(["init", "--bare", fork_git.to_str().unwrap()], &root);
        ok(["remote", "add", "fork", fork_git.to_str().unwrap()], &fork_work);
        ok(["push", "fork", "contribution"], &fork_work);

        Fx { root, target_git, fork_git, custody, base_oid, fork_tip }
    }

    fn fork_delivery(fx: &Fx) -> GitDelivery {
        GitDelivery::new(fx.fork_git.to_str().unwrap(), "contribution", fx.fork_tip.clone())
            .expect("fork delivery")
    }

    // ── Descendant + tip-match + base-from-pin: a valid DEEP-history contribution passes (item i:
    //    a full-depth custody fetch reaches base_oid, so the descendant gate never false-refuses). ──
    #[test]
    fn deep_history_contribution_verifies() {
        let fx = scenario(5, "src/feature.rs");
        let mut v = GitDeliveryVerifier::new(&fx.custody);
        let verified = v
            .contribution_verify(
                &fork_delivery(&fx),
                fx.target_git.to_str().unwrap(),
                "main",
                &fx.base_oid,
                &ContentPolicy::floor(),
            )
            .expect("deep-history contribution must verify");
        assert_eq!(verified.verified().commit_oid(), &fx.fork_tip);
        assert_eq!(verified.custody_local_ref(), PayPathDeliveryVerifier::custody_ref_for(fx.fork_tip.as_str()));
        assert!(!verified.changed_paths().is_empty());
    }

    // ── Descendant gate refuses unrelated history / swapped base ──────────────────────────────
    #[test]
    fn unrelated_history_refused_as_not_descendant() {
        let fx = scenario(1, "src/feature.rs");
        // An ORPHAN base_oid (a commit created independently, not in the fork's history).
        let orphan_work = fx.root.join("orphan");
        fs::create_dir_all(&orphan_work).unwrap();
        ok(["init", "--initial-branch=main"], &orphan_work);
        commit(&orphan_work, "x.txt", "orphan\n", "orphan");
        let orphan_oid = oid(&orphan_work, "HEAD");
        // Put the orphan object into custody (as if a base fetch produced it) so the descendant
        // check is the thing under test, not object-presence.
        let mut v = GitDeliveryVerifier::new(&fx.custody);
        v.verify(&fork_delivery(&fx)).expect("fetch fork tip");
        ok(["init", "--bare", fx.custody.join("dummy").to_str().unwrap_or("dummy")], &fx.root);
        // fetch the orphan commit into custody directly
        let spec = format!("+refs/heads/main:refs/mobee/bases/{}", orphan_oid.as_str());
        ok(["-C", fx.custody.to_str().unwrap(), "fetch", orphan_work.to_str().unwrap(), &spec], &fx.root);
        assert!(matches!(
            v.assert_descendant(&orphan_oid, &fx.fork_tip),
            Err(DeliveryError::NotDescendant { .. })
        ));
    }

    #[test]
    fn swapped_base_that_is_not_an_ancestor_refused() {
        // base = the fork TIP's child? No — use a sibling: a second fork off base with different work.
        let fx = scenario(1, "src/a.rs");
        // Build a sibling commit off base in another clone (a DIFFERENT descendant of base).
        let sib = fx.root.join("sib");
        ok(["clone", fx.target_git.to_str().unwrap(), sib.to_str().unwrap()], &fx.root);
        ok(["checkout", "-B", "sib", fx.base_oid.as_str()], &sib);
        commit(&sib, "src/b.rs", "sibling\n", "sibling");
        let sib_oid = oid(&sib, "HEAD");
        let mut v = GitDeliveryVerifier::new(&fx.custody);
        v.verify(&fork_delivery(&fx)).expect("fetch fork tip");
        let spec = format!("+refs/heads/sib:refs/mobee/x/{}", sib_oid.as_str());
        ok(["-C", fx.custody.to_str().unwrap(), "fetch", sib.to_str().unwrap(), &spec], &fx.root);
        // The sibling is NOT an ancestor of the fork tip ⇒ swapped-base refuse.
        assert!(matches!(
            v.assert_descendant(&sib_oid, &fx.fork_tip),
            Err(DeliveryError::NotDescendant { .. })
        ));
    }

    // ── Base-from-pin: base_oid absent from the pinned target ⇒ fail-closed refuse ────────────
    #[test]
    fn base_absent_from_pinned_target_fails_closed() {
        let fx = scenario(1, "src/feature.rs");
        // An ORPHAN base_oid — a commit that is NOT on the target's main branch AND not in the
        // fork's history (so it is absent from custody after both the fork and the base fetch).
        let orphan = fx.root.join("orphan_base");
        fs::create_dir_all(&orphan).unwrap();
        ok(["init", "--initial-branch=main"], &orphan);
        commit(&orphan, "z.txt", "orphan base\n", "orphan base");
        let bogus_base = oid(&orphan, "HEAD");
        let mut v = GitDeliveryVerifier::new(&fx.custody);
        v.verify(&fork_delivery(&fx)).expect("fetch fork tip");
        // base-from-pin fetches the target's main (base + base_two) but the orphan is not there ⇒
        // fail-closed MissingBaseObject (the buyer never bases against an oid absent from the pin).
        assert!(matches!(
            v.fetch_base(fx.target_git.to_str().unwrap(), "main", &bogus_base),
            Err(DeliveryError::MissingBaseObject)
        ));
    }

    #[test]
    fn base_from_pin_finds_ancestor_base() {
        let fx = scenario(2, "src/feature.rs");
        let mut v = GitDeliveryVerifier::new(&fx.custody);
        v.verify(&fork_delivery(&fx)).expect("fetch fork tip");
        // base_oid is an ANCESTOR of the target's main tip — base-from-pin must resolve it.
        v.fetch_base(fx.target_git.to_str().unwrap(), "main", &fx.base_oid)
            .expect("base-from-pin must find the ancestor base_oid in the pinned target");
        v.assert_descendant(&fx.base_oid, &fx.fork_tip).expect("descends");
    }

    // ── Content gate: empty / out-of-scope refuse; in-scope passes ────────────────────────────
    #[test]
    fn content_gate_refuses_empty_diff() {
        // A fork whose tip has the SAME tree as base (an empty commit) ⇒ empty diff.
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("mobee-contrib-empty-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let tw = root.join("tw");
        let tg = root.join("t.git");
        let fw = root.join("fw");
        let fg = root.join("f.git");
        let custody = root.join("c.git");
        fs::create_dir_all(&tw).unwrap();
        ok(["init", "--initial-branch=main"], &tw);
        commit(&tw, "README.md", "base\n", "base");
        let base_oid = oid(&tw, "HEAD");
        ok(["init", "--bare", tg.to_str().unwrap()], &root);
        ok(["remote", "add", "origin", tg.to_str().unwrap()], &tw);
        ok(["push", "origin", "main"], &tw);
        ok(["clone", tg.to_str().unwrap(), fw.to_str().unwrap()], &root);
        ok(["checkout", "-B", "contribution", base_oid.as_str()], &fw);
        ok(["commit", "--allow-empty", "-m", "empty"], &fw);
        let fork_tip = oid(&fw, "HEAD");
        ok(["init", "--bare", fg.to_str().unwrap()], &root);
        ok(["remote", "add", "fork", fg.to_str().unwrap()], &fw);
        ok(["push", "fork", "contribution"], &fw);
        let mut v = GitDeliveryVerifier::new(&custody);
        let err = v
            .contribution_verify(
                &GitDelivery::new(fg.to_str().unwrap(), "contribution", fork_tip).unwrap(),
                tg.to_str().unwrap(),
                "main",
                &base_oid,
                &ContentPolicy::floor(),
            )
            .expect_err("empty diff must refuse");
        assert!(matches!(err, DeliveryError::ContentRefused(_)), "got {err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn content_gate_scope_out_of_scope_refuses_in_scope_passes() {
        // out-of-scope: change under "other/", policy allows only "src".
        let fx = scenario(1, "other/x.rs");
        let mut v = GitDeliveryVerifier::new(&fx.custody);
        let policy = ContentPolicy { allowed_paths: vec!["src".into()], ..Default::default() };
        let err = v
            .contribution_verify(&fork_delivery(&fx), fx.target_git.to_str().unwrap(), "main", &fx.base_oid, &policy)
            .expect_err("out-of-scope refuse");
        assert!(matches!(err, DeliveryError::ContentRefused(_)), "got {err}");

        // in-scope: change under "src/", same policy → pass.
        let fx2 = scenario(1, "src/ok.rs");
        let mut v2 = GitDeliveryVerifier::new(&fx2.custody);
        v2.contribution_verify(&fork_delivery(&fx2), fx2.target_git.to_str().unwrap(), "main", &fx2.base_oid, &policy)
            .expect("in-scope must pass");
    }

    // ── Custody retention (the STRAND-PROOF): after verify, the object is in buyer custody; the
    //    fork moving/deleting cannot strand the buyer — merge succeeds from the custodied local oid. ─
    #[test]
    fn custody_retention_survives_fork_deletion_and_merges_from_local_oid() {
        let fx = scenario(1, "src/feature.rs");
        let pay = PayPathDeliveryVerifier::new(&fx.custody);
        // Custody-fetch via the bare verifier (local path; allowlist tested separately).
        let mut bare = GitDeliveryVerifier::new(&fx.custody);
        let verified = bare
            .contribution_verify(&fork_delivery(&fx), fx.target_git.to_str().unwrap(), "main", &fx.base_oid, &ContentPolicy::floor())
            .expect("verify");
        // Simulate the seller DELETING the fork remote after accept.
        fs::remove_dir_all(&fx.fork_git).expect("delete fork");
        // The object is RETAINED in custody (a deleted fork cannot strand the buyer).
        bare.require_local_object(&fx.fork_tip).expect("custodied object retained after fork deletion");
        // Merge the CUSTODIED local oid into a target working clone, FF-preferred.
        let target_clone = fx.root.join("accept_pr");
        ok(["clone", fx.target_git.to_str().unwrap(), target_clone.to_str().unwrap()], &fx.root);
        ok(["checkout", fx.base_oid.as_str()], &target_clone);
        pay.merge_custodied_commit(&target_clone, &fx.fork_tip)
            .expect("merge from custody must succeed by LOCAL oid even after fork deletion");
        assert_eq!(oid(&target_clone, "HEAD"), fx.fork_tip, "FF merge lands the paid fork-tip oid");
        assert_eq!(verified.verified().commit_oid(), &fx.fork_tip);
    }

    // ── Allowlist: the pay-path contribution verify refuses ext:: for BOTH fork and base ──────
    #[test]
    fn pay_path_contribution_verify_refuses_ext_transport() {
        use crate::delivery_transport::TransportRefuse;
        let fx = scenario(1, "src/feature.rs");
        let mut pay = PayPathDeliveryVerifier::new(&fx.custody);
        // ext:: fork repo refused before any fetch.
        let ext_fork = GitDelivery::new("ext::sh -c evil", "contribution", fx.fork_tip.clone()).unwrap();
        assert!(matches!(
            pay.verify_contribution(&ext_fork, fx.target_git.to_str().unwrap(), "main", &fx.base_oid, &ContentPolicy::floor()),
            Err(DeliveryError::Transport(TransportRefuse::ForbiddenScheme(_)))
        ));
        // ext:: base clone url refused too — the fork here is an allowlisted https URL so the base
        // is the value under test (neither fetch runs; both URLs are allowlist-checked up front).
        let https_fork =
            GitDelivery::new("https://example.invalid/fork.git", "contribution", fx.fork_tip.clone())
                .unwrap();
        assert!(matches!(
            pay.verify_contribution(&https_fork, "ext::sh -c evil", "main", &fx.base_oid, &ContentPolicy::floor()),
            Err(DeliveryError::Transport(TransportRefuse::ForbiddenScheme(_)))
        ));
    }
}
