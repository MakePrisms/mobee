//! `mobee doctor` — seller environment self-check.
//!
//! Runs a registry of independent checks, each printing `PASS`/`WARN`/`FAIL` plus a one-line fix
//! hint, and exits `0` when nothing FAILed, `1` when any check FAILed (a WARN never fails the exit).
//! Every check runs even when an earlier one fails, so one run surfaces the full picture.
//!
//! The load-bearing motivation is the boot-time field failure: git 2.53 and earlier silently drop
//! the Authorization credential on the git-receive-pack POST (reads work, pushes 401), so a seller
//! looks healthy until the first delivery. The git-version check is the definitive detector for that
//! class; the relay/mint/helper/agent checks catch the rest of the "can this box actually sell"
//! surface.

use std::io::Write;

const SUCCESS: i32 = 0;
const FAILURE: i32 = 1;

/// One check outcome. `Warn` is advisory (does not fail the exit); `Fail` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

/// A single named check result plus an optional one-line fix hint (shown only when not `Pass`).
#[derive(Debug, Clone)]
struct Check {
    name: String,
    status: Status,
    detail: String,
    hint: Option<String>,
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>, hint: Option<&str>) -> Self {
        Self {
            name: name.to_owned(),
            status,
            detail: detail.into(),
            hint: hint.map(str::to_owned),
        }
    }

    fn pass(name: &str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Pass, detail, None)
    }

    fn warn(name: &str, detail: impl Into<String>, hint: &str) -> Self {
        Self::new(name, Status::Warn, detail, Some(hint))
    }

    fn fail(name: &str, detail: impl Into<String>, hint: &str) -> Self {
        Self::new(name, Status::Fail, detail, Some(hint))
    }

    fn render(&self) -> String {
        let base = format!("{:<4} {} — {}", self.status.label(), self.name, self.detail);
        match &self.hint {
            Some(hint) if self.status != Status::Pass => format!("{base} (fix: {hint})"),
            _ => base,
        }
    }
}

/// Run every check in order, collecting all results — a `Fail` NEVER short-circuits later checks.
fn run_checks(checks: Vec<Box<dyn FnOnce() -> Check>>) -> Vec<Check> {
    checks.into_iter().map(|check| check()).collect()
}

/// Exit code for a set of results: `1` if ANY check FAILed, else `0`. WARN never fails the exit.
fn exit_code(results: &[Check]) -> i32 {
    if results.iter().any(|c| c.status == Status::Fail) {
        FAILURE
    } else {
        SUCCESS
    }
}

/// Parse `git version` output (e.g. `git version 2.54.1` or `git version 2.39.5 (Apple Git-154)`)
/// into `(major, minor)`. Returns `None` when no version token is recognizable.
fn parse_git_version(output: &str) -> Option<(u64, u64)> {
    let token = output
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut parts = token.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    Some((major, minor))
}

/// git < 2.54 silently drops the Authorization credential on the git-receive-pack POST (reads work,
/// pushes 401). A working push path needs `(major, minor) >= (2, 54)`.
fn git_version_ok(version: (u64, u64)) -> bool {
    version >= (2, 54)
}

const GIT_VERSION_CHECK: &str = "git version";

fn check_git_version() -> Check {
    let raw = match std::process::Command::new("git").arg("version").output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
        _ => {
            return Check::warn(
                GIT_VERSION_CHECK,
                "git not found on PATH",
                "install git 2.54+ (older git drops push credentials)",
            );
        }
    };
    match parse_git_version(&raw) {
        None => Check::warn(
            GIT_VERSION_CHECK,
            format!("could not parse git version from {raw:?}"),
            "ensure `git version` reports a standard X.Y.Z string; want 2.54+",
        ),
        Some(version) if git_version_ok(version) => {
            Check::pass(GIT_VERSION_CHECK, format!("git {}.{}", version.0, version.1))
        }
        Some((major, minor)) => Check::fail(
            GIT_VERSION_CHECK,
            format!("git {major}.{minor} drops push credentials on the receive-pack POST"),
            "upgrade to git 2.54+",
        ),
    }
}

#[cfg(feature = "wallet")]
mod checks {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::time::Duration;

    use mobee_core::doctor::{self, RelayProbe};
    use mobee_core::home::{AgentPresetConfig, SellerConfig};
    use mobee_core::seller_git;

    use super::Check;
    use crate::agent_presets;

    const RELAY_TIMEOUT: Duration = Duration::from_secs(15);
    const MINT_TIMEOUT: Duration = Duration::from_secs(10);

    const CREDENTIAL_HELPER_CHECK: &str = "credential helper";
    const RELAY_CHECK: &str = "relay reachability";
    const MINT_CHECK: &str = "mint reachability";
    const AGENT_CHECK: &str = "agent preset";

    pub(super) fn check_credential_helper() -> Check {
        match seller_git::resolve_git_credential_nostr() {
            Some(path) => Check::pass(
                CREDENTIAL_HELPER_CHECK,
                format!("git-credential-nostr at {}", path.display()),
            ),
            None => Check::fail(
                CREDENTIAL_HELPER_CHECK,
                "git-credential-nostr not resolvable",
                "install it, add it to PATH, or set MOBEE_GIT_CREDENTIAL_NOSTR to its path",
            ),
        }
    }

    pub(super) fn check_relay(relay_url: String, secret: Option<String>) -> Check {
        let Some(secret) = secret else {
            return Check::warn(
                RELAY_CHECK,
                format!("{relay_url}: seller key unreadable — cannot test NIP-42 auth"),
                "ensure ~/.mobee/key exists and is readable (mode 0600)",
            );
        };
        let outcome = match build_runtime() {
            Ok(runtime) => runtime.block_on(doctor::probe_relay(&relay_url, &secret, RELAY_TIMEOUT)),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(RelayProbe::Authenticated) => {
                Check::pass(RELAY_CHECK, format!("{relay_url}: connected + NIP-42 authenticated"))
            }
            Ok(RelayProbe::ConnectedNoChallenge) => Check::pass(
                RELAY_CHECK,
                format!("{relay_url}: connected (relay issued no NIP-42 challenge)"),
            ),
            Err(error) => Check::fail(
                RELAY_CHECK,
                format!("{relay_url}: {error}"),
                "check relay_url in config.toml and network/relay availability",
            ),
        }
    }

    pub(super) fn check_mint(mint_url: String) -> Check {
        let outcome = match build_runtime() {
            Ok(runtime) => runtime.block_on(doctor::probe_mint(&mint_url, MINT_TIMEOUT)),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(()) => Check::pass(MINT_CHECK, format!("{mint_url}: /v1/info reachable")),
            Err(error) => Check::fail(
                MINT_CHECK,
                format!("{mint_url}: {error}"),
                "check the mint URL and network availability",
            ),
        }
    }

    pub(super) fn check_agent_preset(
        seller: Option<SellerConfig>,
        custom: BTreeMap<String, AgentPresetConfig>,
    ) -> Check {
        let Some(seller) = seller else {
            return Check::warn(
                AGENT_CHECK,
                "no [seller] section configured",
                "run `mobee sell --agent <claude|cursor|codex> --rate-sats <n>` once to configure",
            );
        };
        let available = agent_presets::detect_available_agents(&custom);
        let (label, argv) = match seller.agent.as_deref() {
            Some(name) => match agent_presets::resolve_agent_preset(name, &custom) {
                Ok(pair) => pair,
                Err(message) => {
                    return Check::fail(
                        AGENT_CHECK,
                        message,
                        "set [seller] agent to claude|cursor|codex or a configured [agents] preset",
                    );
                }
            },
            None => ("custom".to_owned(), seller.agent_command.clone()),
        };
        let argv0 = argv.first().cloned().unwrap_or_default();
        if available.contains(&label) || argv0_resolvable(&argv0) {
            Check::pass(AGENT_CHECK, format!("agent '{label}' resolvable (argv0={argv0})"))
        } else {
            Check::fail(
                AGENT_CHECK,
                format!("agent '{label}' not found (argv0={argv0})"),
                "install the agent harness or fix [seller] agent / [agents]",
            )
        }
    }

    fn build_runtime() -> Result<tokio::runtime::Runtime, String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("tokio runtime: {error}"))
    }

    /// True when `argv0` names a runnable program: an existing file path, or a bare name found on
    /// PATH. Mirrors how the seller daemon would launch it.
    fn argv0_resolvable(argv0: &str) -> bool {
        if argv0.is_empty() {
            return false;
        }
        if Path::new(argv0).is_file() {
            return true;
        }
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| dir.join(argv0).is_file())
    }
}

/// Entry from `cli::run` for `mobee doctor`.
pub fn run(_args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    #[cfg(not(feature = "wallet"))]
    {
        let _ = out;
        let _ = writeln!(
            err,
            "mobee doctor requires the wallet feature (rebuild with default features)"
        );
        return FAILURE;
    }

    #[cfg(feature = "wallet")]
    {
        run_doctor(out, err)
    }
}

#[cfg(feature = "wallet")]
fn run_doctor(out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use mobee_core::home;

    let root = match home::default_home_dir() {
        Ok(root) => root,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return FAILURE;
        }
    };
    let home = match home::bootstrap(&root) {
        Ok(home) => home,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return FAILURE;
        }
    };

    let _ = writeln!(out, "mobee doctor — seller environment self-check (home={})", home.root.display());

    let relay_url = home.config.relay_url.clone();
    // Read the seller key only to test NIP-42 auth; it is NEVER printed or put in any Check detail.
    let secret = home::read_secret_key_hex(&home).ok();
    let mint_urls: Vec<String> = std::iter::once(home.config.mint_url.clone())
        .chain(home.config.extra_mints.clone())
        .collect();
    let seller = home.config.seller.clone();
    let custom_agents = home.config.agents.clone();

    let mut checks: Vec<Box<dyn FnOnce() -> Check>> = vec![
        Box::new(check_git_version),
        Box::new(checks::check_credential_helper),
        Box::new(move || checks::check_relay(relay_url, secret)),
    ];
    for mint_url in mint_urls {
        checks.push(Box::new(move || checks::check_mint(mint_url)));
    }
    checks.push(Box::new(move || checks::check_agent_preset(seller, custom_agents)));

    let results = run_checks(checks);
    for result in &results {
        let _ = writeln!(out, "{}", result.render());
    }
    let code = exit_code(&results);
    let _ = writeln!(
        out,
        "\n{} check(s), exit {code}",
        results.len()
    );
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_version_parse_and_verdict() {
        assert_eq!(parse_git_version("git version 2.54.0"), Some((2, 54)));
        assert_eq!(parse_git_version("git version 2.53.9"), Some((2, 53)));
        assert_eq!(parse_git_version("git version 2.55.1"), Some((2, 55)));
        assert_eq!(parse_git_version("git version 3.0.0"), Some((3, 0)));
        // Vendor suffixes (Apple / Windows) still parse the leading X.Y.
        assert_eq!(
            parse_git_version("git version 2.39.5 (Apple Git-154)"),
            Some((2, 39))
        );
        assert_eq!(parse_git_version("git version 2.54.windows.1"), Some((2, 54)));
        // Garbage → None (surfaced as WARN by the check).
        assert_eq!(parse_git_version("not a version"), None);
        assert_eq!(parse_git_version(""), None);

        assert!(!git_version_ok((2, 53)), "2.53 drops push creds");
        assert!(git_version_ok((2, 54)), "2.54 fixed it");
        assert!(git_version_ok((2, 55)));
        assert!(git_version_ok((3, 0)));
        assert!(!git_version_ok((1, 99)), "ancient git fails");
    }

    #[test]
    fn garbage_git_version_is_warn_not_fail() {
        // A parse miss must WARN (advisory), never FAIL — we cannot prove the push path is broken.
        let check = match parse_git_version("wat") {
            None => Check::warn(GIT_VERSION_CHECK, "unparsed", "hint"),
            Some(v) if git_version_ok(v) => Check::pass(GIT_VERSION_CHECK, "ok"),
            Some((a, b)) => Check::fail(GIT_VERSION_CHECK, format!("{a}.{b}"), "upgrade"),
        };
        assert_eq!(check.status, Status::Warn);
    }

    #[test]
    fn registry_runs_every_check_even_after_an_early_fail() {
        use std::cell::Cell;
        use std::rc::Rc;

        let ran = Rc::new(Cell::new(0usize));
        let checks: Vec<Box<dyn FnOnce() -> Check>> = vec![
            {
                let ran = Rc::clone(&ran);
                Box::new(move || {
                    ran.set(ran.get() + 1);
                    Check::fail("first", "boom", "fix")
                })
            },
            {
                let ran = Rc::clone(&ran);
                Box::new(move || {
                    ran.set(ran.get() + 1);
                    Check::warn("second", "meh", "fix")
                })
            },
            {
                let ran = Rc::clone(&ran);
                Box::new(move || {
                    ran.set(ran.get() + 1);
                    Check::pass("third", "ok")
                })
            },
        ];
        let results = run_checks(checks);
        assert_eq!(ran.get(), 3, "an early FAIL must not short-circuit later checks");
        assert_eq!(results.len(), 3);
        assert_eq!(exit_code(&results), FAILURE, "any FAIL ⇒ exit 1");
    }

    #[test]
    fn exit_code_zero_when_only_pass_and_warn() {
        let results = vec![
            Check::pass("a", "ok"),
            Check::warn("b", "meh", "fix"),
            Check::pass("c", "ok"),
        ];
        assert_eq!(exit_code(&results), SUCCESS, "WARN alone must not fail the exit");
    }

    #[test]
    fn render_shows_fix_hint_only_when_not_pass() {
        assert!(!Check::pass("x", "ok").render().contains("fix:"));
        assert!(Check::fail("x", "bad", "do this").render().contains("(fix: do this)"));
        assert!(Check::warn("x", "hmm", "do this").render().contains("(fix: do this)"));
    }
}
