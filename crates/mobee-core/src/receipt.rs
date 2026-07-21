use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    /// Piece-14: SHA-256 hex of the seller-authored NUT-18 payment request (the `creqA…` string),
    /// so both co-signatures commit to the request the seller quoted. `None` for a v1 claim that
    /// carries no `creq` — its preimage hashes byte-identically to the pre-piece-14 form (the slot
    /// is omitted, not null). `Some` once the seller authors a `creq` (Job C); the buyer sources it
    /// from the accepted claim's `creq` tag. A binding-format change, hence the version bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creq_hash: Option<String>,
}

impl ReceiptPreimage {
    /// Canonical JSON array (domain-prefixed, fixed field order) — the signed bytes.
    pub fn canonical_json(&self) -> String {
        let mut fields = vec![
            serde_json::json!(RECEIPT_PREIMAGE_DOMAIN),
            serde_json::json!(self.job_hash),
            serde_json::json!(self.offer_id),
            serde_json::json!(self.amount),
            serde_json::json!(self.unit),
            serde_json::json!(self.mint),
            serde_json::json!(self.buyer_pubkey),
            serde_json::json!(self.seller_pubkey),
            serde_json::json!(self.delivery_integrity_hash),
            serde_json::json!(self.delivery_kind),
            serde_json::json!(self.exec_metadata_commitment),
        ];
        // Piece-14 additive slot at a FIXED final position: folded ONLY when a creq_hash is
        // present, so a None-creq (v1) receipt hashes byte-identically to the pre-piece-14
        // preimage (the regression guard) while a v2 creq changes the co-signed digest.
        if let Some(creq_hash) = &self.creq_hash {
            fields.push(serde_json::json!(creq_hash));
        }
        serde_json::to_string(&serde_json::Value::Array(fields))
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

    #[test]
    fn result_content_hash_is_sha256_hex_of_result_content() {
        assert_eq!(
            result_content_hash_hex("done"),
            "a4c3ed04a95a3da14a9d235c83d868bed7c0f45cf7f3faa751ee8f50598d2211"
        );
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
            creq_hash: None,
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
    fn preimage_canonical_json_is_domain_prefixed() {
        assert!(preimage().canonical_json().starts_with(&format!(
            "[\"{RECEIPT_PREIMAGE_DOMAIN}\""
        )));
    }

    // Piece-14: a None creq_hash omits the additive slot entirely, so the co-signed digest is
    // byte-identical to the pre-piece-14 preimage (the regression guard — v1 claims carry no
    // creq). Some(creq_hash) folds the slot, changing the digest, and different creq_hashes
    // yield different digests.
    #[test]
    fn receipt_preimage_binds_creq_hash_additively() {
        let none = preimage();
        let mut some = preimage();
        some.creq_hash = Some("11".repeat(32));
        let mut other = preimage();
        other.creq_hash = Some("22".repeat(32));

        // None must NOT append a slot: its canonical JSON stops at the exec-metadata field.
        assert!(none
            .canonical_json()
            .ends_with(&format!("\"{EXEC_METADATA_COMMITMENT_EMPTY}\"]")));
        // Some appends the creq_hash as the final array element.
        assert!(some
            .canonical_json()
            .ends_with(&format!("\"{}\"]", "11".repeat(32))));

        assert_ne!(none.digest_hex(), some.digest_hex());
        assert_ne!(some.digest_hex(), other.digest_hex());
        assert_eq!(none.digest_hex().len(), 64);
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
