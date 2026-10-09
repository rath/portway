//! Portway application components. For embedding, depend on `portway-core`.
pub use portway_core::{
    ack, body, clock, dict, flights, forwarder, pool, relay, router, server, usage,
};

/// The version this binary was built from.
///
/// `build.rs` stamps `<major>.<minor>.<commit-count>+<short-hash>` here, so a
/// deployed binary can be told apart from the one it replaced. `--version` and
/// the console's `/status` payload both read this rather than
/// `CARGO_PKG_VERSION`, which never moves between commits.
pub const VERSION: &str = env!("PORTWAY_VERSION");
pub mod aliases;
#[cfg(any(feature = "tui", feature = "web"))]
pub mod board;
pub mod cli;
pub mod config;
pub mod control;
pub mod daemon;
pub mod live;
pub mod logfmt;
#[cfg(feature = "tui")]
pub mod remote;
pub mod report;
pub mod request_log;
pub mod setup;
#[cfg(any(feature = "tui", feature = "web"))]
pub mod spend;
pub mod store;
pub mod telemetry;
#[cfg(feature = "tui")]
pub mod tui;
#[cfg(any(feature = "tui", feature = "web"))]
pub mod watch;
#[cfg(feature = "web")]
pub mod web;
