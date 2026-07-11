pub mod acp;
pub mod mock;

use std::error::Error;
use std::fmt::{self, Display};

pub use crate::event::RuntimeId;

pub use acp::{
    Artifact, Caps, ContentBlock, ExtMethod, Initialize, InitializeResult, McpServer,
    PermissionOutcome, PermissionRequest, PromptTurn, SessionConfig, SessionId, SessionUpdate,
    StopReason, UpdateStream,
};
pub use mock::{MockDriver, ScriptedSession};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Readiness {
    pub runtime_id: RuntimeId,
    pub protocol_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverError {
    NotReady,
    SessionNotFound(SessionId),
    SessionCancelled(SessionId),
    ScriptExhausted,
    Other(String),
}

impl Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotReady => write!(f, "driver is not ready"),
            Self::SessionNotFound(session_id) => write!(f, "session not found: {session_id}"),
            Self::SessionCancelled(session_id) => write!(f, "session is cancelled: {session_id}"),
            Self::ScriptExhausted => write!(f, "mock driver script exhausted"),
            Self::Other(message) => write!(f, "{message}"),
        }
    }
}

impl Error for DriverError {}

#[allow(async_fn_in_trait)]
pub trait Driver {
    fn id(&self) -> RuntimeId;
    async fn ready(&mut self) -> Result<Readiness, DriverError>;
    async fn start_session(&mut self, cfg: SessionConfig) -> Result<SessionId, DriverError>;
    async fn prompt(
        &mut self,
        session_id: &SessionId,
        turn: PromptTurn,
    ) -> Result<UpdateStream, DriverError>;
    async fn on_permission(&mut self, req: PermissionRequest) -> PermissionOutcome;
    async fn artifacts(&self, session_id: &SessionId) -> Result<Vec<Artifact>, DriverError>;
    async fn cancel(&mut self, session_id: &SessionId) -> Result<(), DriverError>;
    async fn shutdown(&mut self) -> Result<(), DriverError>;
}
