use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq)]
pub struct PaymentPayload {
    pub job_id: String,
    pub result_id: String,
    pub mint_url: String,
    pub amount_sats: u64,
    #[cfg(feature = "wallet")]
    pub token: cashu::Token,
    pub seller_pubkey: String,
}

impl fmt::Debug for PaymentPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PaymentPayload(<redacted>)")
    }
}

impl PaymentPayload {
    #[cfg(all(any(test, feature = "gateway"), feature = "wallet"))]
    fn canonical_json(&self, buyer_pubkey: &str) -> String {
        use serde::ser::{SerializeMap, Serializer};

        let mut json = Vec::new();
        let mut serializer = serde_json::Serializer::new(&mut json);
        let mut map = serializer
            .serialize_map(Some(7))
            .expect("payment payload starts a JSON map");
        map.serialize_entry("job_id", &self.job_id)
            .expect("job id is JSON-serializable");
        map.serialize_entry("result_id", &self.result_id)
            .expect("result id is JSON-serializable");
        map.serialize_entry("mint_url", &self.mint_url)
            .expect("mint URL is JSON-serializable");
        map.serialize_entry("amount_sats", &self.amount_sats)
            .expect("amount is JSON-serializable");
        map.serialize_entry("token", &self.token.to_string())
            .expect("token is JSON-serializable");
        map.serialize_entry("buyer_pubkey", buyer_pubkey)
            .expect("buyer pubkey is JSON-serializable");
        map.serialize_entry("seller_pubkey", &self.seller_pubkey)
            .expect("seller pubkey is JSON-serializable");
        map.end().expect("payment payload closes its JSON map");

        String::from_utf8(json).expect("JSON serializer emits UTF-8")
    }
}

#[cfg(all(any(test, feature = "gateway"), feature = "wallet"))]
pub fn parse_nip17_payment_payload(json: &str) -> Result<PaymentPayload, PaymentSendError> {
    let envelope: PaymentEnvelope = serde_json::from_str(json).map_err(|error| {
        PaymentSendError::Transport(format!("invalid NIP-17 payment payload JSON: {error}"))
    })?;
    envelope.try_into()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentSent {
    pub payment_id: String,
    pub relay_success: Vec<String>,
    pub relay_failed: Vec<PaymentRelayFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRelayFailure {
    pub relay: String,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentSendError {
    Transport(String),
}

impl fmt::Display for PaymentSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "payment send failed: {message}"),
        }
    }
}

impl std::error::Error for PaymentSendError {}

#[allow(async_fn_in_trait)]
pub trait PaymentSend {
    async fn send_payment(
        &mut self,
        payload: PaymentPayload,
    ) -> Result<PaymentSent, PaymentSendError>;
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct MemoryPaymentSend {
    next_id: u64,
    payments: Vec<PaymentSent>,
}

#[cfg(any(test, feature = "test-support"))]
impl MemoryPaymentSend {
    pub fn payments(&self) -> &[PaymentSent] {
        &self.payments
    }

    pub fn record_payment(
        &mut self,
        _payload: PaymentPayload,
    ) -> Result<PaymentSent, PaymentSendError> {
        self.next_id += 1;
        let sent = PaymentSent {
            payment_id: format!("mem-payment-{}", self.next_id),
            relay_success: vec!["memory://payment-send".into()],
            relay_failed: Vec::new(),
        };
        self.payments.push(sent.clone());
        Ok(sent)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl PaymentSend for MemoryPaymentSend {
    async fn send_payment(
        &mut self,
        payload: PaymentPayload,
    ) -> Result<PaymentSent, PaymentSendError> {
        self.record_payment(payload)
    }
}

#[cfg(all(feature = "gateway", feature = "wallet"))]
pub struct NostrPaymentSend {
    relay: String,
    keys: nostr_sdk::prelude::Keys,
}

#[cfg(feature = "gateway")]
const GIFT_WRAP_TIMESTAMP_TWEAK_MAX_SECS: u64 = 180;

#[cfg(all(feature = "gateway", feature = "wallet"))]
impl NostrPaymentSend {
    pub fn new(relay: impl Into<String>, keys: nostr_sdk::prelude::Keys) -> Self {
        Self {
            relay: relay.into(),
            keys,
        }
    }
}

#[cfg(all(feature = "gateway", feature = "wallet"))]
impl PaymentSend for NostrPaymentSend {
    async fn send_payment(
        &mut self,
        payload: PaymentPayload,
    ) -> Result<PaymentSent, PaymentSendError> {
        use nostr_sdk::prelude::{Client, PublicKey};

        let receiver = PublicKey::parse(&payload.seller_pubkey).map_err(|error| {
            PaymentSendError::Transport(format!(
                "invalid seller pubkey for NIP-17 payment send: {error}"
            ))
        })?;
        let buyer_pubkey = self.keys.public_key().to_hex();
        let gift_wrap =
            payment_send_gift_wrap(&self.keys, receiver, payload.canonical_json(&buyer_pubkey))
                .await?;
        let client = Client::new(self.keys.clone());
        client.add_relay(&self.relay).await.map_err(|error| {
            PaymentSendError::Transport(format!("failed to add relay: {error}"))
        })?;
        client.connect().await;

        let output = client
            .send_event_to([self.relay.as_str()], &gift_wrap)
            .await;
        client.disconnect().await;
        let output = output.map_err(|error| {
            PaymentSendError::Transport(format!("failed to send NIP-17 payment: {error}"))
        })?;

        payment_sent(
            output.val.to_string(),
            output
                .success
                .into_iter()
                .map(|url| url.to_string())
                .collect(),
            output
                .failed
                .into_iter()
                .map(|(url, error)| PaymentRelayFailure {
                    relay: url.to_string(),
                    error,
                })
                .collect(),
        )
    }
}

#[cfg(any(test, feature = "gateway"))]
fn payment_sent(
    payment_id: String,
    relay_success: Vec<String>,
    relay_failed: Vec<PaymentRelayFailure>,
) -> Result<PaymentSent, PaymentSendError> {
    if relay_success.is_empty() {
        return Err(PaymentSendError::Transport(
            "no relay accepted the NIP-17 payment".into(),
        ));
    }

    Ok(PaymentSent {
        payment_id,
        relay_success,
        relay_failed,
    })
}

#[cfg(feature = "gateway")]
async fn payment_send_gift_wrap(
    keys: &nostr_sdk::prelude::Keys,
    receiver: nostr_sdk::prelude::PublicKey,
    message: String,
) -> Result<nostr_sdk::prelude::Event, PaymentSendError> {
    use nostr_sdk::nostr::nips::nip44;
    use nostr_sdk::prelude::{EventBuilder, JsonUtil, Kind, Tag};

    let rumor = EventBuilder::private_msg_rumor(receiver, message).build(keys.public_key());
    let seal = EventBuilder::seal(keys, &receiver, rumor)
        .await
        .map_err(|error| {
            PaymentSendError::Transport(format!("failed to build NIP-17 seal: {error}"))
        })?
        .sign(keys)
        .await
        .map_err(|error| {
            PaymentSendError::Transport(format!("failed to sign NIP-17 seal: {error}"))
        })?;
    let wrapping_keys = nostr_sdk::prelude::Keys::generate();
    let content = nip44::encrypt(
        wrapping_keys.secret_key(),
        &receiver,
        seal.as_json(),
        nip44::Version::default(),
    )
    .map_err(|error| {
        PaymentSendError::Transport(format!("failed to encrypt NIP-17 gift wrap: {error}"))
    })?;

    EventBuilder::new(Kind::GiftWrap, content)
        .tags([Tag::public_key(receiver)])
        .custom_created_at(fresh_gift_wrap_created_at())
        .sign_with_keys(&wrapping_keys)
        .map_err(|error| {
            PaymentSendError::Transport(format!("failed to sign NIP-17 gift wrap: {error}"))
        })
}

#[cfg(all(any(test, feature = "gateway"), feature = "wallet"))]
#[derive(Deserialize)]
struct PaymentEnvelope {
    job_id: String,
    result_id: String,
    mint_url: String,
    amount_sats: u64,
    #[serde(rename = "token")]
    serialized_token: String,
    seller_pubkey: String,
}

#[cfg(all(any(test, feature = "gateway"), feature = "wallet"))]
impl TryFrom<PaymentEnvelope> for PaymentPayload {
    type Error = PaymentSendError;

    fn try_from(envelope: PaymentEnvelope) -> Result<Self, Self::Error> {
        use std::str::FromStr;

        let token = cashu::Token::from_str(&envelope.serialized_token).map_err(|error| {
            PaymentSendError::Transport(format!("invalid Cashu token in NIP-17 payment: {error}"))
        })?;
        Ok(Self {
            job_id: envelope.job_id,
            result_id: envelope.result_id,
            mint_url: envelope.mint_url,
            amount_sats: envelope.amount_sats,
            token,
            seller_pubkey: envelope.seller_pubkey,
        })
    }
}

#[cfg(feature = "gateway")]
fn fresh_gift_wrap_created_at() -> nostr_sdk::prelude::Timestamp {
    nostr_sdk::prelude::Timestamp::tweaked(0..GIFT_WRAP_TIMESTAMP_TWEAK_MAX_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "wallet")]
    const VALID_CASHU_TOKEN: &str = "cashuBpGFtdWh0dHA6Ly9sb2NhbGhvc3Q6MzMzOGF1Y3NhdGFkaVRoYW5rIHlvdWF0gaJhaUgArSaMTR9YJmFwgaRhYQFhc3hAOWE2ZGJiODQ3YmQyMzJiYTc2ZGIwZGYxOTcyMTZiMjlkM2I4Y2MxNDU1M2NkMjc4MjdmYzFjYzk0MmZlZGI0ZWFjWCEDhhhUP_trhpXfStS6vN6So0qWvc2X3O4NfM-Y1HISZ5JhZPY=";

    #[cfg(feature = "wallet")]
    #[test]
    fn payment_send_payload_canonical_json_is_stable() {
        let payload = payload();

        assert_eq!(
            payload.canonical_json("buyer"),
            format!(
                "{{\"job_id\":\"job\",\"result_id\":\"result\",\"mint_url\":\"https://testnut.cashu.space\",\"amount_sats\":7,\"token\":\"{VALID_CASHU_TOKEN}\",\"buyer_pubkey\":\"buyer\",\"seller_pubkey\":\"seller\"}}"
            )
        );
    }

    #[test]
    fn memory_payment_records_metadata_only() {
        let mut delivery = MemoryPaymentSend::default();

        let delivered = delivery.record_payment(payload()).unwrap();

        assert_eq!(delivered.payment_id, "mem-payment-1");
        assert_eq!(delivered.relay_success, ["memory://payment-send"]);
        assert!(delivered.relay_failed.is_empty());
        assert_eq!(delivery.payments(), &[delivered]);
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn payment_payload_debug_redacts_bearer_token_and_proof_secret() {
        let payload = payload();
        let token_debug = format!("{:?}", payload.token);
        let payload_debug = format!("{payload:?}");

        assert!(
            token_debug
                .contains("9a6dbb847bd232ba76db0df197216b29d3b8cc14553cd27827fc1cc942fedb4e"),
            "negative control lost the known proof secret"
        );
        assert!(!payload_debug.contains(VALID_CASHU_TOKEN));
        assert!(
            !payload_debug
                .contains("9a6dbb847bd232ba76db0df197216b29d3b8cc14553cd27827fc1cc942fedb4e")
        );
        assert_eq!(payload_debug, "PaymentPayload(<redacted>)");
    }

    #[test]
    fn payment_send_fails_closed_when_no_relay_accepts() {
        let error = payment_sent(
            "event".into(),
            Vec::new(),
            vec![PaymentRelayFailure {
                relay: "wss://relay.example".into(),
                error: "rejected".into(),
            }],
        )
        .unwrap_err();

        assert_eq!(
            error,
            PaymentSendError::Transport("no relay accepted the NIP-17 payment".into())
        );
    }

    #[test]
    fn payment_send_preserves_partial_relay_outcome() {
        let failed = PaymentRelayFailure {
            relay: "wss://failed.example".into(),
            error: "rejected".into(),
        };
        let delivered = payment_sent(
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

    #[cfg(feature = "wallet")]
    #[test]
    fn corrupt_wire_token_fails_closed_before_payload_construction() {
        let json = "{\"job_id\":\"job\",\"result_id\":\"result\",\"mint_url\":\"https://testnut.cashu.space\",\"amount_sats\":7,\"token\":\"not-a-cashu-token\",\"buyer_pubkey\":\"buyer\",\"seller_pubkey\":\"seller\"}";

        let error = parse_nip17_payment_payload(json).unwrap_err();

        assert!(
            matches!(error, PaymentSendError::Transport(message) if message.contains("invalid Cashu token in NIP-17 payment"))
        );
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn payment_payload_round_trips_token_identity_at_envelope_boundary() {
        let payload = payload();
        let json = payload.canonical_json("buyer");

        let parsed = parse_nip17_payment_payload(&json).unwrap();

        assert_eq!(parsed.token, payload.token);
        assert_eq!(parsed.token.to_string(), VALID_CASHU_TOKEN);
    }

    #[cfg(all(feature = "gateway", feature = "wallet"))]
    #[test]
    fn gift_wrap_never_leaks_token_or_payload_plaintext() {
        use std::str::FromStr;

        use nostr_sdk::{
            nips::nip59::UnwrappedGift,
            prelude::{JsonUtil, Keys, Kind, PublicKey, Tag},
        };

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
        let payload = PaymentPayload {
            job_id: SECRET_JOB_ID.into(),
            result_id: SECRET_RESULT_ID.into(),
            mint_url: "https://testnut.cashu.space".into(),
            amount_sats: 7,
            token: cashu::Token::from_str(VALID_CASHU_TOKEN).unwrap(),
            seller_pubkey: receiver.public_key().to_hex(),
        };
        let plaintext = payload.canonical_json(&sender.public_key().to_hex());

        // Non-vacuous guard: the token really is present in the plaintext we hand in, so its
        // absence from the wire artifact below is a meaningful result, not a typo.
        assert!(
            plaintext.contains(VALID_CASHU_TOKEN),
            "test setup broken: token missing from canonical payload"
        );

        let recipient = PublicKey::parse(&payload.seller_pubkey).expect("valid recipient pubkey");
        let gift_wrap = block_on(payment_send_gift_wrap(
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
            !wire.contains(VALID_CASHU_TOKEN),
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
        let second = block_on(payment_send_gift_wrap(
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

    fn payload() -> PaymentPayload {
        #[cfg(feature = "wallet")]
        use std::str::FromStr;

        PaymentPayload {
            job_id: "job".into(),
            result_id: "result".into(),
            mint_url: "https://testnut.cashu.space".into(),
            amount_sats: 7,
            #[cfg(feature = "wallet")]
            token: cashu::Token::from_str(VALID_CASHU_TOKEN).unwrap(),
            seller_pubkey: "seller".into(),
        }
    }
}
