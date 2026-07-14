use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq)]
pub struct TokenDeliveryPayload {
    pub job_id: String,
    pub result_id: String,
    pub mint_url: String,
    pub amount_sats: u64,
    pub token: String,
    pub seller_pubkey: String,
}

impl TokenDeliveryPayload {
    #[cfg(any(test, feature = "gateway"))]
    fn canonical_json(&self, buyer_pubkey: &str) -> String {
        use serde::ser::{SerializeMap, Serializer};

        let mut json = Vec::new();
        let mut serializer = serde_json::Serializer::new(&mut json);
        let mut map = serializer
            .serialize_map(Some(7))
            .expect("token delivery payload starts a JSON map");
        map.serialize_entry("job_id", &self.job_id)
            .expect("job id is JSON-serializable");
        map.serialize_entry("result_id", &self.result_id)
            .expect("result id is JSON-serializable");
        map.serialize_entry("mint_url", &self.mint_url)
            .expect("mint URL is JSON-serializable");
        map.serialize_entry("amount_sats", &self.amount_sats)
            .expect("amount is JSON-serializable");
        map.serialize_entry("token", &self.token)
            .expect("token is JSON-serializable");
        map.serialize_entry("buyer_pubkey", buyer_pubkey)
            .expect("buyer pubkey is JSON-serializable");
        map.serialize_entry("seller_pubkey", &self.seller_pubkey)
            .expect("seller pubkey is JSON-serializable");
        map.end()
            .expect("token delivery payload closes its JSON map");

        String::from_utf8(json).expect("JSON serializer emits UTF-8")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredToken {
    pub delivery_id: String,
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

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct MemoryTokenDelivery {
    next_id: u64,
    deliveries: Vec<DeliveredToken>,
}

#[cfg(any(test, feature = "test-support"))]
impl MemoryTokenDelivery {
    pub fn deliveries(&self) -> &[DeliveredToken] {
        &self.deliveries
    }

    pub fn record_delivery(
        &mut self,
        _payload: TokenDeliveryPayload,
    ) -> Result<DeliveredToken, DeliveryError> {
        self.next_id += 1;
        let delivered = DeliveredToken {
            delivery_id: format!("mem-delivery-{}", self.next_id),
            relay_success: vec!["memory://token-delivery".into()],
            relay_failed: Vec::new(),
        };
        self.deliveries.push(delivered.clone());
        Ok(delivered)
    }
}

#[cfg(any(test, feature = "test-support"))]
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
        let buyer_pubkey = self.keys.public_key().to_hex();
        let gift_wrap =
            token_delivery_gift_wrap(&self.keys, receiver, payload.canonical_json(&buyer_pubkey))
                .await?;
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

        delivered_token(
            output.val.to_string(),
            output
                .success
                .into_iter()
                .map(|url| url.to_string())
                .collect(),
            output
                .failed
                .into_iter()
                .map(|(url, error)| DeliveryRelayFailure {
                    relay: url.to_string(),
                    error,
                })
                .collect(),
        )
    }
}

#[cfg(any(test, feature = "gateway"))]
fn delivered_token(
    delivery_id: String,
    relay_success: Vec<String>,
    relay_failed: Vec<DeliveryRelayFailure>,
) -> Result<DeliveredToken, DeliveryError> {
    if relay_success.is_empty() {
        return Err(DeliveryError::Transport(
            "no relay accepted the NIP-17 token delivery".into(),
        ));
    }

    Ok(DeliveredToken {
        delivery_id,
        relay_success,
        relay_failed,
    })
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
            payload.canonical_json("buyer"),
            "{\"job_id\":\"job\",\"result_id\":\"result\",\"mint_url\":\"https://testnut.cashu.space\",\"amount_sats\":7,\"token\":\"cashu-token\",\"buyer_pubkey\":\"buyer\",\"seller_pubkey\":\"seller\"}"
        );
    }

    #[test]
    fn memory_delivery_records_metadata_only() {
        let mut delivery = MemoryTokenDelivery::default();

        let delivered = delivery.record_delivery(payload()).unwrap();

        assert_eq!(delivered.delivery_id, "mem-delivery-1");
        assert_eq!(delivered.relay_success, ["memory://token-delivery"]);
        assert!(delivered.relay_failed.is_empty());
        assert_eq!(delivery.deliveries(), &[delivered]);
    }

    #[test]
    fn delivery_fails_closed_when_no_relay_accepts() {
        let error = delivered_token(
            "event".into(),
            Vec::new(),
            vec![DeliveryRelayFailure {
                relay: "wss://relay.example".into(),
                error: "rejected".into(),
            }],
        )
        .unwrap_err();

        assert_eq!(
            error,
            DeliveryError::Transport("no relay accepted the NIP-17 token delivery".into())
        );
    }

    #[test]
    fn delivery_preserves_partial_relay_outcome() {
        let failed = DeliveryRelayFailure {
            relay: "wss://failed.example".into(),
            error: "rejected".into(),
        };
        let delivered = delivered_token(
            "event".into(),
            vec!["wss://accepted.example".into()],
            vec![failed.clone()],
        )
        .unwrap();

        assert_eq!(delivered.relay_success, ["wss://accepted.example"]);
        assert_eq!(delivered.relay_failed, [failed]);
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
        use nostr_sdk::{
            nips::nip59::UnwrappedGift,
            prelude::{JsonUtil, Keys, Kind, PublicKey, Tag},
        };

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
            seller_pubkey: receiver.public_key().to_hex(),
        };
        let plaintext = payload.canonical_json(&sender.public_key().to_hex());

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

        // Unwrap the exact publishable artifact as the recipient does. This proves the
        // encrypted rumor is a decryptable kind-14 DM addressed to the seller, rather than
        // merely checking that the outer event looks opaque.
        let unwrapped = block_on(UnwrappedGift::from_gift_wrap(&receiver, &gift_wrap))
            .expect("recipient decrypts and authenticates the gift wrap");
        assert_eq!(unwrapped.sender, sender.public_key());
        assert_eq!(unwrapped.rumor.kind, Kind::PrivateDirectMessage);
        assert_eq!(unwrapped.rumor.content, plaintext);
        assert!(
            unwrapped
                .rumor
                .tags
                .iter()
                .any(|tag| tag == &Tag::public_key(receiver.public_key())),
            "kind-14 rumor is missing its recipient p-tag"
        );

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
            seller_pubkey: "seller".into(),
        }
    }
}
