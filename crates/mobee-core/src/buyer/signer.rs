//! The signer actor — the buyer's single owner of the Nostr identity.
//!
//! The buyer key is read from `$MOBEE_HOME/key` once at startup and lives only
//! inside this task. Marketplace-event signing (awards, receipts) routes through
//! the queue in later phases so there is one signing principal per home and the
//! secret never leaves the actor. Step 1 exposes the public key; the secret is
//! never sent over the socket or returned to a client.

use nostr_sdk::Keys;
use tokio::sync::{mpsc, oneshot};

use crate::home::{self, HomeError, MobeeHome};

enum Command {
    /// Return the buyer public key (hex). Safe to expose; not secret material.
    PublicKey {
        reply: oneshot::Sender<String>,
    },
}

/// A cheap, cloneable handle to the signer actor.
#[derive(Clone)]
pub struct SignerHandle {
    tx: mpsc::Sender<Command>,
    /// Cached once at spawn so `status` need not round-trip for the common read.
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
    /// The buyer public key (hex), served from the cache set at spawn.
    pub fn public_key_hex(&self) -> &str {
        &self.public_key_hex
    }

    /// The buyer public key (hex), routed through the actor queue. Proves the
    /// serialized signer path end to end (later phases sign over this same slot).
    pub async fn public_key_via_actor(&self) -> Result<String, SignerActorGone> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::PublicKey { reply })
            .await
            .map_err(|_| SignerActorGone)?;
        rx.await.map_err(|_| SignerActorGone)
    }
}

/// Load the buyer key from `home` and spawn the signer actor. The secret is
/// consumed into the task and never held elsewhere.
pub fn spawn(home: &MobeeHome) -> Result<SignerHandle, HomeError> {
    let secret = home::read_secret_key_hex(home)?;
    let keys = Keys::parse(&secret)
        .map_err(|error| HomeError::Key(format!("signer key parse: {error}")))?;
    let public_key_hex = keys.public_key().to_hex();

    let (tx, mut rx) = mpsc::channel::<Command>(64);
    tokio::spawn(async move {
        // `keys` (holding the secret) lives only inside this task.
        while let Some(command) = rx.recv().await {
            match command {
                Command::PublicKey { reply } => {
                    let _ = reply.send(keys.public_key().to_hex());
                }
            }
        }
    });

    Ok(SignerHandle {
        tx,
        public_key_hex,
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
        std::env::temp_dir().join(format!("mobee-buyer-signer-{label}-{}-{id}", std::process::id()))
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
}
