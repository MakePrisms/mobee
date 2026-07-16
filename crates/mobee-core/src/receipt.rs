use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const RECEIPT_HASH_DOMAIN: &str = "mobee/v1/receipt";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptHashInput {
    /// Market/offer job id; this is part of the receipt hash tuple and must not change.
    pub job_id: String,
    /// Integrity identifier for the delivered work (commit oid for git delivery).
    pub delivery_integrity_hash: String,
    /// Integer payment amount for the receipt.
    pub price_int: u64,
    /// Payment unit for `price_int`, such as `sat`.
    pub unit: String,
    /// Cashu mint URL that issued the payment proofs.
    pub mint_url: String,
    /// Hex-encoded buyer Nostr public key bound into the receipt.
    pub buyer_pubkey_hex: String,
    /// Hex-encoded seller Nostr public key bound into the receipt.
    pub seller_pubkey_hex: String,
}

impl ReceiptHashInput {
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(&serde_json::json!([
            RECEIPT_HASH_DOMAIN,
            self.job_id,
            self.delivery_integrity_hash,
            self.price_int,
            self.unit,
            self.mint_url,
            self.buyer_pubkey_hex,
            self.seller_pubkey_hex,
        ]))
        .expect("receipt hash input is JSON-serializable")
    }

    pub fn hash_hex(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_json().as_bytes());
        hex::encode(hasher.finalize())
    }
}

pub fn result_content_hash_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

/// Domain separator for the piece-9 co-signed receipt preimage (distinct from the
/// receipt H-tuple domain above so the two hashes can never collide).
pub const RECEIPT_PREIMAGE_DOMAIN: &str = "mobee/v1/receipt-preimage";

/// Marker committed in [`ReceiptPreimage::exec_metadata_commitment`] when no
/// exec-metadata is folded into the co-signature (the default today — see the type doc).
pub const EXEC_METADATA_COMMITMENT_EMPTY: &str = "none";

/// Delivered git object kind bound (non-forgeably) into the co-signed receipt preimage.
///
/// In the preimage the kind is a signed field, so an unsigned path cannot be flipped to
/// reinterpret the same 40-hex as a commit vs a tree oid (piece-9 D4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryKind {
    /// Fork-tip `commit_oid` (piece-7 git delivery — the only live kind today).
    Fork,
    /// (Deferred) patch result `tree_oid`.
    Patch,
}

impl DeliveryKind {
    /// Wire label used in the preimage and the kind-3400 `delivery_kind` tag.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fork => "fork",
            Self::Patch => "patch",
        }
    }
}

/// The message both parties schnorr-sign for a kind-3400 receipt (piece-9 Item 1).
///
/// Binds the trade **and** the delivered git object (D4: `delivery_integrity_hash` +
/// `delivery_kind`).
///
/// Two deliberate deviations from the literal spec preimage, FLAGGED for operator
/// ratification (money-semantics — do not silently "fix"):
/// - **`result_id` is EXCLUDED.** It is the seller's own kind-6109 event id, unknowable
///   when the seller signs at delivery (the signature is a tag *inside* that very event,
///   so including its id is circular). The result is still bound to the receipt by the
///   `["e", result_id, "", "reply"]` tag under the buyer's event-level nostr signature.
/// - **`exec_metadata_commitment` carries [`EXEC_METADATA_COMMITMENT_EMPTY`] today.** The
///   field is speced (Item 1) but the co-signature does not yet cover exec-metadata: Item
///   2 states `sig/seller` does not cover it (seller-claimed, result-authoritative), and
///   the buyer filters the echo — folding a filtered set into the digest would break
///   signature matching. Exec-metadata rides the events as unsigned tags; the commitment
///   is walk-forward (populating it later is additive, never a retraction).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptPreimage {
    pub job_hash: String,
    pub offer_id: String,
    pub amount: u64,
    pub unit: String,
    pub mint: String,
    /// Buyer nostr x-only pubkey hex (== offer author; the external anchor).
    pub buyer_pubkey: String,
    /// Seller nostr x-only pubkey hex (== accepted-claim seller; the external anchor).
    pub seller_pubkey: String,
    pub delivery_integrity_hash: String,
    /// `fork` | `patch` — see [`DeliveryKind`].
    pub delivery_kind: String,
    /// Commitment over the echoed exec-metadata tag set, or [`EXEC_METADATA_COMMITMENT_EMPTY`].
    pub exec_metadata_commitment: String,
}

impl ReceiptPreimage {
    /// Canonical JSON array (domain-prefixed, fixed field order) — the signed bytes.
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(&serde_json::json!([
            RECEIPT_PREIMAGE_DOMAIN,
            self.job_hash,
            self.offer_id,
            self.amount,
            self.unit,
            self.mint,
            self.buyer_pubkey,
            self.seller_pubkey,
            self.delivery_integrity_hash,
            self.delivery_kind,
            self.exec_metadata_commitment,
        ]))
        .expect("receipt preimage is JSON-serializable")
    }

    /// SHA-256 digest both parties sign (schnorr `Message::from_digest`).
    pub fn digest_bytes(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_json().as_bytes());
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&hasher.finalize());
        bytes
    }

    /// Lowercase-hex form of [`Self::digest_bytes`].
    pub fn digest_hex(&self) -> String {
        hex::encode(self.digest_bytes())
    }
}

/// Canonical commitment over an echoed exec-metadata tag set (piece-9 Item 1 hook).
///
/// Empty set → [`EXEC_METADATA_COMMITMENT_EMPTY`]. Otherwise a SHA-256 over the canonical
/// JSON of the tag rows (the caller passes the already-filtered canonical set, in order).
pub fn exec_metadata_commitment(tags: &[Vec<String>]) -> String {
    if tags.is_empty() {
        return EXEC_METADATA_COMMITMENT_EMPTY.to_owned();
    }
    let mut hasher = Sha256::new();
    hasher.update(
        serde_json::to_string(tags)
            .expect("exec-metadata tags are JSON-serializable")
            .as_bytes(),
    );
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ReceiptHashInput {
        ReceiptHashInput {
            job_id: "job".into(),
            delivery_integrity_hash: "delivery-integrity-hash".into(),
            price_int: 7,
            unit: "sat".into(),
            mint_url: "https://testnut.cashu.space".into(),
            buyer_pubkey_hex: "buyer".into(),
            seller_pubkey_hex: "seller".into(),
        }
    }

    #[test]
    fn canonical_json_matches_locked_receipt_tuple_order() {
        assert_eq!(
            input().canonical_json(),
            "[\"mobee/v1/receipt\",\"job\",\"delivery-integrity-hash\",7,\"sat\",\"https://testnut.cashu.space\",\"buyer\",\"seller\"]"
        );
    }

    #[test]
    fn result_content_hash_is_sha256_hex_of_result_content() {
        assert_eq!(
            result_content_hash_hex("done"),
            "a4c3ed04a95a3da14a9d235c83d868bed7c0f45cf7f3faa751ee8f50598d2211"
        );
    }

    #[test]
    fn hash_changes_when_any_contract_field_changes() {
        let base = input().hash_hex();
        let mut changed = input();
        changed.price_int = 8;

        assert_ne!(base, changed.hash_hex());
        assert_eq!(base.len(), 64);
    }

    #[test]
    fn receipt_hash_binds_the_verified_delivery_oid() {
        let first = input();
        let mut second = first.clone();
        second.delivery_integrity_hash = "b".repeat(40);

        assert_ne!(first.hash_hex(), second.hash_hex());
    }

    #[test]
    fn legacy_result_content_hash_field_name_refuses_to_deserialize() {
        let legacy = serde_json::json!({
            "job_id": "job",
            "result_content_hash": "legacy-hash",
            "price_int": 7,
            "unit": "sat",
            "mint_url": "https://testnut.cashu.space",
            "buyer_pubkey_hex": "buyer",
            "seller_pubkey_hex": "seller"
        });

        assert!(serde_json::from_value::<ReceiptHashInput>(legacy).is_err());
    }

    #[test]
    fn receipt_delivery_integrity_hash_round_trips() {
        let original = input();
        let json = serde_json::to_value(&original).expect("serialize receipt");
        assert!(json.get("delivery_integrity_hash").is_some());
        assert!(json.get("result_content_hash").is_none());

        let parsed: ReceiptHashInput =
            serde_json::from_value(json).expect("deserialize receipt");
        assert_eq!(parsed, original);
    }

    fn preimage() -> ReceiptPreimage {
        ReceiptPreimage {
            job_hash: "aa".repeat(32),
            offer_id: "offer".into(),
            amount: 7,
            unit: "sat".into(),
            mint: "https://testnut.cashu.space".into(),
            buyer_pubkey: "bb".repeat(32),
            seller_pubkey: "cc".repeat(32),
            delivery_integrity_hash: "dd".repeat(20),
            delivery_kind: DeliveryKind::Fork.as_str().into(),
            exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.into(),
        }
    }

    #[test]
    fn preimage_digest_binds_the_delivered_object_and_kind() {
        let base = preimage();
        let mut other_hash = base.clone();
        other_hash.delivery_integrity_hash = "ee".repeat(20);
        let mut other_kind = base.clone();
        other_kind.delivery_kind = DeliveryKind::Patch.as_str().into();

        assert_eq!(base.digest_hex().len(), 64);
        assert_ne!(base.digest_hex(), other_hash.digest_hex());
        // Same 40-hex, different kind ⇒ different signed digest (D4 non-forgeable path).
        assert_ne!(base.digest_hex(), other_kind.digest_hex());
    }

    #[test]
    fn preimage_domain_is_distinct_from_the_receipt_htuple_domain() {
        assert_ne!(RECEIPT_PREIMAGE_DOMAIN, RECEIPT_HASH_DOMAIN);
        assert!(preimage().canonical_json().starts_with(&format!(
            "[\"{RECEIPT_PREIMAGE_DOMAIN}\""
        )));
    }

    #[test]
    fn exec_metadata_commitment_empty_marker_and_stability() {
        assert_eq!(exec_metadata_commitment(&[]), EXEC_METADATA_COMMITMENT_EMPTY);
        let tags = vec![
            vec!["harness".into(), "claude-agent-acp".into()],
            vec!["tokens".into(), "3172".into(), "total".into()],
        ];
        let commit = exec_metadata_commitment(&tags);
        assert_eq!(commit.len(), 64);
        assert_eq!(commit, exec_metadata_commitment(&tags));
        assert_ne!(commit, exec_metadata_commitment(&tags[..1]));
    }
}
