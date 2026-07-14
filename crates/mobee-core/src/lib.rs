pub mod driver;
pub mod engine;
pub mod event;
pub mod format;
pub mod gateway;
pub mod log;
#[cfg(feature = "wallet")]
pub mod payment;
#[cfg(feature = "wallet")]
pub mod payment_edge;
pub mod payment_send;
pub mod receipt;
#[cfg(feature = "wallet")]
pub mod wallet;

pub use event::{Envelope, Event};
pub use log::{EventLog, LogError, ReadError, Replay};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
