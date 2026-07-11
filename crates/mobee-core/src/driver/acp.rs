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
