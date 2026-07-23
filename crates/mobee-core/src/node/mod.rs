//! The persistent per-home **mobee node** (step 1 of the stateful-buyer design, #127).
//!
//! One daemon owns a home. It takes an exclusive OS lock on `$MOBEE_HOME/node.lock`
//! (a second daemon on the same home fails closed), opens the CDK wallet and the
//! Nostr identity behind serialized in-process actors, opens the durable state DB
//! `$MOBEE_HOME/node.sqlite`, and serves a small JSON-RPC surface over the
//! user-only Unix socket `$MOBEE_HOME/node.sock`. Every other process is a thin,
//! stateless [`client`] over that socket.
//!
//! This module is deliberately the *shell*: the boundary that makes financial
//! authority singular and durable. The reservation ledger, auto-award, lifecycle
//! engine, and crash-safe payment saga are later phases that build on this state
//! home. Symmetry: the pieces here (lock / socket / wallet + signer actors /
//! state DB) are named for the node, not the buyer, so a seller service can share
//! the same core in a later phase.
//!
//! Concurrency is 1: the queue behind each actor — not SQLite locking — is the
//! in-process concurrency boundary, mirroring the home lock across processes.

pub mod client;
pub mod lock;
pub mod protocol;
pub mod signer;
pub mod store;
pub mod wallet_actor;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::buyer_fund::{self, FundError};
use crate::home::{HomeError, MobeeHome};
use lock::{HomeLock, LockError};
use protocol::{CODE_INTERNAL, CODE_METHOD_NOT_FOUND, CODE_NOT_IMPLEMENTED, Request, Response};
use signer::SignerHandle;
use store::{NodeStore, StoreError};
use wallet_actor::WalletHandle;

/// Lock file leaf under the home.
pub const LOCK_FILE: &str = "node.lock";
/// State DB leaf under the home.
pub const STATE_DB_FILE: &str = "node.sqlite";
/// Socket leaf under the home.
pub const SOCKET_FILE: &str = "node.sock";

/// Node startup / run failure.
#[derive(Debug)]
pub enum NodeError {
    Lock(LockError),
    Store(StoreError),
    Wallet(FundError),
    Identity(HomeError),
    Io(String),
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lock(error) => write!(formatter, "{error}"),
            Self::Store(error) => write!(formatter, "{error}"),
            Self::Wallet(error) => write!(formatter, "node wallet error: {error}"),
            Self::Identity(error) => write!(formatter, "node identity error: {error}"),
            Self::Io(message) => write!(formatter, "node io error: {message}"),
        }
    }
}

impl std::error::Error for NodeError {}

impl From<LockError> for NodeError {
    fn from(value: LockError) -> Self {
        Self::Lock(value)
    }
}
impl From<StoreError> for NodeError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}
impl From<FundError> for NodeError {
    fn from(value: FundError) -> Self {
        Self::Wallet(value)
    }
}
impl From<HomeError> for NodeError {
    fn from(value: HomeError) -> Self {
        Self::Identity(value)
    }
}

/// Shared, immutable-after-startup handles the connection handlers reach into.
struct NodeContext {
    home: MobeeHome,
    store: NodeStore,
    wallet: WalletHandle,
    signer: SignerHandle,
    started_at_unix: i64,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Bring up the node's owned resources: take the exclusive lock, open the state
/// DB and record the start, then open the wallet and identity behind their
/// serialized actors. Returns the held lock (keep it alive for the node's life),
/// the shared context, and the socket path to bind.
///
/// Fails closed at the lock step if another daemon already owns this home.
async fn bootstrap(home: MobeeHome) -> Result<(HomeLock, Arc<NodeContext>, PathBuf), NodeError> {
    let lock = HomeLock::acquire(home.root.join(LOCK_FILE))?;

    let store = NodeStore::open(home.root.join(STATE_DB_FILE))?;
    let started_at_unix = now_unix();
    store.record_start(started_at_unix)?;

    // The daemon is the ONLY opener of the CDK wallet — this is what the exclusive
    // home lock protects. Opening touches the local sqlite store only (no network).
    let wallet = buyer_fund::open_wallet_async(&home).await?;
    let wallet = wallet_actor::spawn(wallet);

    let signer = signer::spawn(&home)?;

    let socket_path = home.root.join(SOCKET_FILE);
    let context = Arc::new(NodeContext {
        home,
        store,
        wallet,
        signer,
        started_at_unix,
    });
    Ok((lock, context, socket_path))
}

/// Bind the user-only Unix socket, replacing a stale socket file left by a prior
/// run (safe: we already hold the exclusive lock, so no live daemon owns it).
fn bind_socket(path: &std::path::Path) -> Result<UnixListener, NodeError> {
    if path.exists() {
        std::fs::remove_file(path).map_err(|error| NodeError::Io(error.to_string()))?;
    }
    let listener = UnixListener::bind(path).map_err(|error| NodeError::Io(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| NodeError::Io(error.to_string()))?;
    }
    Ok(listener)
}

/// Run the node until the process is terminated. Acquires the home lock (fail
/// closed if held), binds the socket, and serves connections forever.
pub async fn run(home: MobeeHome) -> Result<(), NodeError> {
    // `_lock` is held for the whole run; dropping it releases the OS lock.
    let (_lock, context, socket_path) = bootstrap(home).await?;
    let listener = bind_socket(&socket_path)?;
    accept_loop(listener, context).await
}

/// Accept connections and service each on its own task.
async fn accept_loop(listener: UnixListener, context: Arc<NodeContext>) -> Result<(), NodeError> {
    loop {
        let (stream, _addr) = listener
            .accept()
            .await
            .map_err(|error| NodeError::Io(error.to_string()))?;
        let context = context.clone();
        tokio::spawn(async move {
            // A handler failure never takes down the daemon; the connection is
            // just dropped.
            let _ = handle_connection(stream, context).await;
        });
    }
}

/// One request line in, one response line out.
async fn handle_connection(stream: UnixStream, context: Arc<NodeContext>) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        return Ok(());
    }

    let response = match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => dispatch(&context, request).await,
        Err(error) => Response::err(Value::Null, CODE_METHOD_NOT_FOUND, format!("malformed request: {error}")),
    };

    let mut encoded = serde_json::to_string(&response).unwrap_or_else(|error| {
        format!("{{\"id\":null,\"error\":{{\"code\":{CODE_INTERNAL},\"message\":\"encode failed: {error}\"}}}}")
    });
    encoded.push('\n');
    write_half.write_all(encoded.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Map a request to a response. `status`/`health` is live; the buyer trade
/// methods are recognized but deferred to later phases (they return a structured
/// not-implemented error rather than silently succeeding).
async fn dispatch(context: &NodeContext, request: Request) -> Response {
    let id = request.id.clone();
    match request.method.as_str() {
        "status" | "health" => status(context, id).await,
        "post_job" | "get_job" | "accept_claim" | "authorize_pay" => Response::err(
            id,
            CODE_NOT_IMPLEMENTED,
            format!(
                "{} is deferred to a later phase; step 1 ships the daemon shell (lock, socket, wallet/identity actors, state DB) only",
                request.method
            ),
        ),
        other => Response::err(id, CODE_METHOD_NOT_FOUND, format!("unknown method: {other}")),
    }
}

/// The health/status method: prove the boundary end to end — the state DB, the
/// wallet actor, and the signer actor all answered through the socket. The secret
/// key is never included.
async fn status(context: &NodeContext, id: Value) -> Response {
    let store = context.store.clone();
    let health = tokio::task::spawn_blocking(move || store.health()).await;

    let (schema_version, jobs) = match health {
        Ok(Ok(snapshot)) => (json!(snapshot.schema_version), json!(snapshot.jobs)),
        Ok(Err(error)) => return Response::err(id, CODE_INTERNAL, error.to_string()),
        Err(error) => return Response::err(id, CODE_INTERNAL, format!("state DB task failed: {error}")),
    };

    let mint = context.home.config.default_mint().to_owned();
    let wallet = match context.wallet.balance().await {
        Ok(Ok(balance_sats)) => json!({ "mint": mint, "balance_sats": balance_sats }),
        Ok(Err(error)) => json!({ "mint": mint, "error": error }),
        Err(error) => json!({ "mint": mint, "error": error.to_string() }),
    };

    Response::ok(
        id,
        json!({
            "ok": true,
            "version": crate::version(),
            "home": context.home.root.display().to_string(),
            "socket": context.home.root.join(SOCKET_FILE).display().to_string(),
            "pubkey": context.signer.public_key_hex(),
            "started_at_unix": context.started_at_unix,
            "wallet": wallet,
            "store": {
                "schema_version": schema_version,
                "jobs": jobs,
            },
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::bootstrap as bootstrap_home;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("mobee-node-mod-{label}-{}-{id}", std::process::id()))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_round_trips_over_the_socket() {
        let root = temp_home("status");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap_home(&root).expect("bootstrap home");
        let secret = crate::home::read_secret_key_hex(&home).expect("secret");

        let (_lock, context, socket_path) = bootstrap(home).await.expect("node bootstrap");
        let listener = bind_socket(&socket_path).expect("bind socket");
        let server = tokio::spawn(accept_loop(listener, context));

        // The thin client is synchronous; drive it off the runtime.
        let sock = socket_path.clone();
        let response = tokio::task::spawn_blocking(move || client::status(&sock))
            .await
            .expect("join client")
            .expect("client call");

        let result = response.result.expect("status result");
        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["wallet"]["balance_sats"], json!(0));
        assert_eq!(result["store"]["schema_version"], json!(store::SCHEMA_VERSION));
        let pubkey = result["pubkey"].as_str().expect("pubkey string");
        assert_eq!(pubkey.len(), 64);
        // The socket surface must never leak the secret key.
        assert!(!response_contains(&result, &secret), "status must not echo the secret key");

        // A deferred trade method is recognized but not implemented in step 1.
        let sock = socket_path.clone();
        let deferred = tokio::task::spawn_blocking(move || {
            client::call(&sock, "post_job", json!({}))
        })
        .await
        .expect("join")
        .expect("call");
        let error = deferred.error.expect("post_job must be a structured error in step 1");
        assert_eq!(error.code, CODE_NOT_IMPLEMENTED);

        // Socket is user-only (0600).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&socket_path)
                .expect("socket metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "node.sock must be user-only");
        }

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    fn response_contains(value: &Value, needle: &str) -> bool {
        value.to_string().contains(needle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_node_on_same_home_fails_closed() {
        let root = temp_home("exclusive");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap_home(&root).expect("bootstrap home");

        // First node holds the lock and the wallet.
        let (_lock, _context, _sock) = bootstrap(home.clone()).await.expect("first node");

        // A second bootstrap on the same home must fail closed at the lock — before
        // it ever opens the wallet.
        let second = bootstrap(home).await;
        let failed_closed = matches!(&second, Err(NodeError::Lock(LockError::Held { .. })));
        // Drop any accidentally-acquired context without needing Debug on it.
        drop(second);
        assert!(
            failed_closed,
            "second node must fail closed on the home lock (LockError::Held)"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
