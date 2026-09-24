//! Embeddable compression-first HTTP forwarding.
//! Callers own the runtime, authentication, configuration and event handling.

pub mod ack;
pub mod body;
pub mod clock;
pub mod config;
pub mod dict;
pub mod forwarder;
pub mod pool;
pub mod relay;
pub mod router;
pub mod server;
pub mod telemetry;
pub mod time;
pub mod usage;

pub use config::{CodingPreference, DictionaryPreference, ForwarderConfig};
pub use forwarder::Forwarder;
pub use router::Router;
pub use telemetry::{Event, EventSink, Telemetry};
pub mod receiver;
pub use receiver::{Receiver, ReceiverConfig};
