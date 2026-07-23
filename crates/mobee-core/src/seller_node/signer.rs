//! The signer actor — the node's single owner of the seller Nostr identity.
//!
//! The seller key is read from `$MOBEE_HOME/key` once at startup and lives only inside this task.
//! Every published event is signed here, through the queue, so there is exactly one signing
//! principal per home and the secret never leaves the actor — no agent process, no client, ever
//! sees it. The outbox publisher hands drafts to [`SignerHandle::sign`]; the fixed authored-at
//! second it passes makes the resulting event id deterministic, so a re-publish after a crash is
//! idempotent at the relay.

use nostr_sdk::{EventBuilder, JsonUtil, Keys, Kind, Timestamp};
use tokio::sync::{mpsc, oneshot};

use crate::home::{self, HomeError, MobeeHome};

/// A signed event, ready to publish.
#[derive(Debug, Clone)]
pub struct SignedEvent {
    pub id: String,
    pub json: String,
}

enum Command {
    PublicKey {
        reply: oneshot::Sender<String>,
    },
    /// Sign an event of `kind`/`content` authored at the fixed `created_at` second (deterministic
    /// id for crash-idempotent re-publish).
    Sign {
        kind: u16,
        content: String,
        created_at: i64,
        reply: oneshot::Sender<Result<SignedEvent, String>>,
    },
}

/// A cheap, cloneable handle to the signer actor.
#[derive(Clone)]
pub struct SignerHandle {
    tx: mpsc::Sender<Command>,
    /// Cached once at spawn so the common read need not round-trip.
    public_key_hex: String,
}

/// The signer task exited.
#[derive(Debug)]
pub struct SignerActorGone;

impl std::fmt::Display for SignerActorGone {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "signer actor is not running")
    }
}

impl std::error::Error for SignerActorGone {}

impl SignerHandle {
    /// The seller public key (hex), served from the cache set at spawn.
    pub fn public_key_hex(&self) -> &str {
        &self.public_key_hex
    }

    /// Sign an event through the serialized signer. `created_at` is the fixed authored-at second so
    /// the event id is deterministic across retries.
    pub async fn sign(
        &self,
        kind: u16,
        content: String,
        created_at: i64,
    ) -> Result<Result<SignedEvent, String>, SignerActorGone> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Sign {
                kind,
                content,
                created_at,
                reply,
            })
            .await
            .map_err(|_| SignerActorGone)?;
        rx.await.map_err(|_| SignerActorGone)
    }

    /// The seller public key (hex), routed through the actor queue (proves the serialized path).
    pub async fn public_key_via_actor(&self) -> Result<String, SignerActorGone> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::PublicKey { reply })
            .await
            .map_err(|_| SignerActorGone)?;
        rx.await.map_err(|_| SignerActorGone)
    }
}

/// Load the seller key from `home` and spawn the signer actor. The secret is consumed into the task
/// and never held elsewhere.
pub fn spawn(home: &MobeeHome) -> Result<SignerHandle, HomeError> {
    let secret = home::read_secret_key_hex(home)?;
    let keys =
        Keys::parse(&secret).map_err(|error| HomeError::Key(format!("signer key parse: {error}")))?;
    let public_key_hex = keys.public_key().to_hex();

    let (tx, mut rx) = mpsc::channel::<Command>(64);
    tokio::spawn(async move {
        // `keys` (holding the secret) lives only inside this task.
        while let Some(command) = rx.recv().await {
            match command {
                Command::PublicKey { reply } => {
                    let _ = reply.send(keys.public_key().to_hex());
                }
                Command::Sign {
                    kind,
                    content,
                    created_at,
                    reply,
                } => {
                    let result = sign_event(&keys, kind, &content, created_at);
                    let _ = reply.send(result);
                }
            }
        }
    });

    Ok(SignerHandle {
        tx,
        public_key_hex,
    })
}

fn sign_event(
    keys: &Keys,
    kind: u16,
    content: &str,
    created_at: i64,
) -> Result<SignedEvent, String> {
    let event = EventBuilder::new(Kind::from(kind), content)
        .custom_created_at(Timestamp::from(created_at.max(0) as u64))
        .sign_with_keys(keys)
        .map_err(|error| error.to_string())?;
    let json = event.try_as_json().map_err(|error| error.to_string())?;
    Ok(SignedEvent {
        id: event.id.to_hex(),
        json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::bootstrap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-seller-signer-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn actor_serves_pubkey_and_never_the_secret() {
        let root = temp_home("pubkey");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let secret = home::read_secret_key_hex(&home).expect("secret");

        let signer = spawn(&home).expect("spawn signer");
        let cached = signer.public_key_hex().to_owned();
        let via_actor = signer.public_key_via_actor().await.expect("pubkey");
        assert_eq!(cached, via_actor);
        assert_eq!(cached.len(), 64);
        assert_ne!(cached, secret, "public key must never equal the secret");
        let _ = std::fs::remove_dir_all(&root);
    }

    // The fixed created_at makes the signed event id deterministic — the property the outbox relies
    // on for crash-idempotent re-publish.
    #[tokio::test(flavor = "current_thread")]
    async fn signing_is_deterministic_for_a_fixed_created_at() {
        let root = temp_home("determinism");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let secret = home::read_secret_key_hex(&home).expect("secret");
        let signer = spawn(&home).expect("spawn");

        let first = signer.sign(3402, "creq".to_owned(), 1000).await.expect("a").expect("sign");
        let again = signer.sign(3402, "creq".to_owned(), 1000).await.expect("b").expect("sign");
        assert_eq!(first.id, again.id, "same content + created_at ⇒ same event id");
        assert!(!first.json.contains(&secret), "signed json must not leak the secret");
        let _ = std::fs::remove_dir_all(&root);
    }
}
