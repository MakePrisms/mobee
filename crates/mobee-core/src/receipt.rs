use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const RECEIPT_HASH_DOMAIN: &str = "mobee/v1/receipt";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptHashInput {
    /// Market/offer job id. Keep this tuple stable if execution ids are split out later.
    pub job_id: String,
    pub result_content_hash: String,
    pub price_int: u64,
    pub unit: String,
    pub mint_url: String,
    pub buyer_pubkey_hex: String,
    pub seller_pubkey_hex: String,
}

impl ReceiptHashInput {
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(&serde_json::json!([
            RECEIPT_HASH_DOMAIN,
            self.job_id,
            self.result_content_hash,
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
            result_content_hash: "result-content-hash".into(),
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
            "[\"mobee/v1/receipt\",\"job\",\"result-content-hash\",7,\"sat\",\"https://testnut.cashu.space\",\"buyer\",\"seller\"]"
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
}
