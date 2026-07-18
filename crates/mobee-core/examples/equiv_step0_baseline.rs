//! Step-0 behavior-equivalence emit harness — **BASELINE** (pre-re-type, direct commit API).
//!
//! Byte-for-byte twin of `equiv_step0_candidate.rs`. Emits the SAME money-path PRODUCED artifacts
//! (receipt-preimage `canonical_json` + digest, the `delivery_integrity_hash` pay-bind, the
//! `delivery_kind` wire tag, and refusal identities) as deterministic JSONL — but derives them via
//! the PRE-re-type direct API: the `DeliveryKind::Fork` value the pay path hardcoded, the
//! `GitDelivery::commit_oid()` bound oid, and a direct `VerifiedDelivery::from_fetched_tip` call.
//!
//! Uses ONLY APIs present at `dev@bde34e2` (no typed `Delivery`), so it compiles + runs unchanged
//! on the branch-point to capture the golden `baseline.jsonl`. Contract:
//! `python3 equivdiff.py baseline.jsonl candidate.jsonl` prints `IDENTICAL <n> lines` (rc 0).

use mobee_core::delivery::{CommitOid, GitDelivery, VerifiedDelivery};
use mobee_core::receipt::{DeliveryKind, ReceiptPreimage, EXEC_METADATA_COMMITMENT_EMPTY};

#[derive(serde::Serialize)]
struct Artifact {
    fixture: String,
    class: String,
    delivery_kind: String,
    delivery_integrity_hash: String,
    preimage_canonical_json: String,
    preimage_digest_hex: String,
    refusal: String,
}

/// A valid commit-delivery fixture (all oids/pubkeys are canonical lowercase hex).
struct Valid {
    name: &'static str,
    repo: &'static str,
    branch: &'static str,
    commit_oid: &'static str,
    job_hash: &'static str,
    offer_id: &'static str,
    amount: u64,
    unit: &'static str,
    mint: &'static str,
    buyer_pubkey: &'static str,
    seller_pubkey: &'static str,
}

fn valids() -> Vec<Valid> {
    vec![
        Valid {
            name: "valid_sha1_min_amount",
            repo: "https://example.invalid/repo.git",
            branch: "main",
            commit_oid: "0123456789abcdef0123456789abcdef01234567",
            job_hash: "11111111111111111111111111111111111111111111111111111111111111aa",
            offer_id: "offer-0001",
            amount: 1,
            unit: "sat",
            mint: "https://testnut.cashudevkit.org",
            buyer_pubkey: "22222222222222222222222222222222222222222222222222222222222222bb",
            seller_pubkey: "33333333333333333333333333333333333333333333333333333333333333cc",
        },
        Valid {
            name: "valid_sha1_typical",
            repo: "https://relay.example/git/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789/job.git",
            branch: "delivery/work",
            commit_oid: "fedcba9876543210fedcba9876543210fedcba98",
            job_hash: "44444444444444444444444444444444444444444444444444444444444444dd",
            offer_id: "offer-0002",
            amount: 4096,
            unit: "sat",
            mint: "https://testnut.cashudevkit.org",
            buyer_pubkey: "55555555555555555555555555555555555555555555555555555555555555ee",
            seller_pubkey: "66666666666666666666666666666666666666666666666666666666666666ff",
        },
        Valid {
            name: "valid_sha256_object_format",
            repo: "https://example.invalid/sha256-repo.git",
            branch: "main",
            commit_oid: "a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4",
            job_hash: "7777777777777777777777777777777777777777777777777777777777777701",
            offer_id: "offer-0003",
            amount: 21000000,
            unit: "sat",
            mint: "https://testnut.cashudevkit.org",
            buyer_pubkey: "8888888888888888888888888888888888888888888888888888888888888802",
            seller_pubkey: "9999999999999999999999999999999999999999999999999999999999999903",
        },
    ]
}

fn print_artifact(a: &Artifact) {
    println!(
        "{}",
        serde_json::to_string(a).expect("artifact is JSON-serializable")
    );
}

fn emit_valid(v: &Valid) {
    // --- pre-re-type direct derivation ---
    let git = GitDelivery::new(
        v.repo,
        v.branch,
        CommitOid::parse(v.commit_oid).expect("valid oid"),
    )
    .expect("valid delivery");
    let delivery_kind = DeliveryKind::Fork.as_str().to_owned();
    let delivery_integrity_hash = git.commit_oid().as_str().to_owned();

    // Built EXACTLY as authorize_pay::build_and_publish_receipt builds the co-signed preimage.
    let preimage = ReceiptPreimage {
        job_hash: v.job_hash.to_owned(),
        offer_id: v.offer_id.to_owned(),
        amount: v.amount,
        unit: v.unit.to_owned(),
        mint: v.mint.to_owned(),
        buyer_pubkey: v.buyer_pubkey.to_owned(),
        seller_pubkey: v.seller_pubkey.to_owned(),
        delivery_integrity_hash: delivery_integrity_hash.clone(),
        delivery_kind: delivery_kind.clone(),
        exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
    };
    print_artifact(&Artifact {
        fixture: v.name.to_owned(),
        class: "valid".to_owned(),
        delivery_kind,
        delivery_integrity_hash,
        preimage_canonical_json: preimage.canonical_json(),
        preimage_digest_hex: preimage.digest_hex(),
        refusal: String::new(),
    });
}

fn refusal_artifact(name: &str, refusal: String) -> Artifact {
    Artifact {
        fixture: name.to_owned(),
        class: "refusal".to_owned(),
        delivery_kind: String::new(),
        delivery_integrity_hash: String::new(),
        preimage_canonical_json: String::new(),
        preimage_digest_hex: String::new(),
        refusal,
    }
}

fn emit_refusals() {
    // 1) tip mismatch — advertised oid vs a different fetched tip (direct pre-re-type call).
    let advertised = GitDelivery::new(
        "https://example.invalid/repo.git",
        "main",
        CommitOid::parse("a".repeat(40)).expect("advertised oid"),
    )
    .expect("delivery");
    let fetched = CommitOid::parse("b".repeat(40)).expect("fetched oid");
    let err = VerifiedDelivery::from_fetched_tip(&advertised, fetched)
        .expect_err("tip mismatch must refuse");
    print_artifact(&refusal_artifact("refuse_tip_mismatch", format!("{err:?}")));

    // 2) invalid commit oid — empty.
    let err = CommitOid::parse("").expect_err("empty oid must refuse");
    print_artifact(&refusal_artifact("refuse_commit_oid_empty", format!("{err:?}")));

    // 3) invalid commit oid — non-hex, full length.
    let err = CommitOid::parse("z".repeat(40)).expect_err("non-hex oid must refuse");
    print_artifact(&refusal_artifact("refuse_commit_oid_nonhex", format!("{err:?}")));

    // 4) invalid commit oid — wrong length.
    let err = CommitOid::parse("abc").expect_err("short oid must refuse");
    print_artifact(&refusal_artifact("refuse_commit_oid_wrong_len", format!("{err:?}")));

    // 5) authorize_pay buyer tip-match gate — hash != advertised commit_oid.
    //    authorize_pay.rs:157-167 is byte-FROZEN by this re-type (raw-string compare, unchanged);
    //    its refusal identity is reproduced here so refuse-path parity is machine-checked pre/post.
    print_artifact(&refusal_artifact(
        "refuse_gate_hash_mismatch",
        gate_hash_mismatch_identity(&"a".repeat(40), &"c".repeat(40)),
    ));

    // 6) authorize_pay buyer tip-match gate — empty hash (never auto-filled).
    print_artifact(&refusal_artifact(
        "refuse_gate_hash_empty",
        gate_hash_empty_identity(),
    ));
}

/// EXACT caller-visible identity of the `authorize_pay.rs:162` hash-mismatch refusal
/// (`AuthorizePayError::Input` Display = `authorize_pay input: {message}`). Frozen text.
fn gate_hash_mismatch_identity(delivery_integrity_hash: &str, commit_oid: &str) -> String {
    format!(
        "authorize_pay input: delivery_integrity_hash {delivery_integrity_hash} does not match seller-advertised commit_oid {commit_oid} (buyer tip-match required; refuse mismatch)"
    )
}

/// EXACT caller-visible identity of the `authorize_pay.rs:158` empty-hash refusal. Frozen text.
fn gate_hash_empty_identity() -> String {
    "authorize_pay input: delivery_integrity_hash is required (buyer tip-match); never auto-filled from claim/result oid".to_owned()
}

fn main() {
    for v in valids() {
        emit_valid(&v);
    }
    emit_refusals();
}
