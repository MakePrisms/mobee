use std::path::PathBuf;
#[cfg(feature = "acp")]
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[cfg(feature = "acp")]
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 2;
pub type SessionId = String;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Initialize {
    pub protocol_version: u32,
    pub client_capabilities: Caps,
}

impl Initialize {
    pub fn new(client_capabilities: Caps) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub agent_capabilities: Caps,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Caps {
    pub methods: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfig {
    pub cwd: PathBuf,
    pub mcp_servers: Vec<McpServer>,
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct McpServer {
    pub name: String,
    pub command: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTurn {
    pub input: Vec<ContentBlock>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "artifact")]
    Artifact(Artifact),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum SessionUpdate {
    #[serde(rename = "agent_message")]
    AgentMessage(Vec<ContentBlock>),
    #[serde(rename = "agent_message_chunk")]
    AgentMessageChunk(ContentBlock),
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult { id: String, output: Value },
    #[serde(rename = "plan")]
    Plan { entries: Vec<String> },
    #[serde(rename = "permission_request")]
    PermissionRequest(PermissionRequest),
    #[serde(rename = "ext")]
    Ext(ExtMethod),
    #[serde(rename = "turn_ended")]
    TurnEnded(StopReason),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PermissionRequest {
    pub tool: String,
    pub detail: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    Allow,
    AllowAlways,
    Deny,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Artifact {
    pub uri_or_path: String,
    pub mime: Option<String>,
    pub bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExtMethod {
    pub method: String,
    pub params: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug)]
pub struct UpdateStream {
    inner: UpdateStreamInner,
}

#[derive(Debug)]
enum UpdateStreamInner {
    Scripted {
        updates: Vec<SessionUpdate>,
        next_index: usize,
        cancelled: Arc<AtomicBool>,
        emitted_cancelled: bool,
    },
    #[cfg(feature = "acp")]
    Live {
        receiver: mpsc::Receiver<SessionUpdate>,
        idle_timeout: Duration,
    },
}

impl UpdateStream {
    pub(crate) fn new(updates: Vec<SessionUpdate>, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            inner: UpdateStreamInner::Scripted {
                updates,
                next_index: 0,
                cancelled,
                emitted_cancelled: false,
            },
        }
    }

    #[cfg(feature = "acp")]
    pub(crate) fn live(receiver: mpsc::Receiver<SessionUpdate>, idle_timeout: Duration) -> Self {
        Self {
            inner: UpdateStreamInner::Live {
                receiver,
                idle_timeout,
            },
        }
    }

    pub async fn next(&mut self) -> Option<SessionUpdate> {
        match &mut self.inner {
            UpdateStreamInner::Scripted {
                updates,
                next_index,
                cancelled,
                emitted_cancelled,
            } => {
                if cancelled.load(Ordering::SeqCst) {
                    if *emitted_cancelled {
                        None
                    } else {
                        *emitted_cancelled = true;
                        Some(SessionUpdate::TurnEnded(StopReason::Cancelled))
                    }
                } else if *next_index >= updates.len() {
                    None
                } else {
                    let update = updates[*next_index].clone();
                    *next_index += 1;
                    Some(update)
                }
            }
            #[cfg(feature = "acp")]
            UpdateStreamInner::Live {
                receiver,
                idle_timeout,
            } => receiver.recv_timeout(*idle_timeout).ok(),
        }
    }
}

use super::{UsageCost, UsageMetadata, UsageTransport};

/// Extract execution usage from an ACP `session/prompt` JSON-RPC result.
///
/// The prompt result is the only ACP-native usage surface the driver has: whatever the harness
/// reports under its `usage` object is captured here. **Absent-stays-absent** — a result with no
/// recognizable usage returns `None` and nothing is emitted downstream, so a missing number is
/// never rendered as a fabricated zero. When any field is found, `transport = acp-native`,
/// because the value arrived over the ACP wire.
///
/// Token components are read only from a real `usage` object, never guessed off unrelated root
/// fields.
pub fn parse_acp_usage(result: &Value) -> Option<UsageMetadata> {
    let usage_obj = result
        .get("usage")
        .or_else(|| result.get("_meta").and_then(|m| m.get("usage")))
        .or_else(|| result.get("result").and_then(|r| r.get("usage")));

    let (input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens) =
        match usage_obj {
            Some(u) => (
                first_u64(u, &["inputTokens", "input_tokens", "promptTokens", "prompt_tokens"]),
                first_u64(u, &["outputTokens", "output_tokens", "completionTokens", "completion_tokens"]),
                first_u64(u, &["reasoningTokens", "reasoning_tokens", "reasoning"]),
                first_u64(
                    u,
                    &["cacheReadTokens", "cache_read_tokens", "cacheReadInputTokens", "cache_read_input_tokens", "cache_read"],
                ),
                first_u64(
                    u,
                    &["cacheCreationTokens", "cache_creation_tokens", "cacheCreationInputTokens", "cache_creation_input_tokens", "cacheWriteTokens", "cache_write_tokens", "cache_write"],
                ),
            ),
            None => (None, None, None, None, None),
        };

    let model = first_str(result, &["model"])
        .or_else(|| usage_obj.and_then(|u| first_str(u, &["model"])))
        .or_else(|| result.get("_meta").and_then(|m| first_str(m, &["model"])))
        .or_else(|| model_usage_key(result));

    let cost = first_cost(result, usage_obj);

    let meta = UsageMetadata {
        model,
        input_tokens,
        output_tokens,
        reasoning_tokens,
        cache_read_tokens,
        cache_write_tokens,
        cost,
        transport: None,
    };
    if meta.is_empty() {
        return None;
    }
    Some(UsageMetadata {
        transport: Some(UsageTransport::AcpNative),
        ..meta
    })
}

fn first_str(v: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(s) = v.get(*key).and_then(Value::as_str)
            && !s.is_empty()
        {
            return Some(s.to_owned());
        }
    }
    None
}

fn first_u64(v: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        let Some(raw) = v.get(*key) else { continue };
        if let Some(n) = raw.as_u64() {
            return Some(n);
        }
        if let Some(n) = raw.as_str().and_then(|s| s.trim().parse::<u64>().ok()) {
            return Some(n);
        }
    }
    None
}

/// claude's `json` surface exposes the model as the KEY of `modelUsage` (no top-level string).
fn model_usage_key(result: &Value) -> Option<String> {
    result
        .get("modelUsage")
        .and_then(Value::as_object)
        .and_then(|m| m.keys().next().cloned())
        .filter(|s| !s.is_empty())
}

/// A reported USD cost → the exact string as reported (byte-exact, never float-mangled).
/// Basis defaults to `harness-reported-usd` (incurred API-key billing — the spec's primary
/// mapping for `total_cost_usd`); notional/subscription detection is a named follow-up.
fn first_cost(result: &Value, usage_obj: Option<&Value>) -> Option<UsageCost> {
    const KEYS: &[&str] = &["total_cost_usd", "totalCostUsd", "cost_usd", "costUsd"];
    let sources = [Some(result), usage_obj];
    for src in sources.into_iter().flatten() {
        for key in KEYS {
            if let Some(raw) = src.get(*key)
                && let Some(amount) = number_string(raw)
            {
                return Some(UsageCost {
                    amount,
                    basis: "harness-reported-usd".to_owned(),
                });
            }
        }
    }
    None
}

fn number_string(v: &Value) -> Option<String> {
    if v.is_number() {
        return Some(v.to_string());
    }
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_usage_stays_absent_never_fabricated() {
        // A bare stop-reason result (today's real claude-agent-acp shape) → no usage at all.
        assert_eq!(parse_acp_usage(&json!({"stopReason": "end_turn"})), None);
        assert_eq!(parse_acp_usage(&json!({})), None);
        // A usage object with no recognizable fields is still nothing.
        assert_eq!(parse_acp_usage(&json!({"usage": {"unrelated": 5}})), None);
    }

    #[test]
    fn acp_native_usage_is_captured_from_the_prompt_result() {
        // Rich claude-shaped ACP result: model + non-cached input/output + cache siblings + USD cost.
        let usage = parse_acp_usage(&json!({
            "stopReason": "end_turn",
            "model": "claude-opus-4-8",
            "total_cost_usd": 0.0123,
            "usage": {
                "inputTokens": 100,
                "outputTokens": 40,
                "cacheReadInputTokens": 4096,
                "cacheCreationInputTokens": 512
            }
        }))
        .expect("usage present");

        assert_eq!(usage.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(40));
        // reasoning absent = unknown, NOT zero.
        assert_eq!(usage.reasoning_tokens, None);
        assert_eq!(usage.cache_read_tokens, Some(4096));
        assert_eq!(usage.cache_write_tokens, Some(512));
        // total = input + output (+ reasoning if present); cache siblings NEVER folded in.
        assert_eq!(usage.total_tokens(), Some(140));
        assert_eq!(usage.transport, Some(UsageTransport::AcpNative));
        let cost = usage.cost.expect("cost present");
        assert_eq!(cost.amount, "0.0123");
        assert_eq!(cost.basis, "harness-reported-usd");
    }

    #[test]
    fn reasoning_is_summed_into_total_when_present() {
        let usage = parse_acp_usage(&json!({
            "usage": {"input_tokens": 10, "output_tokens": 5, "reasoning_tokens": 3}
        }))
        .expect("usage present");
        assert_eq!(usage.reasoning_tokens, Some(3));
        assert_eq!(usage.total_tokens(), Some(18));
    }

    #[test]
    fn partial_capture_never_reports_a_total() {
        // Output known, input unknown → no total (a partial must not masquerade as complete).
        let usage = parse_acp_usage(&json!({"usage": {"outputTokens": 40}})).expect("some");
        assert_eq!(usage.output_tokens, Some(40));
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.total_tokens(), None);
    }

    #[test]
    fn model_read_from_model_usage_key_when_no_top_level_string() {
        let usage = parse_acp_usage(&json!({
            "modelUsage": {"claude-sonnet-4-6": {"inputTokens": 1}},
            "usage": {"inputTokens": 1, "outputTokens": 1}
        }))
        .expect("some");
        assert_eq!(usage.model.as_deref(), Some("claude-sonnet-4-6"));
    }
}
