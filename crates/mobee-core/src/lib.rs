pub mod driver;
pub mod engine;
pub mod event;
pub mod format;
pub mod gateway;
pub mod log;
pub mod receipt;

pub use event::{Envelope, Event};
pub use log::{EventLog, LogError, ReadError, Replay};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
