use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::driver::acp::{PROTOCOL_VERSION, UpdateStream};
use crate::driver::{
    Artifact, Caps, Driver, DriverError, Initialize, PermissionOutcome, PermissionRequest,
    PromptTurn, Readiness, RuntimeId, SessionConfig, SessionId, SessionUpdate, StopReason,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCommand {
    program: String,
    args: Vec<String>,
}

impl AgentCommand {
    pub fn new(program: String, args: Vec<String>) -> Self {
        Self { program, args }
    }

    fn runtime_id(&self) -> RuntimeId {
        RuntimeId(self.program.clone())
    }
}

pub struct AcpDriver {
    command: AgentCommand,
    permission_policy: PermissionOutcome,
    idle_timeout: Duration,
    child: Option<Child>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    responses: Option<mpsc::Receiver<RpcResponse>>,
    updates: Option<mpsc::Receiver<SessionUpdate>>,
    pending_permission_ids: Arc<Mutex<VecDeque<Value>>>,
    next_request_id: AtomicU64,
}

impl AcpDriver {
    pub fn new(
        command: AgentCommand,
        permission_policy: PermissionOutcome,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            command,
            permission_policy,
            idle_timeout,
            child: None,
            stdin: None,
            responses: None,
            updates: None,
            pending_permission_ids: Arc::new(Mutex::new(VecDeque::new())),
            next_request_id: AtomicU64::new(1),
        }
    }

    fn spawn(&mut self) -> Result<(), DriverError> {
        if self.child.is_some() {
            return Ok(());
        }

        let mut command = Command::new(&self.command.program);
        command
            .args(&self.command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command
            .spawn()
            .map_err(|error| DriverError::Other(format!("failed to spawn ACP agent: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| DriverError::Other("ACP child stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DriverError::Other("ACP child stdout unavailable".into()))?;

        let stdin = Arc::new(Mutex::new(stdin));
        let (response_tx, response_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();
        let pending_permission_ids = self.pending_permission_ids.clone();

        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                route_wire_message(&value, &response_tx, &update_tx, &pending_permission_ids);
            }
        });

        self.stdin = Some(stdin);
        self.responses = Some(response_rx);
        self.updates = Some(update_rx);
        self.child = Some(child);
        Ok(())
    }

    fn send_request(&self, method: &str, params: Value) -> Result<u64, DriverError> {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_wire(&request)?;
        Ok(id)
    }

    fn send_response(&self, id: Value, result: Value) -> Result<(), DriverError> {
        self.write_wire(&response_value(id, result))
    }

    fn write_wire(&self, value: &Value) -> Result<(), DriverError> {
        let stdin = self
            .stdin
            .as_ref()
            .ok_or_else(|| DriverError::Other("ACP child stdin unavailable".into()))?;
        let mut stdin = stdin
            .lock()
            .map_err(|_| DriverError::Other("ACP child stdin lock poisoned".into()))?;
        serde_json::to_writer(&mut *stdin, value).map_err(|error| {
            DriverError::Other(format!("failed to encode ACP JSON-RPC: {error}"))
        })?;
        stdin
            .write_all(b"\n")
            .and_then(|_| stdin.flush())
            .map_err(|error| DriverError::Other(format!("failed to write ACP JSON-RPC: {error}")))
    }

    fn wait_response(&self, id: u64) -> Result<Value, DriverError> {
        let responses = self
            .responses
            .as_ref()
            .ok_or_else(|| DriverError::Other("ACP response channel unavailable".into()))?;
        loop {
            let response = responses.recv_timeout(self.idle_timeout).map_err(|_| {
                DriverError::Other(format!("ACP request {id} timed out waiting for response"))
            })?;
            if response.id != json!(id) {
                continue;
            }
            if let Some(error) = response.error {
                return Err(DriverError::Other(format!(
                    "ACP request {id} failed: {error}"
                )));
            }
            return Ok(response.result.unwrap_or(Value::Null));
        }
    }
}

impl Driver for AcpDriver {
    fn id(&self) -> RuntimeId {
        self.command.runtime_id()
    }

    async fn ready(&mut self) -> Result<Readiness, DriverError> {
        self.spawn()?;
        let initialize = Initialize::new(Caps::default());
        let id = self.send_request(
            "initialize",
            serde_json::to_value(initialize).map_err(|error| {
                DriverError::Other(format!("failed to encode initialize params: {error}"))
            })?,
        )?;
        let result = self.wait_response(id)?;
        let protocol_version = result
            .get("protocol_version")
            .or_else(|| result.get("protocolVersion"))
            .and_then(Value::as_u64)
            .map(|value| value as u32)
            .unwrap_or(PROTOCOL_VERSION);
        if protocol_version != PROTOCOL_VERSION {
            return Err(DriverError::Other(format!(
                "unsupported ACP protocol version {protocol_version}"
            )));
        }
        Ok(Readiness {
            runtime_id: self.command.runtime_id(),
            protocol_version,
        })
    }

    async fn start_session(&mut self, cfg: SessionConfig) -> Result<SessionId, DriverError> {
        let id = self.send_request(
            "session/new",
            serde_json::to_value(cfg).map_err(|error| {
                DriverError::Other(format!("failed to encode session params: {error}"))
            })?,
        )?;
        let result = self.wait_response(id)?;
        session_id_from_result(&result)
    }

    async fn prompt(
        &mut self,
        session_id: &SessionId,
        turn: PromptTurn,
    ) -> Result<UpdateStream, DriverError> {
        let id = self.send_request("session/prompt", prompt_params(session_id, turn))?;
        let _ = self.wait_response(id)?;
        let receiver = self
            .updates
            .take()
            .ok_or_else(|| DriverError::Other("ACP update channel already consumed".into()))?;
        Ok(UpdateStream::live(receiver, self.idle_timeout))
    }

    async fn on_permission(&mut self, _req: PermissionRequest) -> PermissionOutcome {
        let id = self
            .pending_permission_ids
            .lock()
            .ok()
            .and_then(|mut ids| ids.pop_front());
        if let Some(id) = id {
            let _ = self.send_response(
                id,
                json!({
                    "outcome": self.permission_policy,
                }),
            );
        }
        self.permission_policy.clone()
    }

    async fn artifacts(&self, _session_id: &SessionId) -> Result<Vec<Artifact>, DriverError> {
        Ok(Vec::new())
    }

    async fn cancel(&mut self, session_id: &SessionId) -> Result<(), DriverError> {
        if self.stdin.is_some() {
            let id = self.send_request(
                "session/cancel",
                json!({
                    "session_id": session_id,
                    "sessionId": session_id,
                }),
            )?;
            let _ = self.wait_response(id);
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), DriverError> {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(format!("-{}", child.id()))
                    .status();
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }
}

#[derive(Debug)]
struct RpcResponse {
    id: Value,
    result: Option<Value>,
    error: Option<Value>,
}

fn route_wire_message(
    value: &Value,
    response_tx: &mpsc::Sender<RpcResponse>,
    update_tx: &mpsc::Sender<SessionUpdate>,
    pending_permission_ids: &Arc<Mutex<VecDeque<Value>>>,
) {
    if value.get("method").is_none() {
        if let Some(id) = value.get("id").cloned() {
            let _ = response_tx.send(RpcResponse {
                id,
                result: value.get("result").cloned(),
                error: value.get("error").cloned(),
            });
        }
        return;
    }

    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    if is_permission_method(method) {
        if let Some(id) = value.get("id").cloned()
            && let Ok(mut ids) = pending_permission_ids.lock()
        {
            ids.push_back(id);
        }
        if let Some(request) = permission_request_from_params(&params) {
            let _ = update_tx.send(SessionUpdate::PermissionRequest(request));
        }
        return;
    }

    let update = session_update_from_method(method, params);
    let _ = update_tx.send(update);
}

fn response_value(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn prompt_params(session_id: &str, turn: PromptTurn) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": turn.input,
    })
}

fn session_id_from_result(result: &Value) -> Result<SessionId, DriverError> {
    result
        .get("session_id")
        .or_else(|| result.get("sessionId"))
        .or_else(|| result.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| result.as_str().map(str::to_owned))
        .ok_or_else(|| {
            DriverError::Other(format!("ACP session result missing session id: {result}"))
        })
}

fn is_permission_method(method: &str) -> bool {
    method.contains("permission")
}

fn permission_request_from_params(params: &Value) -> Option<PermissionRequest> {
    serde_json::from_value(params.clone())
        .ok()
        .or_else(|| serde_json::from_value(params.get("request")?.clone()).ok())
}

fn session_update_from_method(method: &str, params: Value) -> SessionUpdate {
    if let Some(update) = session_update_from_params(&params) {
        return update;
    }

    match method {
        "session/update" | "session.update" | "session_update" => {
            SessionUpdate::Ext(crate::driver::ExtMethod {
                method: method.into(),
                params,
            })
        }
        method if method.contains("turn") && method.contains("end") => {
            SessionUpdate::TurnEnded(stop_reason_from_params(&params))
        }
        _ => SessionUpdate::Ext(crate::driver::ExtMethod {
            method: method.into(),
            params,
        }),
    }
}

fn session_update_from_params(params: &Value) -> Option<SessionUpdate> {
    serde_json::from_value(params.clone())
        .ok()
        .or_else(|| serde_json::from_value(params.get("update")?.clone()).ok())
}

fn stop_reason_from_params(params: &Value) -> StopReason {
    params
        .get("reason")
        .or_else(|| params.get("stop_reason"))
        .or_else(|| params.get("stopReason"))
        .and_then(Value::as_str)
        .and_then(|reason| match reason {
            "completed" => Some(StopReason::Completed),
            "cancelled" | "canceled" => Some(StopReason::Cancelled),
            "failed" => Some(StopReason::Failed),
            _ => None,
        })
        .unwrap_or(StopReason::Completed)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::driver::{ContentBlock, ExtMethod, PermissionOutcome};

    #[test]
    fn request_side_wire_uses_real_acp_camel_case() {
        let initialize =
            serde_json::to_value(Initialize::new(Caps::default())).expect("serialize initialize");
        assert_eq!(
            initialize,
            json!({
                "protocolVersion": 2,
                "clientCapabilities": {
                    "methods": []
                }
            })
        );

        let session = serde_json::to_value(SessionConfig {
            cwd: "/tmp/mobee".into(),
            mcp_servers: Vec::new(),
            env: Vec::new(),
        })
        .expect("serialize session config");
        assert_eq!(
            session,
            json!({
                "cwd": "/tmp/mobee",
                "mcpServers": [],
                "env": []
            })
        );

        let turn = PromptTurn {
            input: vec![ContentBlock::Text { text: "hi".into() }],
        };
        assert_eq!(
            prompt_params("session-1", turn),
            json!({
                "sessionId": "session-1",
                "prompt": [
                    {
                        "type": "text",
                        "data": {
                            "text": "hi"
                        }
                    }
                ]
            })
        );
    }

    #[test]
    fn fixture_lines_translate_to_updates() {
        let (response_tx, _response_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();
        let ids = Arc::new(Mutex::new(VecDeque::new()));

        route_wire_message(
            &json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "type": "agent_message",
                    "data": [{"type": "text", "data": {"text": "hello"}}]
                }
            }),
            &response_tx,
            &update_tx,
            &ids,
        );
        route_wire_message(
            &json!({
                "jsonrpc": "2.0",
                "method": "session/turn/end",
                "params": {"reason": "completed"}
            }),
            &response_tx,
            &update_tx,
            &ids,
        );

        assert_eq!(
            update_rx.recv().expect("first update"),
            SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                text: "hello".into()
            }])
        );
        assert_eq!(
            update_rx.recv().expect("terminal"),
            SessionUpdate::TurnEnded(StopReason::Completed)
        );
    }

    #[test]
    fn unknown_methods_surface_as_ext() {
        let (response_tx, _response_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();
        let ids = Arc::new(Mutex::new(VecDeque::new()));
        let params = json!({"x": 1});

        route_wire_message(
            &json!({
                "jsonrpc": "2.0",
                "method": "cursor/ask_question",
                "params": params
            }),
            &response_tx,
            &update_tx,
            &ids,
        );

        assert_eq!(
            update_rx.recv().expect("ext update"),
            SessionUpdate::Ext(ExtMethod {
                method: "cursor/ask_question".into(),
                params,
            })
        );
    }

    #[test]
    fn permission_request_parks_id_fifo() {
        let (response_tx, _response_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();
        let ids = Arc::new(Mutex::new(VecDeque::new()));

        route_wire_message(
            &json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "session/permission/request",
                "params": {
                    "tool": "shell",
                    "detail": {"cmd": "true"}
                }
            }),
            &response_tx,
            &update_tx,
            &ids,
        );

        assert_eq!(
            update_rx.recv().expect("permission update"),
            SessionUpdate::PermissionRequest(PermissionRequest {
                tool: "shell".into(),
                detail: json!({"cmd": "true"}),
            })
        );
        assert_eq!(ids.lock().expect("ids").pop_front(), Some(json!(42)));
        assert_eq!(
            response_value(
                json!(42),
                json!({
                    "outcome": PermissionOutcome::Allow,
                })
            ),
            json!({
                "jsonrpc": "2.0",
                "id": 42,
                "result": {
                    "outcome": "allow"
                }
            })
        );

        let mut driver = AcpDriver::new(
            AgentCommand::new("fake".into(), Vec::new()),
            PermissionOutcome::Allow,
            Duration::from_secs(1),
        );
        driver.pending_permission_ids = ids;
        assert_eq!(
            futures_free_on_permission(
                &mut driver,
                PermissionRequest {
                    tool: "shell".into(),
                    detail: json!({"cmd": "true"}),
                }
            ),
            PermissionOutcome::Allow
        );
    }

    fn futures_free_on_permission(
        driver: &mut AcpDriver,
        request: PermissionRequest,
    ) -> PermissionOutcome {
        let future = driver.on_permission(request);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(outcome) => outcome,
            std::task::Poll::Pending => panic!("permission future should not pend"),
        }
    }
}
