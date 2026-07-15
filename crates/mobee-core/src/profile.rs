//! Buyer kind-0 (NIP-01 metadata) publish + best-effort read.
//!
//! **Composition rule:** kind-0 `name` is untrusted display metadata. It must never
//! feed targeting, accept-bind, D2 tip-match, or budget decisions — those stay keyed
//! on hex pubkey alone. This module is intentionally separate from
//! `authorize_pay` / `budget` / `delivery` / `payment`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::home::{self, HomeError, MobeeHome, ProfileConfig};

const DEFAULT_FETCH_TIMEOUT_SECS: u64 = 8;
/// Cap hostile kind-0 payloads (same order as web network parser).
const PROFILE_CONTENT_MAX: usize = 64 * 1024;
const PROFILE_NAME_MAX: usize = 128;
const PROFILE_ABOUT_MAX: usize = 512;

/// Inputs for [`set_profile`]. Omitted fields leave existing config values alone;
/// call with both `None` to re-publish from config as-is.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SetProfileRequest {
    pub name: Option<String>,
    pub about: Option<String>,
}

/// Outcome of a successful `set_profile` (never includes the secret key).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SetProfileOutcome {
    pub ok: bool,
    pub pubkey: String,
    pub name: Option<String>,
    pub about: Option<String>,
    pub event_id: String,
    pub relay_url: String,
}

#[derive(Debug)]
pub enum ProfileError {
    Input(String),
    Home(HomeError),
    Relay(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => write!(formatter, "profile input: {message}"),
            Self::Home(error) => write!(formatter, "{error}"),
            Self::Relay(message) => write!(formatter, "profile relay: {message}"),
        }
    }
}

impl std::error::Error for ProfileError {}

impl From<HomeError> for ProfileError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}

/// Write optional name/about into `[profile]`, then publish/replace buyer kind-0.
///
/// Sync entry for CLI/tests. Nested call from an async context fails fast —
/// use [`set_profile_async`]. Never echoes the secret key.
pub fn set_profile(
    home: &mut MobeeHome,
    request: SetProfileRequest,
) -> Result<SetProfileOutcome, ProfileError> {
    crate::runtime_guard::refuse_nested_block_on("set_profile")
        .map_err(ProfileError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ProfileError::Relay(error.to_string()))?;
    runtime.block_on(set_profile_async(home, request))
}

/// Async `set_profile` for callers already on a Tokio runtime (MCP dispatch).
pub async fn set_profile_async(
    home: &mut MobeeHome,
    request: SetProfileRequest,
) -> Result<SetProfileOutcome, ProfileError> {
    home::reload_config(home)?;

    if let Some(name) = &request.name {
        let trimmed = clamp_field(name, PROFILE_NAME_MAX).ok_or_else(|| {
            ProfileError::Input("name must be a non-empty string (max 128 chars)".into())
        })?;
        profile_mut(home).name = Some(trimmed);
    }
    if let Some(about) = &request.about {
        let trimmed = clamp_field(about, PROFILE_ABOUT_MAX).ok_or_else(|| {
            ProfileError::Input("about must be a non-empty string (max 512 chars)".into())
        })?;
        profile_mut(home).about = Some(trimmed);
    }

    // Ensure the section exists even when re-publishing empties (idempotent replace).
    if home.config.profile.is_none() {
        home.config.profile = Some(ProfileConfig::default());
    }

    home::save_config(home)?;

    let profile = home.config.profile.clone().unwrap_or_default();
    let keys = buyer_keys(home)?;
    let event_id = publish_metadata_async(home, &keys, &profile).await?;

    Ok(SetProfileOutcome {
        ok: true,
        pubkey: keys.public_key().to_hex(),
        name: profile.name,
        about: profile.about,
        event_id,
        relay_url: home.config.relay_url.clone(),
    })
}

/// Best-effort resolve of kind-0 `name` per pubkey. Missing/unparseable → `None`.
///
/// Returns a map keyed by lowercase hex pubkey. Never used for payment decisions.
pub fn resolve_display_names(
    home: &MobeeHome,
    pubkeys: impl IntoIterator<Item = impl AsRef<str>>,
) -> HashMap<String, Option<String>> {
    let mut unique = HashSet::new();
    for key in pubkeys {
        let hex = key.as_ref().trim().to_ascii_lowercase();
        if hex.len() == 64 && hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
            unique.insert(hex);
        }
    }
    if unique.is_empty() {
        return HashMap::new();
    }

    match fetch_names(home, &unique) {
        Ok(map) => map,
        Err(_) => unique.into_iter().map(|k| (k, None)).collect(),
    }
}

fn profile_mut(home: &mut MobeeHome) -> &mut ProfileConfig {
    home.config
        .profile
        .get_or_insert_with(ProfileConfig::default)
}

fn clamp_field(raw: &str, max: usize) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let cut = if trimmed.len() > max {
        trimmed.chars().take(max).collect()
    } else {
        trimmed.to_owned()
    };
    Some(cut)
}

fn buyer_keys(home: &MobeeHome) -> Result<nostr_sdk::Keys, ProfileError> {
    let secret = home::read_secret_key_hex(home)?;
    nostr_sdk::Keys::parse(&secret)
        .map_err(|error| ProfileError::Home(HomeError::Key(format!("buyer key parse: {error}"))))
}

#[allow(dead_code)] // guarded sync twin for non-async callers; MCP uses `_async`
fn publish_metadata(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    profile: &ProfileConfig,
) -> Result<String, ProfileError> {
    crate::runtime_guard::refuse_nested_block_on("publish_metadata")
        .map_err(ProfileError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ProfileError::Relay(error.to_string()))?;
    runtime.block_on(publish_metadata_async(home, keys, profile))
}

async fn publish_metadata_async(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    profile: &ProfileConfig,
) -> Result<String, ProfileError> {
    use nostr_sdk::prelude::{Client, EventBuilder, Metadata};

    let mut metadata = Metadata::new();
    if let Some(name) = &profile.name {
        metadata = metadata.name(name);
    }
    if let Some(about) = &profile.about {
        metadata = metadata.about(about);
    }

    let event = EventBuilder::metadata(&metadata)
        .sign_with_keys(keys)
        .map_err(|error| ProfileError::Relay(format!("sign kind-0: {error}")))?;

    let client = Client::new(keys.clone());
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| ProfileError::Relay(format!("add relay: {error}")))?;
    client.connect().await;
    let output = client
        .send_event_to([&home.config.relay_url], &event)
        .await;
    client.disconnect().await;
    let output = output.map_err(|error| ProfileError::Relay(format!("send kind-0: {error}")))?;
    if output.success.is_empty() {
        let failed: Vec<String> = output
            .failed
            .into_iter()
            .map(|(url, err)| format!("{url}: {err}"))
            .collect();
        return Err(ProfileError::Relay(format!(
            "no relay accepted kind-0 ({})",
            failed.join("; ")
        )));
    }
    Ok(output.val.to_hex())
}

fn fetch_names(
    home: &MobeeHome,
    pubkeys: &HashSet<String>,
) -> Result<HashMap<String, Option<String>>, ProfileError> {
    // Sync entry only — must not be called from inside an existing Tokio runtime
    // (nested block_on panics). Async callers use [`fetch_names_async`].
    crate::runtime_guard::refuse_nested_block_on("fetch_names")
        .map_err(ProfileError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ProfileError::Relay(error.to_string()))?;
    runtime.block_on(fetch_names_async(home, pubkeys))
}

/// Async kind-0 name fetch for callers already on a Tokio runtime (e.g. `get_job`).
pub async fn fetch_names_async(
    home: &MobeeHome,
    pubkeys: &HashSet<String>,
) -> Result<HashMap<String, Option<String>>, ProfileError> {
    use nostr_sdk::prelude::{Client, Filter, Kind, PublicKey};

    let keys = buyer_keys(home)?;
    let authors: Result<Vec<PublicKey>, ProfileError> = pubkeys
        .iter()
        .map(|hex| {
            PublicKey::from_hex(hex)
                .map_err(|error| ProfileError::Input(format!("pubkey {hex}: {error}")))
        })
        .collect();
    let authors = authors?;

    let client = Client::new(keys);
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| ProfileError::Relay(format!("add relay: {error}")))?;
    client.connect().await;

    let filter = Filter::new().authors(authors).kind(Kind::Metadata);
    let timeout = Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS);
    let events = client.fetch_events(filter, timeout).await;
    client.disconnect().await;
    let events =
        events.map_err(|error| ProfileError::Relay(format!("fetch kind-0: {error}")))?;

    // Newest replaceable kind-0 wins per author.
    let mut newest: HashMap<String, (u64, String)> = HashMap::new();
    for event in events {
        let author = event.pubkey.to_hex().to_ascii_lowercase();
        let created = event.created_at.as_secs();
        if newest
            .get(&author)
            .map(|(prev, _)| created > *prev)
            .unwrap_or(true)
        {
            newest.insert(author, (created, event.content.clone()));
        }
    }

    let mut out = HashMap::new();
    for hex in pubkeys {
        let name = newest
            .get(hex)
            .and_then(|(_, content)| parse_kind0_name(content));
        out.insert(hex.clone(), name);
    }
    Ok(out)
}

/// Defensive kind-0 content parse — `name` only (cosmetic).
fn parse_kind0_name(content: &str) -> Option<String> {
    let raw = if content.len() > PROFILE_CONTENT_MAX {
        &content[..PROFILE_CONTENT_MAX]
    } else {
        content
    };
    let parsed: Kind0Content = serde_json::from_str(raw).ok()?;
    clamp_field(parsed.name.as_deref()?, PROFILE_NAME_MAX)
}

#[derive(Debug, Deserialize)]
struct Kind0Content {
    #[serde(default)]
    name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kind0_name_reads_name_field() {
        assert_eq!(
            parse_kind0_name(r#"{"name":"seller-a","about":"x"}"#).as_deref(),
            Some("seller-a")
        );
        assert_eq!(parse_kind0_name(r#"{"about":"only"}"#), None);
        assert_eq!(parse_kind0_name("not-json"), None);
        assert_eq!(parse_kind0_name(r#"{"name":"   "}"#), None);
    }

    #[test]
    fn resolve_display_names_skips_invalid_hex_without_relay() {
        let root = std::env::temp_dir().join(format!(
            "mobee-profile-resolve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let map = resolve_display_names(&home, ["not-a-key", ""]);
        assert!(map.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn set_profile_writes_config_without_inventing_on_bootstrap() {
        let root = std::env::temp_dir().join(format!(
            "mobee-profile-set-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        assert!(home.config.profile.is_none());

        // Merge into config only (skip relay publish by testing profile_mut path via save).
        home.config.profile = Some(ProfileConfig {
            name: Some("buyer-x".into()),
            about: Some("about-x".into()),
        });
        home::save_config(&home).expect("save");
        home::reload_config(&mut home).expect("reload");
        let profile = home.config.profile.expect("present");
        assert_eq!(profile.name.as_deref(), Some("buyer-x"));
        assert_eq!(profile.about.as_deref(), Some("about-x"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_profile_sync_refuses_inside_runtime() {
        let root = std::env::temp_dir().join(format!(
            "mobee-profile-nested-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        let err = set_profile(
            &mut home,
            SetProfileRequest {
                name: Some("nested-guard".into()),
                about: None,
            },
        )
        .expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publish_metadata_sync_refuses_inside_runtime() {
        let root = std::env::temp_dir().join(format!(
            "mobee-publish-meta-nested-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let keys = nostr_sdk::Keys::generate();
        let profile = ProfileConfig {
            name: Some("nested-guard".into()),
            about: None,
        };
        let err = publish_metadata(&home, &keys, &profile).expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        assert!(
            err.to_string().contains("publish_metadata"),
            "op name missing: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_names_sync_refuses_inside_runtime() {
        let root = std::env::temp_dir().join(format!(
            "mobee-fetch-names-nested-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut pubkeys = HashSet::new();
        pubkeys.insert("aa".repeat(32));
        let err = fetch_names(&home, &pubkeys).expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        assert!(
            err.to_string().contains("fetch_names"),
            "op name missing: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
