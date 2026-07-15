//! Thin MCP stdio surface: `mobee mcp`.
//!
//! Writes are newline-delimited JSON-RPC. Reads accept newline JSON *or* legacy
//! `Content-Length` framing (spike scar — Claude Code hung on LSP-only writes).

use std::io::{BufRead, Write};
use std::sync::Mutex;

use mobee_core::budget::BudgetGate;
use mobee_core::home::{self, MobeeHome};
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
    let gate = Mutex::new(BudgetGate::from_config(&home.config));
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
        "stub_pay" => stub_pay(&state.gate, &arguments),
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
        let gate = Mutex::new(BudgetGate::from_config(&home.config));
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
    fn tools_list_exposes_setup_wallet_and_stub_pay() {
        let tools = tools();
        let names: Vec<&str> = tools
            .as_array()
            .expect("array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["setup_wallet", "stub_pay"]);
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
}
