//! Buyer single-call collect: verify delivery integrity, auto-pay, materialize the files.
//!
//! The buyer's post-award receive flow is one atomic call. `collect_async` composes the sealed
//! [`authorize_pay`](crate::authorize_pay) money path — spend gate, budget, single-redeem, and
//! mint-compat refusal all live there and are inherited here by construction — with a read-only
//! checkout of the paid delivery's tree from the buyer store. It adds NO new money authority:
//!
//!   1. load the buyer's accept-bind (their recorded acceptance of the seller's result);
//!   2/3. verify integrity + pay: the buyer's tip-match commitment is the oid it accepted
//!        (`bind.commit_oid`); the machine tip-match (fetch the delivered branch, compare its tip to
//!        that oid) runs inside `authorize_pay` BEFORE any spend, so a delivered-oid ≠ bound-oid
//!        mismatch refuses with ZERO spend and NO materialize;
//!   4. materialize: only after the pay above succeeds (or idempotently reconciles) are the
//!      delivered files checked out.
//!
//! Idempotent by attempt id: re-collecting an already-paid job reconciles the payment without a
//! second spend and re-materializes the files.

use std::path::{Path, PathBuf};

use crate::authorize_pay::{self, AuthorizePayError, AuthorizePayOutcome};
use crate::budget::BudgetGate;
use crate::delivery_git::PayPathDeliveryVerifier;
use crate::home::MobeeHome;
use crate::job_lifecycle::{self, JobLifecycleError};

/// Inputs for [`collect_async`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectRequest {
    /// Offer event id (hex) with a prior accept_claim bind.
    pub job_id: String,
    /// Optional output folder NAME (no path separators) under `<home>/results`. `None` ⇒ the job id.
    pub out: Option<String>,
}

/// Successful collect outcome: the pay result plus the materialized delivery.
#[derive(Clone, Debug)]
pub struct CollectOutcome {
    pub pay: AuthorizePayOutcome,
    pub commit_oid: String,
    /// Absolute path the delivery files were checked out to.
    pub path: String,
    /// Sorted relative file list written under `path`.
    pub files: Vec<String>,
}

#[derive(Debug)]
pub enum CollectError {
    Input(String),
    Lifecycle(JobLifecycleError),
    Pay(AuthorizePayError),
    Materialize(String),
}

impl std::fmt::Display for CollectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input(message) => write!(formatter, "collect: {message}"),
            Self::Lifecycle(error) => write!(formatter, "collect: {error}"),
            Self::Pay(error) => write!(formatter, "collect: {error}"),
            Self::Materialize(message) => write!(formatter, "collect materialize: {message}"),
        }
    }
}

impl std::error::Error for CollectError {}

/// Single-call buyer collect: verify integrity + pay + materialize. See the module docs.
///
/// On an integrity mismatch (the delivered branch does not tip at the accepted `commit_oid`), the
/// composed [`authorize_pay`](crate::authorize_pay::authorize_pay_async) refuses BEFORE the budget
/// gate — so this returns [`CollectError::Pay`] with ZERO spend and no files are materialized.
pub async fn collect_async(
    home: &MobeeHome,
    gate: &mut BudgetGate,
    request: CollectRequest,
) -> Result<CollectOutcome, CollectError> {
    // 1. Load the buyer's accept-bind — the delivered commit + pay terms it recorded at accept.
    // Never caller input, so collect always settles the accepted, tip-matched delivery.
    let bind = job_lifecycle::load_accepted_bind(home, &request.job_id)
        .map_err(CollectError::Lifecycle)?
        .ok_or_else(|| {
            CollectError::Input(format!(
                "no accept-bind for job {} (run accept_claim first)",
                request.job_id
            ))
        })?;

    // Resolve + validate the destination BEFORE spending, so a bad `out` name never pays then fails.
    let dest = results_dest(home, &request.job_id, request.out.as_deref())
        .map_err(CollectError::Input)?;

    // 2/3. Verify integrity + pay through the sealed money path. The tip-match commitment is the oid
    // the buyer accepted; the machine tip-match runs inside authorize_pay before any spend.
    // Idempotent by attempt id: a re-collect reconciles without a second spend.
    let pay_request = job_lifecycle::authorize_request_from_bind(
        &bind,
        bind.amount_sats,
        bind.commit_oid.clone(),
    )
    .map_err(CollectError::Lifecycle)?;
    let pay = authorize_pay::authorize_pay_async(home, gate, pay_request)
        .await
        .map_err(CollectError::Pay)?;

    // 4. Materialize the paid delivery's files (read-only checkout from the buyer store). Reached
    // only after the pay above succeeded or reconciled — never on an integrity refusal.
    let store = delivery_store_path(home);
    let store_ref = PayPathDeliveryVerifier::store_ref_for(&bind.commit_oid);
    let files = materialize_delivery(&store, &store_ref, &bind.commit_oid, &dest)
        .map_err(CollectError::Materialize)?;

    Ok(CollectOutcome {
        pay,
        commit_oid: bind.commit_oid,
        path: dest.display().to_string(),
        files,
    })
}

/// Blocking wrapper over [`collect_async`] for the CLI (builds a current-thread runtime).
pub fn collect_blocking(
    home: &MobeeHome,
    gate: &mut BudgetGate,
    request: CollectRequest,
) -> Result<CollectOutcome, CollectError> {
    crate::runtime_guard::refuse_nested_block_on("collect_blocking")
        .map_err(CollectError::Input)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| CollectError::Input(format!("collect runtime: {error}")))?;
    runtime.block_on(collect_async(home, gate, request))
}

/// The buyer store: the local bare repository the pay path retains verified delivery objects in.
/// Mirrors the path [`authorize_pay`](crate::authorize_pay) opens the verifier against.
pub fn delivery_store_path(home: &MobeeHome) -> PathBuf {
    home.root.join("store")
}

/// Resolve a delivery output folder under `<home>/results`. `out` is an optional simple folder NAME
/// — path separators / traversal are refused so a caller can never write outside `results`. `None`
/// ⇒ `<home>/results/<job_id>`.
pub fn results_dest(
    home: &MobeeHome,
    job_id: &str,
    out: Option<&str>,
) -> Result<PathBuf, String> {
    let results = home.root.join("results");
    match out {
        Some(name) => {
            if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
                return Err(
                    "'out' must be a simple folder name (no path separators or '..')".into(),
                );
            }
            Ok(results.join(name))
        }
        None => Ok(results.join(job_id)),
    }
}

/// Check out the tree of `commit_oid` from the buyer store (bare repo) into `dest`, writing each
/// blob to its path and returning the sorted relative file list. Fail-closed: any read/write error
/// aborts (never a partial-but-reported materialization). Resolves the retention ref first, then the
/// raw oid as a fallback.
pub fn materialize_delivery(
    store: &Path,
    store_ref: &str,
    commit_oid: &str,
    dest: &Path,
) -> Result<Vec<String>, String> {
    let repo = git2::Repository::open_bare(store)
        .map_err(|error| format!("open buyer store {}: {error}", store.display()))?;
    let commit = repo
        .revparse_single(store_ref)
        .or_else(|_| repo.revparse_single(commit_oid))
        .map_err(|error| {
            format!("delivery {commit_oid} not found in buyer store (ref {store_ref}): {error}")
        })?
        .peel_to_commit()
        .map_err(|error| error.to_string())?;
    let tree = commit.tree().map_err(|error| error.to_string())?;

    std::fs::create_dir_all(dest).map_err(|error| error.to_string())?;
    let mut files: Vec<String> = Vec::new();
    let mut walk_err: Option<String> = None;
    tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        if entry.kind() != Some(git2::ObjectType::Blob) {
            return git2::TreeWalkResult::Ok;
        }
        let name = match entry.name() {
            Some(name) => name,
            None => {
                walk_err = Some("non-UTF-8 tree entry name".into());
                return git2::TreeWalkResult::Abort;
            }
        };
        let rel = format!("{root}{name}");
        let object = match entry.to_object(&repo) {
            Ok(object) => object,
            Err(error) => {
                walk_err = Some(error.to_string());
                return git2::TreeWalkResult::Abort;
            }
        };
        let Some(blob) = object.as_blob() else {
            return git2::TreeWalkResult::Ok;
        };
        let path = dest.join(&rel);
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                walk_err = Some(error.to_string());
                return git2::TreeWalkResult::Abort;
            }
        }
        if let Err(error) = std::fs::write(&path, blob.content()) {
            walk_err = Some(error.to_string());
            return git2::TreeWalkResult::Abort;
        }
        files.push(rel);
        git2::TreeWalkResult::Ok
    })
    .map_err(|error| error.to_string())?;
    if let Some(error) = walk_err {
        return Err(format!("failed: {error}"));
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home;
    use crate::job_lifecycle::AcceptedBind;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("mobee-collect-{label}-{}-{id}", std::process::id()))
    }

    /// Build a buyer store (bare repo) holding a delivered commit (README.md + src/lib.rs) under its
    /// retention ref, returning the commit hex. Mirrors what the pay path retains post-verify.
    fn seed_store(store: &Path) -> String {
        let repo = git2::Repository::init_bare(store).expect("init store");
        let readme = repo.blob(b"# delivered\n").expect("blob readme");
        let lib = repo.blob(b"pub fn delivered() {}\n").expect("blob lib");
        let mut sub = repo.treebuilder(None).expect("subtree");
        sub.insert("lib.rs", lib, 0o100644).expect("insert lib");
        let sub_oid = sub.write().expect("write subtree");
        let mut top = repo.treebuilder(None).expect("tree");
        top.insert("README.md", readme, 0o100644).expect("insert readme");
        top.insert("src", sub_oid, 0o040000).expect("insert src");
        let tree_oid = top.write().expect("write tree");
        let tree = repo.find_tree(tree_oid).expect("find tree");
        let sig = git2::Signature::now("t", "t@e").expect("sig");
        let commit_oid = repo
            .commit(None, &sig, &sig, "delivery", &tree, &[])
            .expect("commit");
        let commit_hex = commit_oid.to_string();
        repo.reference(
            &PayPathDeliveryVerifier::store_ref_for(&commit_hex),
            commit_oid,
            true,
            "retain",
        )
        .expect("retain ref");
        commit_hex
    }

    // No accept-bind ⇒ collect refuses fail-closed BEFORE any pay, and burns zero spend.
    #[tokio::test(flavor = "current_thread")]
    async fn collect_refuses_without_accept_bind() {
        let root = temp_root("no-bind");
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut gate = BudgetGate::from_home(&home).expect("gate");

        let error = collect_async(
            &home,
            &mut gate,
            CollectRequest {
                job_id: "a".repeat(64),
                out: None,
            },
        )
        .await
        .expect_err("no bind must refuse");
        assert!(
            matches!(error, CollectError::Input(_)) && error.to_string().contains("no accept-bind"),
            "unexpected error: {error}"
        );
        assert_eq!(gate.spent(), 0, "a no-bind refusal must not spend");
        let _ = std::fs::remove_dir_all(&root);
    }

    // `results_dest` maps to <home>/results/<job_id> by default and refuses traversal in `out`.
    #[test]
    fn results_dest_defaults_to_job_and_refuses_traversal() {
        let root = temp_root("dest");
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let job = "b".repeat(64);
        assert_eq!(
            results_dest(&home, &job, None).expect("default"),
            home.root.join("results").join(&job)
        );
        assert_eq!(
            results_dest(&home, &job, Some("out-1")).expect("named"),
            home.root.join("results").join("out-1")
        );
        for bad in ["../escape", "a/b", "a\\b", ".."] {
            assert!(results_dest(&home, &job, Some(bad)).is_err(), "must refuse {bad:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // `materialize_delivery` writes the delivered tree to disk and returns the sorted file list;
    // re-running is idempotent (same files, overwritten in place — the re-collect materialize path).
    #[test]
    fn materialize_delivery_writes_files_and_is_idempotent() {
        let root = temp_root("materialize");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let commit_hex = seed_store(&store);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        let dest = root.join("results").join("job");

        let files = materialize_delivery(&store, &store_ref, &commit_hex, &dest).expect("materialize");
        assert_eq!(files, vec!["README.md".to_string(), "src/lib.rs".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dest.join("README.md")).expect("readme"),
            "# delivered\n"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("src/lib.rs")).expect("lib"),
            "pub fn delivered() {}\n"
        );

        // Idempotent re-materialize (what re-collect does after a reconciled pay): same result.
        let again = materialize_delivery(&store, &store_ref, &commit_hex, &dest).expect("re-materialize");
        assert_eq!(again, files);
        let _ = std::fs::remove_dir_all(&root);
    }

    // A bind that pins a commit absent from the store fails closed with a precise reason (never a
    // partial/empty "success"). Guards the materialize half of collect.
    #[test]
    fn materialize_delivery_refuses_missing_commit() {
        let root = temp_root("missing");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        git2::Repository::init_bare(&store).expect("init store");
        let missing = "a".repeat(40);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&missing);
        let error = materialize_delivery(&store, &store_ref, &missing, &root.join("out"))
            .expect_err("missing commit must refuse");
        assert!(error.contains("not found in buyer store"), "unexpected: {error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Helper: a from-scratch accept-bind pinning `commit_oid` (used by refuse-path tests).
    fn bind_for(job_id: &str, seller_hex: &str, commit_oid: &str) -> AcceptedBind {
        AcceptedBind {
            job_id: job_id.to_owned(),
            claim_id: "c".repeat(64),
            result_id: "d".repeat(64),
            seller_pubkey: seller_hex.to_owned(),
            commit_oid: commit_oid.to_owned(),
            repo: "https://example.invalid/repo.git".into(),
            branch: "main".into(),
            job_hash: "e".repeat(64),
            amount_sats: 2,
            accept_event_id: "f".repeat(64),
            accepted_at: 1,
            seller_signature: String::new(),
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        }
    }

    // Belt-and-suspenders money gate: a bind carrying a FORGED seller co-signature (a real schnorr
    // sig by a non-seller key) makes the sealed pay path refuse at the pre-pay cosig tooth — BEFORE
    // any spend and BEFORE the wallet opens. collect must surface that refusal, spend ZERO, and
    // materialize NO files. Red-on-revert: rewiring collect to materialize regardless of the pay
    // outcome would leave files on disk here.
    #[tokio::test(flavor = "current_thread")]
    async fn collect_forged_cosig_blocks_pay_and_materialize_zero_spend() {
        use nostr_sdk::secp256k1::Message;
        use nostr_sdk::Keys;

        let root = temp_root("forged-cosig");
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let seller_hex = home::public_key_hex(&home).expect("pubkey");

        // Seed the store so the ONLY thing stopping materialize is the pay refusal (not a missing
        // object) — proving collect gates materialize on the pay outcome.
        let commit_hex = seed_store(&delivery_store_path(&home));

        // A real schnorr signature over the honest receipt-preimage digest, but by an unrelated key.
        // We do not need the exact preimage bytes: any signature not from the seller anchor is
        // refused by the pre-pay cosig tooth.
        let attacker = Keys::generate();
        let forged = attacker
            .sign_schnorr(&Message::from_digest([0x11u8; 32]))
            .to_string();
        let mut bind = bind_for(&"a".repeat(64), &seller_hex, &commit_hex);
        bind.seller_signature = forged;
        write_bind(&home, &bind);

        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let error = collect_async(
            &home,
            &mut gate,
            CollectRequest {
                job_id: bind.job_id.clone(),
                out: None,
            },
        )
        .await
        .expect_err("forged cosig must block collect");
        assert!(matches!(error, CollectError::Pay(_)), "must be a pay refusal: {error}");
        assert_eq!(gate.spent(), 0, "a pay refusal must burn zero spend");
        assert_eq!(
            BudgetGate::from_home(&home).expect("reload").spent(),
            0,
            "durable spent must stay 0"
        );
        assert!(
            !home.root.join("payment-journal").exists(),
            "no payment journal may be created on a pre-pay refusal"
        );
        assert!(
            !home.root.join("results").join(&bind.job_id).exists(),
            "collect must NOT materialize files when the pay refuses"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn write_bind(home: &MobeeHome, bind: &AcceptedBind) {
        let jobs = home.root.join("jobs");
        std::fs::create_dir_all(&jobs).expect("jobs dir");
        std::fs::write(
            jobs.join(format!("{}.json", bind.job_id)),
            serde_json::to_string(bind).expect("serialize bind"),
        )
        .expect("write bind");
    }
}
