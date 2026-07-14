use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenDeliveryPayload {
    pub job_id: String,
    pub result_id: String,
    pub mint_url: String,
    pub amount_sats: u64,
    pub token: String,
    pub buyer_pubkey: String,
    pub seller_pubkey: String,
}

impl TokenDeliveryPayload {
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(self).expect("token delivery payload is JSON-serializable")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredToken {
    pub delivery_id: String,
    pub payload: TokenDeliveryPayload,
    pub relay_success: Vec<String>,
    pub relay_failed: Vec<DeliveryRelayFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryRelayFailure {
    pub relay: String,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryError {
    Transport(String),
}

impl fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "token delivery failed: {message}"),
        }
    }
}

impl std::error::Error for DeliveryError {}

#[allow(async_fn_in_trait)]
pub trait TokenDelivery {
    async fn deliver_token(
        &mut self,
        payload: TokenDeliveryPayload,
    ) -> Result<DeliveredToken, DeliveryError>;
}

#[derive(Default)]
pub struct MemoryTokenDelivery {
    next_id: u64,
    deliveries: Vec<DeliveredToken>,
}

impl MemoryTokenDelivery {
    pub fn deliveries(&self) -> &[DeliveredToken] {
        &self.deliveries
    }

    pub fn record_delivery(
        &mut self,
        payload: TokenDeliveryPayload,
    ) -> Result<DeliveredToken, DeliveryError> {
        self.next_id += 1;
        let delivered = DeliveredToken {
            delivery_id: format!("mem-delivery-{}", self.next_id),
            payload,
            relay_success: Vec::new(),
            relay_failed: Vec::new(),
        };
        self.deliveries.push(delivered.clone());
        Ok(delivered)
    }
}

impl TokenDelivery for MemoryTokenDelivery {
    async fn deliver_token(
        &mut self,
        payload: TokenDeliveryPayload,
    ) -> Result<DeliveredToken, DeliveryError> {
        self.record_delivery(payload)
    }
}

#[cfg(feature = "gateway")]
pub struct NostrTokenDelivery {
    relay: String,
    keys: nostr_sdk::prelude::Keys,
}

#[cfg(feature = "gateway")]
const GIFT_WRAP_TIMESTAMP_TWEAK_MAX_SECS: u64 = 180;

#[cfg(feature = "gateway")]
impl NostrTokenDelivery {
    pub fn new(relay: impl Into<String>, keys: nostr_sdk::prelude::Keys) -> Self {
        Self {
            relay: relay.into(),
            keys,
        }
    }
}

#[cfg(feature = "gateway")]
impl TokenDelivery for NostrTokenDelivery {
    async fn deliver_token(
        &mut self,
        payload: TokenDeliveryPayload,
    ) -> Result<DeliveredToken, DeliveryError> {
        use nostr_sdk::prelude::{Client, PublicKey};

        let receiver = PublicKey::parse(&payload.seller_pubkey).map_err(|error| {
            DeliveryError::Transport(format!(
                "invalid seller pubkey for NIP-17 delivery: {error}"
            ))
        })?;
        let gift_wrap =
            token_delivery_gift_wrap(&self.keys, receiver, payload.canonical_json()).await?;
        let client = Client::new(self.keys.clone());
        client
            .add_relay(&self.relay)
            .await
            .map_err(|error| DeliveryError::Transport(format!("failed to add relay: {error}")))?;
        client.connect().await;

        let output = client
            .send_event_to([self.relay.as_str()], &gift_wrap)
            .await
            .map_err(|error| {
                DeliveryError::Transport(format!("failed to send NIP-17 token delivery: {error}"))
            })?;

        Ok(DeliveredToken {
            delivery_id: output.val.to_string(),
            payload,
            relay_success: output
                .success
                .into_iter()
                .map(|url| url.to_string())
                .collect(),
            relay_failed: output
                .failed
                .into_iter()
                .map(|(url, error)| DeliveryRelayFailure {
                    relay: url.to_string(),
                    error,
                })
                .collect(),
        })
    }
}

#[cfg(feature = "gateway")]
async fn token_delivery_gift_wrap(
    keys: &nostr_sdk::prelude::Keys,
    receiver: nostr_sdk::prelude::PublicKey,
    message: String,
) -> Result<nostr_sdk::prelude::Event, DeliveryError> {
    use nostr_sdk::nostr::nips::nip44;
    use nostr_sdk::prelude::{EventBuilder, JsonUtil, Kind, Tag};

    let rumor = EventBuilder::private_msg_rumor(receiver, message).build(keys.public_key());
    let seal = EventBuilder::seal(keys, &receiver, rumor)
        .await
        .map_err(|error| DeliveryError::Transport(format!("failed to build NIP-17 seal: {error}")))?
        .sign(keys)
        .await
        .map_err(|error| {
            DeliveryError::Transport(format!("failed to sign NIP-17 seal: {error}"))
        })?;
    let wrapping_keys = nostr_sdk::prelude::Keys::generate();
    let content = nip44::encrypt(
        wrapping_keys.secret_key(),
        &receiver,
        seal.as_json(),
        nip44::Version::default(),
    )
    .map_err(|error| {
        DeliveryError::Transport(format!("failed to encrypt NIP-17 gift wrap: {error}"))
    })?;

    EventBuilder::new(Kind::GiftWrap, content)
        .tags([Tag::public_key(receiver)])
        .custom_created_at(fresh_gift_wrap_created_at())
        .sign_with_keys(&wrapping_keys)
        .map_err(|error| {
            DeliveryError::Transport(format!("failed to sign NIP-17 gift wrap: {error}"))
        })
}

#[cfg(feature = "gateway")]
fn fresh_gift_wrap_created_at() -> nostr_sdk::prelude::Timestamp {
    nostr_sdk::prelude::Timestamp::tweaked(0..GIFT_WRAP_TIMESTAMP_TWEAK_MAX_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_delivery_payload_canonical_json_is_stable() {
        let payload = payload();

        assert_eq!(
            payload.canonical_json(),
            "{\"job_id\":\"job\",\"result_id\":\"result\",\"mint_url\":\"https://testnut.cashu.space\",\"amount_sats\":7,\"token\":\"cashu-token\",\"buyer_pubkey\":\"buyer\",\"seller_pubkey\":\"seller\"}"
        );
    }

    #[test]
    fn memory_delivery_records_token_payloads() {
        let mut delivery = MemoryTokenDelivery::default();

        let delivered = delivery.record_delivery(payload()).unwrap();

        assert_eq!(delivered.delivery_id, "mem-delivery-1");
        assert!(delivered.relay_success.is_empty());
        assert!(delivered.relay_failed.is_empty());
        assert_eq!(delivery.deliveries(), &[delivered]);
    }

    #[cfg(feature = "gateway")]
    #[test]
    fn gift_wrap_timestamp_tweak_stays_inside_relay_freshness_window() {
        let now = nostr_sdk::prelude::Timestamp::now();
        let created_at = fresh_gift_wrap_created_at();
        assert!(created_at <= now);
        assert!(
            now.as_secs().saturating_sub(created_at.as_secs()) < GIFT_WRAP_TIMESTAMP_TWEAK_MAX_SECS
        );
    }

    #[cfg(feature = "gateway")]
    #[test]
    fn gift_wrap_never_leaks_token_or_payload_plaintext() {
        use nostr_sdk::prelude::{JsonUtil, Keys, Kind, PublicKey};

        // A recognizable, high-entropy secret so a substring match cannot collide by chance.
        const SECRET_TOKEN: &str = "cashuAsecret-proof-DO-NOT-LEAK-9f3c1a7b0e5d4266";
        const SECRET_JOB_ID: &str = "job-secret-corr-42";
        const SECRET_RESULT_ID: &str = "result-secret-corr-42";

        let sender = Keys::generate();
        let receiver = Keys::generate();

        // Secret-class fields ride ONLY inside the encrypted NIP-17 rumor:
        //   * `token` is bearer ecash / proof material -> the hard money-safety requirement;
        //     any plaintext leak is a spendable secret on a public relay.
        //   * job_id / result_id / mint_url / amount / pubkeys travel inside the same
        //     encrypted payload. Gift-wrap exists to keep that whole envelope private, so we
        //     assert the entire canonical payload (and the correlation ids) are absent too.
        let payload = TokenDeliveryPayload {
            job_id: SECRET_JOB_ID.into(),
            result_id: SECRET_RESULT_ID.into(),
            mint_url: "https://testnut.cashu.space".into(),
            amount_sats: 7,
            token: SECRET_TOKEN.into(),
            buyer_pubkey: receiver.public_key().to_hex(),
            seller_pubkey: receiver.public_key().to_hex(),
        };
        let plaintext = payload.canonical_json();

        // Non-vacuous guard: the token really is present in the plaintext we hand in, so its
        // absence from the wire artifact below is a meaningful result, not a typo.
        assert!(
            plaintext.contains(SECRET_TOKEN),
            "test setup broken: token missing from canonical payload"
        );

        let recipient = PublicKey::parse(&payload.seller_pubkey).expect("valid recipient pubkey");
        let gift_wrap = block_on(token_delivery_gift_wrap(
            &sender,
            recipient,
            plaintext.clone(),
        ))
        .expect("gift wrap builds without network");

        // The publishable artifact is a NIP-59 gift wrap.
        assert_eq!(gift_wrap.kind, Kind::GiftWrap);
        assert_eq!(gift_wrap.kind.as_u16(), 1059);

        // Serialize the ENTIRE publishable event (id, tags, content, sig) exactly as it goes
        // on the wire, and search that whole artifact.
        let wire = gift_wrap.as_json();

        // Core money-safety property: no proof/token material and no plaintext payload
        // survives on the wire.
        assert!(
            !wire.contains(SECRET_TOKEN),
            "token/proof material leaked in plaintext on the wire: {wire}"
        );
        assert!(
            !wire.contains(&plaintext),
            "canonical payload leaked in plaintext on the wire: {wire}"
        );
        assert!(
            !wire.contains(SECRET_JOB_ID) && !wire.contains(SECRET_RESULT_ID),
            "job/result correlation ids leaked in plaintext on the wire: {wire}"
        );

        // Non-vacuous guard: there is real ciphertext and it is what got serialized, so the
        // absence checks above ran against a non-empty artifact.
        assert!(!gift_wrap.content.is_empty(), "gift wrap content is empty");
        assert!(
            wire.contains(&gift_wrap.content),
            "serialized event does not contain its own encrypted content"
        );

        // Negative control: a second wrap of the SAME payload differs on the wire (ephemeral
        // wrapping key + randomized nip44 nonce). A plaintext or deterministic-passthrough
        // path would make these byte-identical, so this proves the absence assertions above
        // are not passing vacuously against a constant or empty artifact.
        let second = block_on(token_delivery_gift_wrap(
            &sender,
            recipient,
            plaintext.clone(),
        ))
        .expect("second gift wrap builds without network");
        assert_ne!(
            wire,
            second.as_json(),
            "two wraps of the same payload are byte-identical; encryption is not randomized"
        );
    }

    // Minimal single-threaded executor: the gift-wrap path is pure CPU crypto (no I/O), so a
    // busy-poll with a no-op waker drives it to completion without pulling in an async runtime.
    #[cfg(feature = "gateway")]
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::pin::Pin;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        unsafe fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);

        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match Pin::new(&mut future).poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn payload() -> TokenDeliveryPayload {
        TokenDeliveryPayload {
            job_id: "job".into(),
            result_id: "result".into(),
            mint_url: "https://testnut.cashu.space".into(),
            amount_sats: 7,
            token: "cashu-token".into(),
            buyer_pubkey: "buyer".into(),
            seller_pubkey: "seller".into(),
        }
    }
}
