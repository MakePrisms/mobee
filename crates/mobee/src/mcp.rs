//! Thin MCP stdio surface: `mobee mcp`.
//!
//! Writes are newline-delimited JSON-RPC. Reads accept newline JSON *or* legacy
//! `Content-Length` framing (spike scar — Claude Code hung on LSP-only writes).

use std::io::{BufRead, Write};

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

/// Run the MCP server on the provided stdio handles until stdin EOF.
pub fn run(out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let home = match bootstrap_home() {
        Ok(home) => home,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let _ = writeln!(
        err,
        "mobee mcp ready (home={}, key_created={}, mint={}, relay={})",
        home.root.display(),
        home.key_created,
        home.config.mint_url,
        home.config.relay_url
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
        let response = dispatch(&home, &request);
        if let Err(error) = write_mcp_response(out, &response) {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    }
}

fn bootstrap_home() -> Result<MobeeHome, String> {
    let root = home::default_home_dir().map_err(|error| error.to_string())?;
    home::bootstrap(root).map_err(|error| error.to_string())
}

fn dispatch(home: &MobeeHome, request: &McpRequest) -> Value {
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
        "tools/call" => match call_tool(home, &request.params) {
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
            "description": "Bootstrap ~/.mobee (config + autogen key + wallet dir). Returns status for the next funding step; the secret key never appears in the response. Lightning mint-quote invoice lands in the next slice.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }
    ])
}

fn call_tool(home: &MobeeHome, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call missing name".to_owned())?;
    match name {
        "setup_wallet" => setup_wallet_status(home),
        other => Err(format!("unknown tool: {other}")),
    }
}

fn setup_wallet_status(home: &MobeeHome) -> Result<Value, String> {
    // Re-bootstrap so a long-lived MCP process still converges if files were removed.
    let home = home::bootstrap(&home.root).map_err(|error| error.to_string())?;
    let body = json!({
        "home": home.root.display().to_string(),
        "key_created": home.key_created,
        "key_present": home::key_file_present(&home),
        "wallet_dir": home.wallet_dir.display().to_string(),
        "relay_url": home.config.relay_url,
        "mint_url": home.config.mint_url,
        "per_job_budget_sats": home.config.per_job_budget_sats,
        "total_budget_sats": home.config.total_budget_sats,
        "invoice": Value::Null,
        "next": "Lightning fund invoice (cdk mint quote) lands next — secret key never returned"
    });
    Ok(json!({
        "content": [{ "type": "text", "text": body.to_string() }],
        "structuredContent": body,
        "isError": false
    }))
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
    fn tools_list_exposes_setup_wallet() {
        let tools = tools();
        let names: Vec<&str> = tools
            .as_array()
            .expect("array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["setup_wallet"]);
    }

    #[test]
    fn setup_wallet_status_never_emits_secret_key() {
        let root = temp_home("setup");
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let secret = home::read_secret_key_hex(&home).expect("secret");
        let result = setup_wallet_status(&home).expect("status");
        let rendered = result.to_string();
        assert!(!rendered.contains(&secret));
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["mint_url"], home::DEFAULT_MINT_URL);
        assert!(result["structuredContent"]["invoice"].is_null());
    }
}
