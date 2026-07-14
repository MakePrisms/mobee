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
