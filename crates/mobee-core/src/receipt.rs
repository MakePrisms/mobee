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
}
