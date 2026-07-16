//! Named agent presets → ACP stdio argv arrays.
//!
//! Sellers pick `--agent claude|cursor|codex`; raw `--agent-argv` remains the power-user hatch.

use std::path::{Path, PathBuf};

/// Resolve a preset name to an argv array suitable for the seller ACP driver.
pub fn resolve_agent_preset(name: &str) -> Result<(String, Vec<String>), String> {
    let key = name.trim().to_ascii_lowercase();
    match key.as_str() {
        "claude" => Ok(("claude".into(), resolve_claude())),
        "cursor" => Ok(("cursor".into(), resolve_cursor())),
        "codex" => Ok(("codex".into(), resolve_codex())),
        other => Err(format!(
            "unknown --agent {other:?} (want claude|cursor|codex, or use --agent-argv)"
        )),
    }
}

/// Which presets have a resolvable binary on PATH / known locations.
pub fn detect_available_agents() -> Vec<&'static str> {
    let mut out = Vec::new();
    if which("claude-agent-acp").is_some()
        || which("claude").is_some()
        || npx_available()
    {
        out.push("claude");
    }
    if which("cursor-agent").is_some() || which("agent").is_some() {
        out.push("cursor");
    }
    if which("codex-acp").is_some()
        || Path::new("/srv/forge/tools/codex-acp-ng/node_modules/.bin/codex-acp").is_file()
        || npx_available()
    {
        out.push("codex");
    }
    out
}

fn resolve_claude() -> Vec<String> {
    if let Some(bin) = which("claude-agent-acp") {
        return vec![bin.to_string_lossy().into_owned()];
    }
    // npx resolves the published ACP adapter (no raw ACP knowledge required of the seller).
    if let Some(npx) = which("npx") {
        return vec![
            npx.to_string_lossy().into_owned(),
            "-y".into(),
            "@agentclientprotocol/claude-agent-acp".into(),
        ];
    }
    // Last resort: still emit the canonical package argv (install-time failure is clearer).
    vec![
        "npx".into(),
        "-y".into(),
        "@agentclientprotocol/claude-agent-acp".into(),
    ]
}

fn resolve_cursor() -> Vec<String> {
    if let Some(bin) = which("cursor-agent").or_else(|| which("agent")) {
        return vec![bin.to_string_lossy().into_owned(), "acp".into()];
    }
    vec!["cursor-agent".into(), "acp".into()]
}

fn resolve_codex() -> Vec<String> {
    if let Some(bin) = which("codex-acp") {
        return vec![bin.to_string_lossy().into_owned()];
    }
    let dogfood = PathBuf::from("/srv/forge/tools/codex-acp-ng/node_modules/.bin/codex-acp");
    if dogfood.is_file() {
        return vec![dogfood.to_string_lossy().into_owned()];
    }
    if let Some(npx) = which("npx") {
        return vec![
            npx.to_string_lossy().into_owned(),
            "-y".into(),
            "@agentclientprotocol/codex-acp".into(),
        ];
    }
    vec![
        "npx".into(),
        "-y".into(),
        "@agentclientprotocol/codex-acp".into(),
    ]
}

fn npx_available() -> bool {
    which("npx").is_some()
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_resolve_nonempty_argv() {
        for name in ["claude", "cursor", "codex"] {
            let (label, argv) = resolve_agent_preset(name).expect("preset");
            assert_eq!(label, name);
            assert!(!argv.is_empty());
            assert!(argv.iter().all(|p| !p.is_empty()));
        }
    }

    #[test]
    fn unknown_preset_errors() {
        assert!(resolve_agent_preset("goose").is_err());
    }
}
