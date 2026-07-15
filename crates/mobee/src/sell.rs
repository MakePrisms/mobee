//! `mobee sell` — seller daemon dual-mode setup + run.
//!
//! - TTY (default): wizard prompts for missing `[seller]` fields, writes config, runs daemon.
//! - `--non-interactive`: fail-closed naming each missing required field; then run.
//! - Never accepts `--key` (key stays in `~/.mobee/nostr.key` via bootstrap; never argv).

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use mobee_core::home::{self, MobeeHome, SellerConfig};

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

#[derive(Debug, Default)]
struct SellOptions {
    non_interactive: bool,
    /// Repeated `--agent-argv <part>` builds the argv array (no shell string).
    agent_argv: Vec<String>,
    rate_sats: Option<u64>,
    git_remote: Option<String>,
    job_timeout_secs: Option<u64>,
    home: Option<PathBuf>,
}

/// Entry from `cli::run` for `mobee sell ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let options = match SellOptions::parse(args) {
        Ok(options) => options,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            sell_usage(err);
            return USAGE_ERROR;
        }
    };

    #[cfg(not(feature = "wallet"))]
    {
        let _ = (options, out);
        let _ = writeln!(
            err,
            "mobee sell requires the wallet feature (rebuild with default features)"
        );
        return USAGE_ERROR;
    }

    #[cfg(feature = "wallet")]
    {
        match run_sell(options, out, err) {
            Ok(()) => SUCCESS,
            Err(code) => code,
        }
    }
}

#[cfg(feature = "wallet")]
fn run_sell(options: SellOptions, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), i32> {
    let root = match options.home.clone() {
        Some(path) => path,
        None => home::default_home_dir().map_err(|error| {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        })?,
    };
    let mut home = home::bootstrap(&root).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;

    // Status must never echo the secret key.
    let _ = writeln!(
        err,
        "mobee sell home={} key_present={} mint={} relay={}",
        home.root.display(),
        home::key_file_present(&home),
        home.config.mint_url,
        home.config.relay_url
    );

    ensure_seller_config(&mut home, &options, out, err)?;

    let daemon = mobee_core::seller_daemon::SellerDaemon::open(home).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;
    let _ = writeln!(
        err,
        "seller starting pubkey={} (never-echo: key omitted)",
        daemon.seller_pubkey()
    );
    mobee_core::seller_daemon::run_forever_blocking(daemon).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;
    Ok(())
}

#[cfg(feature = "wallet")]
fn ensure_seller_config(
    home: &mut MobeeHome,
    options: &SellOptions,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), i32> {
    let existing = home.config.seller.clone();

    let mut agent_command = if options.agent_argv.is_empty() {
        existing
            .as_ref()
            .map(|seller| seller.agent_command.clone())
            .unwrap_or_default()
    } else {
        options.agent_argv.clone()
    };
    let mut rate_sats = options
        .rate_sats
        .or_else(|| existing.as_ref().map(|seller| seller.rate_sats));
    let mut git_remote = options.git_remote.clone().or_else(|| {
        existing
            .as_ref()
            .map(|seller| seller.git_remote.clone())
            .filter(|value| !value.trim().is_empty())
    });
    let mut job_timeout_secs = options
        .job_timeout_secs
        .or_else(|| existing.as_ref().and_then(|seller| seller.job_timeout_secs));

    if options.non_interactive {
        let mut missing = Vec::new();
        if agent_command.is_empty() {
            missing.push("agent_command");
        }
        if rate_sats.is_none() {
            missing.push("rate_sats");
        }
        if git_remote.as_ref().map(|v| v.trim().is_empty()).unwrap_or(true) {
            missing.push("git_remote");
        }
        if !missing.is_empty() {
            let _ = writeln!(
                err,
                "mobee sell --non-interactive missing required field(s): {}",
                missing.join(", ")
            );
            return Err(USAGE_ERROR);
        }
    } else {
        if agent_command.is_empty() {
            agent_command = prompt_agent_argv(out, err)?;
        }
        if rate_sats.is_none() {
            rate_sats = Some(prompt_u64(
                out,
                err,
                "Seller rate_sats (claim floor, sats)",
                21,
            )?);
        }
        if git_remote.as_ref().map(|v| v.trim().is_empty()).unwrap_or(true) {
            git_remote = Some(prompt_line(
                out,
                err,
                "Seller git_remote (https:// or relay-git only)",
                "",
            )?);
            if git_remote.as_ref().map(|v| v.trim().is_empty()).unwrap_or(true) {
                let _ = writeln!(err, "missing required field git_remote");
                return Err(USAGE_ERROR);
            }
        }
        if job_timeout_secs.is_none() {
            let raw = prompt_line(
                out,
                err,
                "Optional job_timeout_secs (empty = offer deadline / default 600)",
                "",
            )?;
            if !raw.trim().is_empty() {
                job_timeout_secs = Some(raw.trim().parse().map_err(|_| {
                    let _ = writeln!(err, "job_timeout_secs must be a u64");
                    USAGE_ERROR
                })?);
            }
        }
    }

    let seller = SellerConfig {
        agent_command,
        rate_sats: rate_sats.ok_or_else(|| {
            let _ = writeln!(err, "missing required field rate_sats");
            USAGE_ERROR
        })?,
        git_remote: git_remote.ok_or_else(|| {
            let _ = writeln!(err, "missing required field git_remote");
            USAGE_ERROR
        })?,
        job_timeout_secs,
    };
    home.config.seller = Some(seller);
    home::save_config(home).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;
    let _ = writeln!(err, "wrote [seller] to {}", home.root.join("config.toml").display());
    Ok(())
}

fn prompt_agent_argv(out: &mut dyn Write, err: &mut dyn Write) -> Result<Vec<String>, i32> {
    let _ = writeln!(
        out,
        "Enter agent_command as argv parts (one per line; empty line ends)."
    );
    let _ = writeln!(
        out,
        "Example first line: cursor-agent   then: --print   (NOT a shell string)"
    );
    let _ = out.flush();
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut argv = Vec::new();
    loop {
        let _ = write!(out, "argv[{}]> ", argv.len());
        let _ = out.flush();
        let line = match lines.next() {
            Some(Ok(line)) => line,
            Some(Err(error)) => {
                let _ = writeln!(err, "{error}");
                return Err(RUNTIME_ERROR);
            }
            None => break,
        };
        if line.trim().is_empty() {
            break;
        }
        argv.push(line);
    }
    if argv.is_empty() {
        let _ = writeln!(err, "missing required field agent_command");
        return Err(USAGE_ERROR);
    }
    Ok(argv)
}

fn prompt_line(
    out: &mut dyn Write,
    err: &mut dyn Write,
    label: &str,
    default: &str,
) -> Result<String, i32> {
    if default.is_empty() {
        let _ = write!(out, "{label}: ");
    } else {
        let _ = write!(out, "{label} [{default}]: ");
    }
    let _ = out.flush();
    let mut line = String::new();
    io::stdin().read_line(&mut line).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;
    let trimmed = line.trim().to_owned();
    if trimmed.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(trimmed)
    }
}

fn prompt_u64(
    out: &mut dyn Write,
    err: &mut dyn Write,
    label: &str,
    default: u64,
) -> Result<u64, i32> {
    let raw = prompt_line(out, err, label, &default.to_string())?;
    raw.parse().map_err(|_| {
        let _ = writeln!(err, "{label} must be a u64");
        USAGE_ERROR
    })
}

impl SellOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self::default();
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--non-interactive" => options.non_interactive = true,
                "--key" | "--secret-key" | "--private-key" => {
                    return Err(
                        "refused: --key / secret key argv is not allowed (key stays in home file)"
                            .into(),
                    );
                }
                "--agent-argv" => {
                    index += 1;
                    let part = args
                        .get(index)
                        .ok_or_else(|| "missing value for --agent-argv".to_owned())?;
                    if part.is_empty() {
                        return Err("--agent-argv entries must be non-empty".into());
                    }
                    options.agent_argv.push(part.clone());
                }
                "--rate-sats" => {
                    index += 1;
                    let raw = args
                        .get(index)
                        .ok_or_else(|| "missing value for --rate-sats".to_owned())?;
                    options.rate_sats = Some(
                        raw.parse()
                            .map_err(|_| format!("--rate-sats must be a u64, got {raw}"))?,
                    );
                }
                "--git-remote" => {
                    index += 1;
                    options.git_remote = Some(
                        args.get(index)
                            .ok_or_else(|| "missing value for --git-remote".to_owned())?
                            .clone(),
                    );
                }
                "--job-timeout-secs" => {
                    index += 1;
                    let raw = args
                        .get(index)
                        .ok_or_else(|| "missing value for --job-timeout-secs".to_owned())?;
                    options.job_timeout_secs = Some(
                        raw.parse()
                            .map_err(|_| format!("--job-timeout-secs must be a u64, got {raw}"))?,
                    );
                }
                "--home" => {
                    index += 1;
                    options.home = Some(PathBuf::from(
                        args.get(index)
                            .ok_or_else(|| "missing value for --home".to_owned())?,
                    ));
                }
                other => return Err(format!("unknown sell option: {other}")),
            }
            index += 1;
        }
        Ok(options)
    }
}

fn sell_usage(err: &mut dyn Write) {
    let _ = writeln!(
        err,
        "Usage:\n  mobee sell\n  mobee sell --non-interactive --agent-argv <prog> [--agent-argv <arg> ...] --rate-sats <n> --git-remote <url> [--job-timeout-secs <n>] [--home <dir>]\n\nNotes:\n  - agent_command is an argv array (repeat --agent-argv); shell strings refused\n  - no --key (packaged key file only)\n  - TTY wizard writes [seller]; --non-interactive names missing required fields"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_key_argv() {
        let err = SellOptions::parse(&["--key".into(), "deadbeef".into()]).unwrap_err();
        assert!(err.contains("not allowed"));
    }

    #[test]
    fn parses_agent_argv_array() {
        let options = SellOptions::parse(&[
            "--non-interactive".into(),
            "--agent-argv".into(),
            "cursor-agent".into(),
            "--agent-argv".into(),
            "--print".into(),
            "--rate-sats".into(),
            "21".into(),
            "--git-remote".into(),
            "https://example.invalid/repo.git".into(),
        ])
        .expect("parse");
        assert!(options.non_interactive);
        assert_eq!(
            options.agent_argv,
            vec!["cursor-agent".to_owned(), "--print".to_owned()]
        );
        assert_eq!(options.rate_sats, Some(21));
    }

    #[test]
    fn non_interactive_names_missing_fields() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            &[
                "--non-interactive".into(),
                "--rate-sats".into(),
                "1".into(),
                // missing agent_command + git_remote
                "--home".into(),
                std::env::temp_dir()
                    .join(format!("mobee-sell-miss-{}", std::process::id()))
                    .to_string_lossy()
                    .into_owned(),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, USAGE_ERROR);
        let rendered = String::from_utf8_lossy(&err);
        assert!(
            rendered.contains("agent_command") && rendered.contains("git_remote"),
            "stderr={rendered}"
        );
        assert!(!rendered.to_ascii_lowercase().contains("nsec"));
    }
}
