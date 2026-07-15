//! Packaged buyer home under `~/.mobee` (or `MOBEE_HOME`).
//!
//! First-run bootstrap writes working defaults: testnut mint, mobee-relay, budget caps,
//! autogen key (`0600`), and an empty `wallet/` dir. The secret key is never returned.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Open-market demo relay (PROCESS.md).
pub const DEFAULT_RELAY_URL: &str = "wss://mobee-relay.orveth.dev";
/// Standing CDK testnut mint — no real funds. Host re-locked 2026-07-15 after
/// `testnut.cashu.space` died from turtle; class (TESTNUT) is the load-bearing rule.
pub const DEFAULT_MINT_URL: &str = "https://testnut.cashudevkit.org";
/// Conservative per-job spend cap (sats) until config is tuned.
pub const DEFAULT_PER_JOB_BUDGET_SATS: u64 = 21;
/// Conservative rolling/session total spend cap (sats).
pub const DEFAULT_TOTAL_BUDGET_SATS: u64 = 100;

const CONFIG_FILE: &str = "config.toml";
const KEY_FILE: &str = "key";
const WALLET_DIR: &str = "wallet";

/// Failure while resolving or bootstrapping the packaged home.
#[derive(Debug)]
pub enum HomeError {
    Io(String),
    Config(String),
    Key(String),
}

impl std::fmt::Display for HomeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(formatter, "home io error: {message}"),
            Self::Config(message) => write!(formatter, "home config error: {message}"),
            Self::Key(message) => write!(formatter, "home key error: {message}"),
        }
    }
}

impl std::error::Error for HomeError {}

/// Buyer-facing packaged config (`~/.mobee/config.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobeeConfig {
    pub relay_url: String,
    pub mint_url: String,
    pub per_job_budget_sats: u64,
    pub total_budget_sats: u64,
}

impl Default for MobeeConfig {
    fn default() -> Self {
        Self {
            relay_url: DEFAULT_RELAY_URL.to_owned(),
            mint_url: DEFAULT_MINT_URL.to_owned(),
            per_job_budget_sats: DEFAULT_PER_JOB_BUDGET_SATS,
            total_budget_sats: DEFAULT_TOTAL_BUDGET_SATS,
        }
    }
}

/// Resolved packaged home after bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobeeHome {
    pub root: PathBuf,
    pub config: MobeeConfig,
    pub key_path: PathBuf,
    pub wallet_dir: PathBuf,
    /// True when this bootstrap call created the key file.
    pub key_created: bool,
}

/// Default home root: `MOBEE_HOME` if set, else `~/.mobee`.
pub fn default_home_dir() -> Result<PathBuf, HomeError> {
    if let Ok(override_dir) = std::env::var("MOBEE_HOME") {
        let path = PathBuf::from(override_dir);
        if path.as_os_str().is_empty() {
            return Err(HomeError::Io("MOBEE_HOME is empty".into()));
        }
        return Ok(path);
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| HomeError::Io("HOME is unset and MOBEE_HOME was not provided".into()))?;
    Ok(PathBuf::from(home).join(".mobee"))
}

/// Ensure `root` exists with config, key (`0600`), and `wallet/` dir.
///
/// Idempotent: existing config/key are left in place. Never returns the secret key.
pub fn bootstrap(root: impl AsRef<Path>) -> Result<MobeeHome, HomeError> {
    let root = root.as_ref().to_path_buf();
    fs::create_dir_all(&root).map_err(|error| HomeError::Io(error.to_string()))?;

    let config_path = root.join(CONFIG_FILE);
    let key_path = root.join(KEY_FILE);
    let wallet_dir = root.join(WALLET_DIR);

    let config = if config_path.exists() {
        load_config(&config_path)?
    } else {
        let config = MobeeConfig::default();
        write_config(&config_path, &config)?;
        config
    };

    fs::create_dir_all(&wallet_dir).map_err(|error| HomeError::Io(error.to_string()))?;

    let key_created = if key_path.exists() {
        validate_existing_key(&key_path)?;
        false
    } else {
        write_new_key(&key_path)?;
        true
    };

    Ok(MobeeHome {
        root,
        config,
        key_path,
        wallet_dir,
        key_created,
    })
}

/// Hex-encode the secp256k1 x-only/public view is deferred; this returns the *public* key
/// only when a caller supplies a derived pubkey. For bootstrap status we expose whether a
/// key file exists — use [`read_secret_key_hex`] only inside trusted surfaces that never log it.
pub fn key_file_present(home: &MobeeHome) -> bool {
    home.key_path.is_file()
}

/// Read the secret key hex from disk. Callers must not log, print, or put this in MCP tool output.
pub fn read_secret_key_hex(home: &MobeeHome) -> Result<String, HomeError> {
    let mut file =
        File::open(&home.key_path).map_err(|error| HomeError::Key(error.to_string()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|error| HomeError::Key(error.to_string()))?;
    let secret = contents.trim().to_owned();
    validate_secret_hex(&secret)?;
    Ok(secret)
}

fn load_config(path: &Path) -> Result<MobeeConfig, HomeError> {
    let raw = fs::read_to_string(path).map_err(|error| HomeError::Config(error.to_string()))?;
    toml::from_str(&raw).map_err(|error| HomeError::Config(error.to_string()))
}

fn write_config(path: &Path, config: &MobeeConfig) -> Result<(), HomeError> {
    let raw = toml::to_string_pretty(config)
        .map_err(|error| HomeError::Config(error.to_string()))?;
    fs::write(path, raw).map_err(|error| HomeError::Io(error.to_string()))
}

fn validate_existing_key(path: &Path) -> Result<(), HomeError> {
    ensure_key_permissions(path)?;
    let mut file = File::open(path).map_err(|error| HomeError::Key(error.to_string()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|error| HomeError::Key(error.to_string()))?;
    validate_secret_hex(contents.trim())
}

/// Existing keys must be `0600`. Too-open modes are re-chmod'd; if that fails, refuse.
fn ensure_key_permissions(path: &Path) -> Result<(), HomeError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata =
            fs::metadata(path).map_err(|error| HomeError::Key(error.to_string()))?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            return Ok(());
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)
            .map_err(|error| HomeError::Key(format!(
                "key file permissions too open ({mode:#o}); re-chmod 0600 failed: {error}"
            )))?;
        let after = fs::metadata(path)
            .map_err(|error| HomeError::Key(error.to_string()))?
            .permissions()
            .mode()
            & 0o777;
        if after & 0o077 != 0 {
            return Err(HomeError::Key(format!(
                "key file permissions too open ({mode:#o}); refused to leave open (still {after:#o})"
            )));
        }
    }
    Ok(())
}

fn validate_secret_hex(secret: &str) -> Result<(), HomeError> {
    if secret.len() != 64 {
        return Err(HomeError::Key(format!(
            "secret key must be 64 hex chars, got {}",
            secret.len()
        )));
    }
    if !secret.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(HomeError::Key("secret key must be hex".into()));
    }
    if secret.chars().all(|ch| ch == '0') {
        return Err(HomeError::Key("secret key must be non-zero".into()));
    }
    Ok(())
}

fn write_new_key(path: &Path) -> Result<(), HomeError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| HomeError::Key(error.to_string()))?;
    if bytes.iter().all(|&byte| byte == 0) {
        return Err(HomeError::Key("generated an all-zero key".into()));
    }
    let secret = hex::encode(bytes);

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| HomeError::Key(error.to_string()))?;
    file.write_all(secret.as_bytes())
        .map_err(|error| HomeError::Key(error.to_string()))?;
    file.write_all(b"\n")
        .map_err(|error| HomeError::Key(error.to_string()))?;
    file.sync_all()
        .map_err(|error| HomeError::Key(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-home-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn bootstrap_writes_defaults_key_and_wallet_dir() {
        let root = temp_home("fresh");
        let _ = fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        assert!(home.key_created);
        assert_eq!(home.config, MobeeConfig::default());
        assert!(home.root.join(CONFIG_FILE).is_file());
        assert!(home.key_path.is_file());
        assert!(home.wallet_dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&home.key_path)
                .expect("key metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let secret = read_secret_key_hex(&home).expect("read key");
        assert_eq!(secret.len(), 64);
    }

    #[test]
    fn bootstrap_is_idempotent_and_preserves_key() {
        let root = temp_home("idempotent");
        let _ = fs::remove_dir_all(&root);
        let first = bootstrap(&root).expect("first");
        let secret = read_secret_key_hex(&first).expect("secret");
        let second = bootstrap(&root).expect("second");
        assert!(!second.key_created);
        assert_eq!(read_secret_key_hex(&second).expect("secret again"), secret);
        assert_eq!(second.config, first.config);
    }

    #[test]
    fn default_home_dir_honors_mobee_home() {
        let root = temp_home("env");
        // Safety: test process isolation — restore after.
        let previous = std::env::var_os("MOBEE_HOME");
        unsafe { std::env::set_var("MOBEE_HOME", &root) };
        let resolved = default_home_dir().expect("resolve");
        match previous {
            Some(value) => unsafe { std::env::set_var("MOBEE_HOME", value) },
            None => unsafe { std::env::remove_var("MOBEE_HOME") },
        }
        assert_eq!(resolved, root);
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_rechmods_too_open_existing_key() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_home("open-key");
        let _ = fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let secret = read_secret_key_hex(&home).expect("secret");

        let mut permissions = fs::metadata(&home.key_path)
            .expect("meta")
            .permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&home.key_path, permissions).expect("chmod 644");

        let again = bootstrap(&root).expect("re-bootstrap must re-chmod or refuse");
        let mode = fs::metadata(&again.key_path)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(read_secret_key_hex(&again).expect("secret again"), secret);
    }
}
