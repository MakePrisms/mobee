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
