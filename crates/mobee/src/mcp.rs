//! Thin MCP stdio surface: `mobee mcp`.
//!
//! Writes are newline-delimited JSON-RPC. Reads accept newline JSON *or* legacy
//! `Content-Length` framing (spike scar — Claude Code hung on LSP-only writes).

use std::io::{BufRead, Write};
use std::sync::Mutex;

use mobee_core::authorize_pay::{self, AuthorizePayRequest};
use mobee_core::budget::BudgetGate;
use mobee_core::home::{self, MobeeHome};
#[cfg(feature = "wallet")]
use mobee_core::job_lifecycle::{
    self, AcceptClaimRequest, GetJobRequest, PostJobRequest, WaitFor,
};
use serde::Deserialize;
use serde_json::{Value, json};

const SUCCESS: i32 = 0;
const RUNTIME_ERROR: i32 = 2;

#[derive(Debug, Deserialize)]
struct McpRequest {
    #[serde(default)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

struct McpState {
    home: MobeeHome,
    gate: Mutex<BudgetGate>,
}

/// Run the MCP server on the provided stdio handles until stdin EOF.
pub fn run(out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let state = match bootstrap_state() {
        Ok(state) => state,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let _ = writeln!(
        err,
        "mobee mcp ready (home={}, key_created={}, mint={}, relay={})",
        state.home.root.display(),
        state.home.key_created,
        state.home.config.mint_url,
        state.home.config.relay_url
    );

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    loop {
        let request = match read_mcp_request(&mut input) {
            Ok(Some(request)) => request,
            Ok(None) => return SUCCESS,
            Err(error) => {
                let _ = writeln!(err, "{error}");
                return RUNTIME_ERROR;
            }
        };
        // Notifications (no id) get no response.
        if request.id.is_none() {
            if request.method == "notifications/initialized" {
                continue;
            }
            let _ = writeln!(err, "ignoring MCP notification {}", request.method);
            continue;
        }
        let response = dispatch(&state, &request);
        if let Err(error) = write_mcp_response(out, &response) {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    }
}

fn bootstrap_state() -> Result<McpState, String> {
    let root = home::default_home_dir().map_err(|error| error.to_string())?;
    let home = home::bootstrap(root).map_err(|error| error.to_string())?;
    let gate = Mutex::new(BudgetGate::from_home(&home).map_err(|error| error.to_string())?);
    Ok(McpState { home, gate })
}

fn dispatch(state: &McpState, request: &McpRequest) -> Value {
    let id = request.id.clone().unwrap_or(Value::Null);
    match request.method.as_str() {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "mobee",
                    "version": mobee_core::version(),
                },
            }),
        ),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tools() })),
        "tools/call" => match call_tool(state, &request.params) {
            Ok(result) => ok(id, result),
            Err(message) => tool_error(id, message),
        },
        other => error_response(id, -32601, format!("method not found: {other}")),
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "setup_wallet",
            "description": "Bootstrap ~/.mobee (config + autogen key + wallet dir) and fund against the hard-pinned testnut mint (mint quote → auto-pay → mint). Returns status + balance; the secret key never appears in the response.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "post_job",
            "description": "Publish a real kind-5109 job offer to the configured mobee relay. Targeted seller p-tag is the documented default (pass seller_pubkey); set untargeted=true for an open offer. Optional repo+branch attach git delivery tags. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string" },
                    "output": { "type": "string", "description": "MIME / output type (e.g. text/plain)" },
                    "amount_sats": { "type": "integer", "minimum": 0 },
                    "seller_pubkey": {
                        "type": "string",
                        "description": "Targeted seller hex pubkey (documented default)"
                    },
                    "untargeted": {
                        "type": "boolean",
                        "description": "When true, omit p-tag (open offer). Default false."
                    },
                    "deadline_unix": { "type": "integer", "minimum": 0 },
                    "repo": { "type": "string", "description": "Optional https git repo for delivery bind" },
                    "branch": { "type": "string" }
                },
                "required": ["task", "output", "amount_sats"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_job",
            "description": "Read job state from the relay (kind 5109 offer + 7000 claims + 6109 results). Surfaces claim created_at and flags the most-recent LIVE claim. Optional wait_for=claim|result long-poll. Local accept-bind attached if present. Never invents claims/results.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "Offer event id (hex)" },
                    "wait_for": { "type": "string", "enum": ["claim", "result"] },
                    "timeout_secs": { "type": "integer", "minimum": 1 }
                },
                "required": ["job_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "accept_claim",
            "description": "Accept a seller claim: publish kind-7000 status=accepted and record local pay-bind {seller_pubkey, result_id, commit_oid, repo, branch, job_hash} for authorize_pay. Requires a matching git result on the relay. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "claim_id": { "type": "string" },
                    "result_id": {
                        "type": "string",
                        "description": "Optional; defaults to newest git result from the claim seller"
                    }
                },
                "required": ["job_id", "claim_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "stub_pay",
            "description": "Exercise the MCP budget gate over a mock pay (allow/deny). Does not call piece-6 run()/advance(). Caps bind from ~/.mobee config only — tool args that set per_job/total are ignored.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount_sats": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Spend amount in sats to authorize against config caps"
                    }
                },
                "required": ["amount_sats"],
                "additionalProperties": true
            }
        },
        {
            "name": "authorize_pay",
            "description": "Real testnut pay: BudgetGate → PayPathDeliveryVerifier → PaymentService::run(). Documented default: job_id form (job_id + amount_sats + buyer-supplied delivery_integrity_hash) binds fields from accept_claim — tip-match hash is NEVER auto-filled from the claim oid (D2). Explicit 9-field form kept for harness. If an accept-bind exists, seller/result/commit mismatches are REFUSED. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "amount_sats": { "type": "integer", "minimum": 0 },
                    "delivery_integrity_hash": {
                        "type": "string",
                        "description": "Buyer commitment — full lowercase git oid that must tip-match (required; never auto-filled)"
                    },
                    "result_id": { "type": "string" },
                    "job_hash": {
                        "type": "string",
                        "description": "Lowercase SHA-256 job digest (64 hex) — explicit form"
                    },
                    "seller_pubkey": { "type": "string" },
                    "repo": {
                        "type": "string",
                        "description": "Seller git locator (https / relay-git only; ext::/file/ssh refused)"
                    },
                    "branch": { "type": "string" },
                    "commit_oid": { "type": "string" }
                },
                "required": ["job_id", "amount_sats", "delivery_integrity_hash"],
                "additionalProperties": true
            }
        }
    ])
}

fn call_tool(state: &McpState, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call missing name".to_owned())?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "setup_wallet" => setup_wallet_fund(&state.home),
        "post_job" => post_job_tool(state, &arguments),
        "get_job" => get_job_tool(state, &arguments),
        "accept_claim" => accept_claim_tool(state, &arguments),
        "stub_pay" => stub_pay(&state.gate, &arguments),
        "authorize_pay" => authorize_pay_tool(state, &arguments),
        other => Err(format!("unknown tool: {other}")),
    }
}

fn setup_wallet_fund(home: &MobeeHome) -> Result<Value, String> {
    // Re-bootstrap so a long-lived MCP process still converges if files were removed.
    let home = home::bootstrap(&home.root).map_err(|error| error.to_string())?;
    #[cfg(feature = "wallet")]
    {
        use mobee_core::buyer_fund::{self, DEFAULT_FUND_AMOUNT_SATS};
        let outcome = buyer_fund::fund_testnut_wallet_blocking(&home, DEFAULT_FUND_AMOUNT_SATS)
            .map_err(|error| error.to_string())?;
        let body = json!({
            "home": home.root.display().to_string(),
            "key_created": home.key_created,
            "key_present": home::key_file_present(&home),
            "wallet_dir": home.wallet_dir.display().to_string(),
            "relay_url": home.config.relay_url,
            "mint_url": home.config.mint_url,
            "per_job_budget_sats": home.config.per_job_budget_sats,
            "total_budget_sats": home.config.total_budget_sats,
            "invoice": outcome.invoice,
            "funded_sats": outcome.funded_sats,
            "balance_sats": outcome.balance_sats,
            "already_funded": outcome.already_funded,
            "next": if outcome.balance_sats > 0 {
                "wallet funded on testnut — use stub_pay to exercise budget caps"
            } else {
                "fund returned balance 0 — unexpected"
            }
        });
        return Ok(tool_ok(body));
    }
    #[cfg(not(feature = "wallet"))]
    {
        let _ = home;
        Err("setup_wallet fund path requires the wallet feature".into())
    }
}

/// Real pay: BudgetGate → PaymentService::run(&mut PayPathDeliveryVerifier) only.
/// Tool args that set per_job/total caps are ignored. Never echoes the secret key.
///
/// Forms:
/// - **job_id (documented default):** job_id + amount_sats + delivery_integrity_hash
///   (buyer tip-match). Other fields filled from accept_claim bind. D2: integrity hash
///   is never auto-filled from the claim/result oid.
/// - **explicit:** all nine fields (harness / stub path). If an accept-bind exists for
///   job_id, seller/result/commit must match (Gate D).
fn authorize_pay_tool(state: &McpState, arguments: &Value) -> Result<Value, String> {
    let require_str = |key: &str| -> Result<String, String> {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("authorize_pay requires {key} (string)"))
    };
    let amount_sats = arguments
        .get("amount_sats")
        .and_then(Value::as_u64)
        .ok_or_else(|| "authorize_pay requires amount_sats (integer)".to_owned())?;
    let delivery_integrity_hash = require_str("delivery_integrity_hash")?;
    let job_id = require_str("job_id")?;

    let explicit = [
        "result_id",
        "job_hash",
        "seller_pubkey",
        "repo",
        "branch",
        "commit_oid",
    ]
    .iter()
    .all(|key| arguments.get(key).and_then(Value::as_str).is_some());

    #[cfg(feature = "wallet")]
    let request = if explicit {
        let request = AuthorizePayRequest {
            job_id: job_id.clone(),
            result_id: require_str("result_id")?,
            delivery_integrity_hash,
            job_hash: require_str("job_hash")?,
            seller_pubkey: require_str("seller_pubkey")?,
            amount_sats,
            repo: require_str("repo")?,
            branch: require_str("branch")?,
            commit_oid: require_str("commit_oid")?,
        };
        if let Some(bind) = job_lifecycle::load_accepted_bind(&state.home, &job_id)
            .map_err(|error| error.to_string())?
        {
            job_lifecycle::assert_authorize_matches_bind(
                &bind,
                &request.seller_pubkey,
                &request.result_id,
                &request.commit_oid,
            )
            .map_err(|error| error.to_string())?;
        }
        request
    } else {
        let bind = job_lifecycle::load_accepted_bind(&state.home, &job_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                "authorize_pay(job_id) requires a prior accept_claim bind for this job (or pass the explicit 9-field form)".to_owned()
            })?;
        job_lifecycle::authorize_request_from_bind(&bind, amount_sats, delivery_integrity_hash)
            .map_err(|error| error.to_string())?
    };

    #[cfg(not(feature = "wallet"))]
    let request = {
        let _ = explicit;
        AuthorizePayRequest {
            job_id,
            result_id: require_str("result_id")?,
            delivery_integrity_hash,
            job_hash: require_str("job_hash")?,
            seller_pubkey: require_str("seller_pubkey")?,
            amount_sats,
            repo: require_str("repo")?,
            branch: require_str("branch")?,
            commit_oid: require_str("commit_oid")?,
        }
    };

    let mut gate = state
        .gate
        .lock()
        .map_err(|_| "budget gate lock poisoned".to_owned())?;

    let outcome = authorize_pay::authorize_pay(&state.home, &mut gate, request)
        .map_err(|error| error.to_string())?;

    let body = json!({
        "ok": true,
        "amount_sats": outcome.amount_sats,
        "attempt_id": outcome.attempt_id,
        "spent_total_sats": outcome.spent_total_sats,
        "remaining_sats": outcome.remaining_sats,
        "per_job_cap_sats": gate.per_job_cap(),
        "total_cap_sats": gate.total_cap(),
        "state": outcome.state,
        "piece6": "run",
        "verifier": "PayPathDeliveryVerifier",
    });
    Ok(tool_ok(body))
}

#[cfg(feature = "wallet")]
fn post_job_tool(state: &McpState, arguments: &Value) -> Result<Value, String> {
    let require_str = |key: &str| -> Result<String, String> {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("post_job requires {key} (string)"))
    };
    let amount_sats = arguments
        .get("amount_sats")
        .and_then(Value::as_u64)
        .ok_or_else(|| "post_job requires amount_sats (integer)".to_owned())?;
    let untargeted = arguments
        .get("untargeted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let seller_pubkey = arguments
        .get("seller_pubkey")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let deadline_unix = arguments.get("deadline_unix").and_then(Value::as_u64);
    let repo = arguments
        .get("repo")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let branch = arguments
        .get("branch")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let outcome = job_lifecycle::post_job(
        &state.home,
        PostJobRequest {
            task: require_str("task")?,
            output: require_str("output")?,
            amount_sats,
            seller_pubkey,
            untargeted,
            deadline_unix,
            repo,
            branch,
        },
    )
    .map_err(|error| error.to_string())?;

    let body = json!({
        "ok": true,
        "job_id": outcome.job_id,
        "job_hash": outcome.job_hash,
        "offer_kind": outcome.offer_kind,
        "targeted": outcome.targeted,
        "seller_pubkey": outcome.seller_pubkey,
        "amount_sats": outcome.amount_sats,
        "relay_url": outcome.relay_url,
        "task": outcome.task,
        "output": outcome.output,
    });
    // never-echo: secret key must not appear
    let rendered = body.to_string();
    if let Ok(secret) = home::read_secret_key_hex(&state.home) {
        if rendered.contains(&secret) {
            return Err("post_job refused: response would echo secret key".into());
        }
    }
    Ok(tool_ok(body))
}

#[cfg(not(feature = "wallet"))]
fn post_job_tool(_state: &McpState, _arguments: &Value) -> Result<Value, String> {
    Err("post_job requires the wallet feature".into())
}

#[cfg(feature = "wallet")]
fn get_job_tool(state: &McpState, arguments: &Value) -> Result<Value, String> {
    let job_id = arguments
        .get("job_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "get_job requires job_id (string)".to_owned())?
        .to_owned();
    let wait_for = match arguments.get("wait_for").and_then(Value::as_str) {
        Some(raw) => Some(WaitFor::parse(raw).map_err(|error| error.to_string())?),
        None => None,
    };
    let timeout_secs = arguments.get("timeout_secs").and_then(Value::as_u64);
    let view = job_lifecycle::get_job(
        &state.home,
        GetJobRequest {
            job_id,
            wait_for,
            timeout_secs,
        },
    )
    .map_err(|error| error.to_string())?;
    let body = serde_json::to_value(&view).map_err(|error| error.to_string())?;
    let mut body = body;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("ok".into(), json!(true));
        obj.insert("source".into(), json!("relay"));
    }
    Ok(tool_ok(body))
}

#[cfg(not(feature = "wallet"))]
fn get_job_tool(_state: &McpState, _arguments: &Value) -> Result<Value, String> {
    Err("get_job requires the wallet feature".into())
}

#[cfg(feature = "wallet")]
fn accept_claim_tool(state: &McpState, arguments: &Value) -> Result<Value, String> {
    let require_str = |key: &str| -> Result<String, String> {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("accept_claim requires {key} (string)"))
    };
    let result_id = arguments
        .get("result_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let outcome = job_lifecycle::accept_claim(
        &state.home,
        AcceptClaimRequest {
            job_id: require_str("job_id")?,
            claim_id: require_str("claim_id")?,
            result_id,
        },
    )
    .map_err(|error| error.to_string())?;
    let body = json!({
        "ok": true,
        "accept_event_id": outcome.accept_event_id,
        "bind": outcome.bind,
    });
    let rendered = body.to_string();
    if let Ok(secret) = home::read_secret_key_hex(&state.home) {
        if rendered.contains(&secret) {
            return Err("accept_claim refused: response would echo secret key".into());
        }
    }
    Ok(tool_ok(body))
}

#[cfg(not(feature = "wallet"))]
fn accept_claim_tool(_state: &McpState, _arguments: &Value) -> Result<Value, String> {
    Err("accept_claim requires the wallet feature".into())
}

/// Gate-wrapped mock pay. Caps from the in-process gate (config-bound at MCP start).
/// Tool args `per_job` / `total` / `per_job_budget_sats` / `total_budget_sats` are ignored.
fn stub_pay(gate: &Mutex<BudgetGate>, arguments: &Value) -> Result<Value, String> {
    let amount = arguments
        .get("amount_sats")
        .and_then(Value::as_u64)
        .ok_or_else(|| "stub_pay requires amount_sats (integer)".to_owned())?;

    let mut guard = gate
        .lock()
        .map_err(|_| "budget gate lock poisoned".to_owned())?;

    // Capture attempted overrides for the response audit trail (values are NOT applied).
    let ignored_cap_overrides = json!({
        "per_job": arguments.get("per_job"),
        "total": arguments.get("total"),
        "per_job_budget_sats": arguments.get("per_job_budget_sats"),
        "total_budget_sats": arguments.get("total_budget_sats"),
    });

    let mut effect_fired = false;
    match guard.authorize_then(amount, || {
        effect_fired = true;
        "stub_ok"
    }) {
        Ok(stub) => {
            debug_assert!(effect_fired);
            let body = json!({
                "ok": true,
                "stub": stub,
                "amount_sats": amount,
                "spent_total_sats": guard.spent(),
                "remaining_sats": guard.remaining(),
                "per_job_cap_sats": guard.per_job_cap(),
                "total_cap_sats": guard.total_cap(),
                "ignored_cap_overrides": ignored_cap_overrides,
                "piece6": "not_called",
            });
            Ok(tool_ok(body))
        }
        Err(refuse) => {
            debug_assert!(!effect_fired);
            Err(refuse.to_string())
        }
    }
}

fn tool_ok(body: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": body.to_string() }],
        "structuredContent": body,
        "isError": false
    })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i32, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn tool_error(id: Value, message: String) -> Value {
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true
        }),
    )
}

fn read_mcp_request(input: &mut dyn BufRead) -> Result<Option<McpRequest>, String> {
    let mut first = String::new();
    let bytes = input
        .read_line(&mut first)
        .map_err(|error| format!("failed to read MCP request: {error}"))?;
    if bytes == 0 {
        return Ok(None);
    }
    if first.trim().is_empty() {
        return read_mcp_request(input);
    }
    if !first.to_ascii_lowercase().starts_with("content-length:") {
        return serde_json::from_str(first.trim_end())
            .map(Some)
            .map_err(|error| format!("invalid MCP JSON line: {error}"));
    }
    let length = first
        .split_once(':')
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .ok_or_else(|| "invalid MCP Content-Length header".to_string())?;
    loop {
        let mut header = String::new();
        let bytes = input
            .read_line(&mut header)
            .map_err(|error| format!("failed to read MCP header: {error}"))?;
        if bytes == 0 {
            return Err("MCP stream ended inside headers".into());
        }
        if header == "\r\n" || header == "\n" {
            break;
        }
    }
    let mut body = vec![0; length];
    std::io::Read::read_exact(input, &mut body)
        .map_err(|error| format!("failed to read MCP body: {error}"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| format!("invalid MCP JSON body: {error}"))
}

fn write_mcp_response(out: &mut dyn Write, value: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *out, value)
        .map_err(|error| format!("failed to encode MCP: {error}"))?;
    out.write_all(b"\n")
        .map_err(|error| format!("failed to write MCP newline: {error}"))?;
    out.flush()
        .map_err(|error| format!("failed to flush MCP response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-mcp-{label}-{}-{id}",
            std::process::id()
        ))
    }

    fn state_at(root: &std::path::Path) -> McpState {
        let home = home::bootstrap(root).expect("bootstrap");
        let gate = Mutex::new(BudgetGate::from_home(&home).expect("gate"));
        McpState { home, gate }
    }

    #[test]
    fn response_uses_newline_delimited_json_rpc() {
        let mut output = Vec::new();
        write_mcp_response(
            &mut output,
            &json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } }),
        )
        .expect("write");
        let response = String::from_utf8(output).expect("utf8");
        assert!(!response.starts_with("Content-Length:"));
        assert!(response.ends_with('\n'));
        let decoded: Value = serde_json::from_str(response.trim_end()).expect("json");
        assert_eq!(decoded["result"]["ok"], true);
    }

    #[test]
    fn read_accepts_newline_and_content_length() {
        let newline = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
        let request = read_mcp_request(&mut Cursor::new(&newline[..]))
            .expect("read newline")
            .expect("present");
        assert_eq!(request.method, "ping");

        let body = br#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
        let framed = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = framed.into_bytes();
        bytes.extend_from_slice(body);
        let request = read_mcp_request(&mut Cursor::new(bytes))
            .expect("read framed")
            .expect("present");
        assert_eq!(request.method, "ping");
        assert_eq!(request.id, Some(json!(2)));
    }

    #[test]
    fn tools_list_exposes_job_lifecycle_and_pay_tools() {
        let tools = tools();
        let names: Vec<&str> = tools
            .as_array()
            .expect("array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names,
            vec![
                "setup_wallet",
                "post_job",
                "get_job",
                "accept_claim",
                "stub_pay",
                "authorize_pay"
            ]
        );
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn setup_wallet_funds_testnut_and_never_emits_secret_key() {
        let root = temp_home("setup-fund");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let result = setup_wallet_fund(&state.home).expect("fund");
        let rendered = result.to_string();
        assert!(!rendered.contains(&secret), "secret leaked in setup_wallet");
        assert_eq!(result["isError"], false);
        assert_eq!(
            result["structuredContent"]["mint_url"],
            home::DEFAULT_MINT_URL
        );
        let balance = result["structuredContent"]["balance_sats"]
            .as_u64()
            .expect("balance");
        assert!(balance > 0, "balance={balance}");
        assert!(
            result["structuredContent"]["invoice"].is_string()
                || result["structuredContent"]["already_funded"] == true
        );
    }

    #[test]
    fn stub_pay_within_caps_passes() {
        let root = temp_home("stub-ok");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let result = stub_pay(&state.gate, &json!({ "amount_sats": 1 })).expect("allow");
        let rendered = result.to_string();
        assert!(!rendered.contains(&secret));
        assert_eq!(result["structuredContent"]["ok"], true);
        assert_eq!(result["structuredContent"]["piece6"], "not_called");
    }

    #[test]
    fn stub_pay_exceed_per_job_refused_as_error() {
        let root = temp_home("stub-job");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let per_job = state.home.config.per_job_budget_sats;
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let err = stub_pay(&state.gate, &json!({ "amount_sats": per_job + 1 })).expect_err("refuse");
        assert!(err.contains("per-job"));
        assert!(!err.contains(&secret));
        assert_eq!(state.gate.lock().expect("lock").spent(), 0);
    }

    #[test]
    fn stub_pay_exceed_remaining_total_refused_as_error() {
        let root = temp_home("stub-total");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let per_job = state.home.config.per_job_budget_sats;
        let total = state.home.config.total_budget_sats;
        // Drain close to total with per-job-sized chunks.
        let mut remaining = total;
        while remaining > 0 {
            let chunk = per_job.min(remaining);
            if chunk == 0 {
                break;
            }
            // If next full per_job would exceed, use remaining if ≤ per_job.
            stub_pay(&state.gate, &json!({ "amount_sats": chunk })).expect("drain");
            remaining = remaining.saturating_sub(chunk);
        }
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let err = stub_pay(&state.gate, &json!({ "amount_sats": 1 })).expect_err("total");
        assert!(err.contains("remaining total"));
        assert!(!err.contains(&secret));
    }

    #[test]
    fn stub_pay_ignores_cap_mutation_via_tool_args() {
        let root = temp_home("stub-mutate");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let per_job = state.home.config.per_job_budget_sats;
        // Attempt to raise caps via tool args — must still refuse per_job+1.
        let err = stub_pay(
            &state.gate,
            &json!({
                "amount_sats": per_job + 1,
                "per_job": 1_000_000,
                "total": 1_000_000,
                "per_job_budget_sats": 1_000_000,
                "total_budget_sats": 1_000_000,
            }),
        )
        .expect_err("mutation ignored");
        assert!(err.contains("per-job"));
        assert_eq!(
            state.gate.lock().expect("lock").per_job_cap(),
            per_job
        );
    }

    #[test]
    fn stub_pay_boundary_exact_per_job_passes() {
        let root = temp_home("stub-boundary");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let per_job = state.home.config.per_job_budget_sats;
        stub_pay(&state.gate, &json!({ "amount_sats": per_job })).expect("exact");
        stub_pay(&state.gate, &json!({ "amount_sats": per_job + 1 })).expect_err("plus one");
    }

    #[test]
    fn tools_call_error_path_never_echoes_secret() {
        let root = temp_home("never-echo-err");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let response = dispatch(
            &state,
            &McpRequest {
                jsonrpc: Some("2.0".into()),
                id: Some(json!(1)),
                method: "tools/call".into(),
                params: json!({
                    "name": "stub_pay",
                    "arguments": { "amount_sats": state.home.config.per_job_budget_sats + 1 }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(!rendered.contains(&secret));
        assert_eq!(response["result"]["isError"], true);
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn authorize_pay_ext_refused_never_echoes_secret() {
        let root = temp_home("authorize-ext");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let seller = home::public_key_hex(&state.home).expect("pubkey");
        let response = dispatch(
            &state,
            &McpRequest {
                jsonrpc: Some("2.0".into()),
                id: Some(json!(9)),
                method: "tools/call".into(),
                params: json!({
                    "name": "authorize_pay",
                    "arguments": {
                        "job_id": "job",
                        "result_id": "result",
                        "delivery_integrity_hash": "aa".repeat(20),
                        "job_hash": "bb".repeat(32),
                        "seller_pubkey": seller,
                        "amount_sats": 1,
                        "repo": "ext::sh -c evil",
                        "branch": "main",
                        "commit_oid": "aa".repeat(20),
                    }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(!rendered.contains(&secret), "secret leaked on authorize_pay error");
        assert_eq!(response["result"]["isError"], true);
        let message = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            message.contains("ext") || message.contains("refused") || message.contains("transport"),
            "message={message}"
        );
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn post_job_error_path_never_echoes_secret() {
        let root = temp_home("post-echo");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let response = dispatch(
            &state,
            &McpRequest {
                jsonrpc: Some("2.0".into()),
                id: Some(json!(41)),
                method: "tools/call".into(),
                params: json!({
                    "name": "post_job",
                    "arguments": {
                        "task": "gate-e",
                        "output": "text/plain",
                        "amount_sats": 1
                    }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(!rendered.contains(&secret), "secret leaked on post_job error");
        assert_eq!(response["result"]["isError"], true);
        let message = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            message.contains("seller_pubkey") || message.contains("untargeted"),
            "message={message}"
        );
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn accept_claim_error_path_never_echoes_secret() {
        let root = temp_home("accept-echo");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let response = dispatch(
            &state,
            &McpRequest {
                jsonrpc: Some("2.0".into()),
                id: Some(json!(42)),
                method: "tools/call".into(),
                params: json!({
                    "name": "accept_claim",
                    "arguments": {
                        "job_id": "aa".repeat(32),
                        "claim_id": "bb".repeat(32)
                    }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(!rendered.contains(&secret), "secret leaked on accept_claim error");
        assert_eq!(response["result"]["isError"], true);
        let message = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            !message.is_empty(),
            "expected refuse message, got empty"
        );
    }

    #[test]
    fn stub_pay_spent_survives_mcp_state_reload() {
        let root = temp_home("spent-reload");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        stub_pay(&state.gate, &json!({ "amount_sats": 7 })).expect("pay");
        assert_eq!(state.gate.lock().expect("lock").spent(), 7);

        // Simulate MCP process restart: new gate from same home.
        let reloaded = state_at(&root);
        assert_eq!(reloaded.gate.lock().expect("lock").spent(), 7);
        assert_eq!(
            reloaded.gate.lock().expect("lock").remaining(),
            reloaded.home.config.total_budget_sats - 7
        );
    }
}
