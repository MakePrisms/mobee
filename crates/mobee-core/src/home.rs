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
/// Dead testnut host — bootstrap migrates config.toml away from this.
pub const DEAD_TESTNUT_MINT_HOST: &str = "testnut.cashu.space";
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

/// Optional buyer identity metadata (`[profile]` in config.toml).
///
/// Absent by default — fresh bootstrap does **not** invent a name. Kind-0 names are
/// untrusted display metadata only; decision paths must key on hex pubkey alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
}

/// Default relay-git base (delivery). Live on mobee-relay (`/git/<owner>/<repo>.git`).
pub const DEFAULT_RELAY_GIT_BASE: &str = "https://mobee-relay.orveth.dev/git";
/// Legacy shared leaf — NOT used as default (relay name registry is global).
pub const DEFAULT_RELAY_GIT_REPO: &str = "mobee-seller";

/// Seller daemon config (`[seller]` in config.toml). Key never lives here.
///
/// `agent_command` MUST be an argv array — a TOML string/shell value is refused at parse
/// (no-shell by construction).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SellerConfig {
    #[serde(deserialize_with = "deserialize_agent_command_argv")]
    pub agent_command: Vec<String>,
    pub rate_sats: u64,
    pub git_remote: String,
    /// Job deadline override (seconds). Default: offer `deadline_unix`, else ~600s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_timeout_secs: Option<u64>,
    /// Optional preset label (`claude` | `cursor` | `codex`) for rediscovery / status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Opt-in to claim untargeted/open offers. Default **false** (targeted-only).
    #[serde(default)]
    pub claim_open_pool: bool,
    /// Backfill window (seconds) for the seller's UNTARGETED (open-pool) kind-5109 offer
    /// filter. On (re)subscribe the open-pool filter requests stored offers dated at/after
    /// `now - this`, so a daemon started AFTER an open-pool offer was posted still SEES it
    /// (and claims it iff every money-safety guard passes: not deadline-expired, clears the
    /// rate floor, not already delivered/settled, not live-claimed by another seller).
    /// Default **1200** (20 min). **`0` = live-only** — byte-identical pre-backfill shape
    /// (`since(now)` + `limit(0)`): no stored open-pool offers, only ones posted while the
    /// daemon runs. The TARGETED (`#p==self`) filter is NOT affected by this knob — it keeps
    /// its original full-history backfill at all values (stored targeted offers are addressed
    /// to this seller); the classify-level deadline-expiry refusal is the staleness guard on
    /// both paths.
    #[serde(default = "default_offer_backfill_secs")]
    pub offer_backfill_secs: u64,
    /// Opt-in to the piece-10 contribution (freelance-PR fork) path. Default **true**. When
    /// **false** the daemon behaves as a seller WITHOUT contribution support: it kind-7000
    /// `status=error`s a `job-class=contribution` offer instead of running it as from-scratch
    /// (interop courtesy — NOT a security control; buyer refusal is the boundary).
    #[serde(default = "default_contribution_enabled")]
    pub contribution_enabled: bool,
}

/// Piece-13 persistent-seller-memory config (`[seller_memory]` section). The read-on-start +
/// retro-write-back knobs and the two plugin seams (prompt template paths). Every field has a
/// serde default so a config written before this section existed parses to the shipped defaults
/// (back-compat).
///
/// NOTE (build judgment call): the PIECE-13 spec names this section `[seller.memory]` (nested in
/// `[seller]`). Nesting it inside `SellerConfig` would force adding a required field to that
/// struct, whose literal is constructed in `seller.rs` — a file the piece-13 build is forbidden
/// to touch (money-path boundary; money-files diff must stay empty). Placing it top-level as
/// `[seller_memory]` on `MobeeConfig` (built only via `Default`) delivers the identical knobs and
/// seams without touching any money-path file. The nesting is cosmetic; behaviour is unchanged.
///
/// This is **diagnostic/economic** context only. Nothing here ever feeds the pay gate, the
/// journal, or the receipt bind (see PIECE-13 § Threat & integrity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SellerMemoryConfig {
    /// Inline the distilled `MEMORY.md` index into the agent's job prompt at start. Default
    /// **on**; when **false** the composed prompt is byte-identical to the pre-piece-13 output.
    #[serde(default = "default_memory_enabled")]
    pub memory_enabled: bool,
    /// Run one best-effort retro agent turn after a delivered-**paid** job to update memory.
    /// Default **on**; gated separately from `memory_enabled` (the read path is cheap, the retro
    /// turn costs a model call). Never blocks or affects the money path.
    #[serde(default = "default_retro_enabled")]
    pub retro_enabled: bool,
    /// Plugin seam: template for the retro/distiller prompt. Unset ⇒ the in-repo default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retro_prompt_path: Option<PathBuf>,
    /// Plugin seam: template framing how `MEMORY.md` is inlined at job start. Unset ⇒ in-repo
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_on_start_template_path: Option<PathBuf>,
}

impl Default for SellerMemoryConfig {
    fn default() -> Self {
        Self {
            memory_enabled: default_memory_enabled(),
            retro_enabled: default_retro_enabled(),
            retro_prompt_path: None,
            read_on_start_template_path: None,
        }
    }
}

impl SellerMemoryConfig {
    /// True when every field is at its shipped default (so config.toml stays clean — the section
    /// is only serialized once an operator sets a non-default knob).
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// serde default for [`SellerMemoryConfig::memory_enabled`] — read-on-start ON.
pub fn default_memory_enabled() -> bool {
    true
}

/// serde default for [`SellerMemoryConfig::retro_enabled`] — retro write-back ON.
pub fn default_retro_enabled() -> bool {
    true
}

/// Default for [`SellerConfig::contribution_enabled`] — contribution support ON.
pub fn default_contribution_enabled() -> bool {
    true
}

/// serde default for [`SellerConfig::offer_backfill_secs`]: 1200s (20 min). A `[seller]` block
/// written before this field existed parses to this default; `0` must be set explicitly.
pub fn default_offer_backfill_secs() -> u64 {
    1200
}

/// Per-seller NIP-34 `d` / path leaf. Relay `.names/` registry is GLOBAL across
/// owners — a shared constant like `mobee-seller` collides and seeds fail silently.
pub fn default_relay_git_repo_id(seller_pubkey_hex: &str) -> String {
    let pk = seller_pubkey_hex.trim().to_ascii_lowercase();
    let short = &pk[..16.min(pk.len())];
    format!("m{short}")
}

/// Build the default relay-git remote for a seller pubkey (self-owned namespace).
pub fn default_relay_git_remote(seller_pubkey_hex: &str) -> String {
    let pk = seller_pubkey_hex.trim().to_ascii_lowercase();
    let repo = default_relay_git_repo_id(&pk);
    format!("{DEFAULT_RELAY_GIT_BASE}/{pk}/{repo}.git")
}

/// Repo `d`-tag / path leaf for a relay-git remote (`…/git/<owner>/<repo>[.git]`).
pub fn relay_git_repo_id(remote_url: &str) -> Option<String> {
    let lower = remote_url.trim().to_ascii_lowercase();
    let idx = lower.find("/git/")?;
    let prefix_len = "/git/".len();
    let rest = remote_url.trim().get(idx + prefix_len..)?;
    let mut parts = rest.split('/').filter(|p| !p.is_empty());
    let _owner = parts.next()?;
    let mut repo = parts.next()?.to_owned();
    if let Some(stripped) = repo.strip_suffix(".git") {
        repo = stripped.to_owned();
    }
    if repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(repo)
}

fn deserialize_agent_command_argv<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, SeqAccess, Visitor};
    use std::fmt;

    struct ArgvVisitor;

    impl<'de> Visitor<'de> for ArgvVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("argv array (not a shell string)")
        }

        fn visit_str<E: de::Error>(self, _value: &str) -> Result<Self::Value, E> {
            Err(E::custom(
                "agent_command must be an argv array, not a string/shell value",
            ))
        }

        fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
            self.visit_str(&value)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                out.push(item);
            }
            if out.is_empty() {
                return Err(de::Error::custom("agent_command argv must be non-empty"));
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(ArgvVisitor)
}

/// Buyer-facing packaged config (`~/.mobee/config.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobeeConfig {
    pub relay_url: String,
    pub mint_url: String,
    pub per_job_budget_sats: u64,
    pub total_budget_sats: u64,
    /// Opt-in additional mints (`mobee wallet mints add`). Default mint stays
    /// [`DEFAULT_MINT_URL`] / `mint_url`; never invents spendable credit by itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_mints: Vec<String>,
    /// Optional `[profile] name / about`. Skipped when absent so fresh homes stay unnamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileConfig>,
    /// Optional `[seller]` daemon config. Absent until `mobee sell` setup writes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seller: Option<SellerConfig>,
    /// Piece-13 `[seller_memory]` config (read-on-start + retro seams). Defaults when absent.
    #[serde(default, skip_serializing_if = "SellerMemoryConfig::is_default")]
    pub seller_memory: SellerMemoryConfig,
    /// Optional buyer-side piece-10 contribution content policy (the MUST-5 policy hook). Absent
    /// ⇒ the FLOOR (refuse only empty diffs). Present ⇒ tighten pre-pay with a path allowlist /
    /// forbidden paths / max diff size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution: Option<ContributionPolicyConfig>,
}

/// Buyer-side content policy for piece-10 contribution verify (the MUST-5 policy hook). Maps 1:1
/// to `contribution::ContentPolicy`; kept as a plain config type so `home` need not depend on the
/// git-delivery feature. All fields default to the floor (allow all, forbid none, no cap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContributionPolicyConfig {
    /// Non-empty ⇒ every changed path MUST lie under one of these prefixes (out-of-scope refuse).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_paths: Vec<String>,
    /// A changed path under any of these prefixes is refused (checked before the allowlist).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden_paths: Vec<String>,
    /// Refuse when summed churn exceeds this many units. `None` ⇒ no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_diff_bytes: Option<u64>,
}

impl Default for MobeeConfig {
    fn default() -> Self {
        Self {
            relay_url: DEFAULT_RELAY_URL.to_owned(),
            mint_url: DEFAULT_MINT_URL.to_owned(),
            per_job_budget_sats: DEFAULT_PER_JOB_BUDGET_SATS,
            total_budget_sats: DEFAULT_TOTAL_BUDGET_SATS,
            extra_mints: Vec::new(),
            profile: None,
            seller: None,
            seller_memory: SellerMemoryConfig::default(),
            contribution: None,
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
/// Idempotent: existing config/key are left in place except dead-mint migration
/// (`testnut.cashu.space` → [`DEFAULT_MINT_URL`]). Never returns the secret key.
pub fn bootstrap(root: impl AsRef<Path>) -> Result<MobeeHome, HomeError> {
    let root = root.as_ref().to_path_buf();
    fs::create_dir_all(&root).map_err(|error| HomeError::Io(error.to_string()))?;

    let config_path = root.join(CONFIG_FILE);
    let key_path = root.join(KEY_FILE);
    let wallet_dir = root.join(WALLET_DIR);

    let config = if config_path.exists() {
        let mut config = load_config(&config_path)?;
        if migrate_dead_mint_url(&mut config) {
            write_config(&config_path, &config)?;
        }
        config
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

/// Rewrite dead `.cashu.space` testnut hosts to [`DEFAULT_MINT_URL`]. Returns true when changed.
pub fn migrate_dead_mint_url(config: &mut MobeeConfig) -> bool {
    let lower = config.mint_url.to_ascii_lowercase();
    if lower.contains(DEAD_TESTNUT_MINT_HOST) {
        config.mint_url = DEFAULT_MINT_URL.to_owned();
        true
    } else {
        false
    }
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

/// Hex-encode the buyer's nostr public key derived from the packaged secret.
/// Safe to return on MCP surfaces (not secret material).
#[cfg(feature = "wallet")]
pub fn public_key_hex(home: &MobeeHome) -> Result<String, HomeError> {
    let secret = read_secret_key_hex(home)?;
    let keys = nostr_sdk::Keys::parse(&secret)
        .map_err(|error| HomeError::Key(format!("key parse for pubkey: {error}")))?;
    Ok(keys.public_key().to_hex())
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

/// Persist `home.config` to `config.toml` (used by `set_profile` and mint migration).
pub fn save_config(home: &MobeeHome) -> Result<(), HomeError> {
    write_config(&home.root.join(CONFIG_FILE), &home.config)
}

/// Reload `config.toml` into `home.config` without touching the key file.
pub fn reload_config(home: &mut MobeeHome) -> Result<(), HomeError> {
    home.config = load_config(&home.root.join(CONFIG_FILE))?;
    if migrate_dead_mint_url(&mut home.config) {
        write_config(&home.root.join(CONFIG_FILE), &home.config)?;
    }
    Ok(())
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

    #[test]
    fn bootstrap_migrates_dead_cashu_space_mint() {
        let root = temp_home("dead-mint");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("mkdir");
        let config_path = root.join(CONFIG_FILE);
        let stale = MobeeConfig {
            mint_url: format!("https://{DEAD_TESTNUT_MINT_HOST}"),
            ..MobeeConfig::default()
        };
        write_config(&config_path, &stale).expect("write stale");
        let home = bootstrap(&root).expect("bootstrap migrates");
        assert_eq!(home.config.mint_url, DEFAULT_MINT_URL);
        let reloaded = load_config(&config_path).expect("reload");
        assert_eq!(reloaded.mint_url, DEFAULT_MINT_URL);
    }

    #[test]
    fn bootstrap_does_not_invent_profile() {
        let root = temp_home("no-profile");
        let _ = fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        assert!(home.config.profile.is_none());
        let raw = fs::read_to_string(home.root.join(CONFIG_FILE)).expect("read");
        assert!(
            !raw.contains("[profile]"),
            "fresh bootstrap must not invent [profile]: {raw}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn save_and_reload_profile_round_trip() {
        let root = temp_home("profile-rt");
        let _ = fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        home.config.profile = Some(ProfileConfig {
            name: Some("anvil-buyer".into()),
            about: Some("testnut only".into()),
        });
        save_config(&home).expect("save");
        home.config.profile = None;
        reload_config(&mut home).expect("reload");
        let profile = home.config.profile.expect("profile present");
        assert_eq!(profile.name.as_deref(), Some("anvil-buyer"));
        assert_eq!(profile.about.as_deref(), Some("testnut only"));
        let _ = fs::remove_dir_all(&root);
    }
}
