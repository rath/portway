//! Instance-owned observations; never writes files or terminals.
use crate::{flights::Flights, forwarder::Coding, usage::Usage};
use http::Method;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info = 0,
    Warning = 1,
    Error = 2,
}
impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
        }
    }
    pub fn from_stored(raw: i64) -> Self {
        match raw {
            1 => Self::Warning,
            2 => Self::Error,
            _ => Self::Info,
        }
    }
}
/// One finished request, with the numbers the log line is rendered from.
#[derive(Debug, Clone)]
pub struct RequestRecord {
    /// Local `HH:MM:SS` of the moment the relay ended.
    pub stamp: String,
    /// The route the request went through: a mount's name, a model's name,
    /// or `upstream` in single-upstream mode.
    pub upstream: String,
    /// The model the request named in its JSON body; empty when it named
    /// none, such as a catalog or health request. Prices are keyed by it.
    pub model: String,
    pub method: Method,
    pub path: String,
    pub status: u16,
    /// Handshake phases of a freshly dialed connection; all `None` when the
    /// connection came from the pool.
    pub dns: Option<f64>,
    pub tcp: Option<f64>,
    pub tls: Option<f64>,
    /// Request body as the agent sent it, and as it went upstream.
    pub body_len: u64,
    pub wire_len: u64,
    pub coding: Coding,
    /// First body byte to the last byte ACKed by the peer's kernel.
    pub upload: Option<f64>,
    pub ttfb: f64,
    /// Response body as the agent got it, and as it came off the upstream.
    pub received: u64,
    pub received_wire: u64,
    /// What the hop actually sent the agent once it re-encoded the body; equal
    /// to `received` while no coding was negotiated for the agent leg.
    pub received_agent: u64,
    pub upstream_encoding: String,
    pub agent_encoding: Option<String>,
    /// First response byte to the end of the relay.
    pub download: Option<f64>,
    /// False when the relay ended before the upstream body did: an agent
    /// abort, an upstream error or a read timeout.
    pub complete: bool,
    /// The token counts the upstream reported for this answer, read out of the
    /// body as it was relayed. `None` when it reported none: an engine that
    /// was not asked to (`stream_options.include_usage`), or a stream that was
    /// cut short before its last chunk.
    pub usage: Option<Usage>,
    /// The flight this record ends (see `flights`): a reader showing the
    /// requests in flight drops that entry when this record arrives. `None`
    /// for records rebuilt from the database.
    pub flight: Option<u64>,
}

impl RequestRecord {
    pub fn reused(&self) -> bool {
        self.dns.is_none() && self.tcp.is_none() && self.tls.is_none()
    }

    /// dns + tcp + tls of a fresh dial.
    pub fn handshake(&self) -> Option<f64> {
        if self.reused() {
            return None;
        }
        Some(self.dns.unwrap_or(0.0) + self.tcp.unwrap_or(0.0) + self.tls.unwrap_or(0.0))
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    /// A `logfmt` record that would otherwise have gone to stderr.
    Log {
        stamp: String,
        level: Level,
        message: String,
    },
    /// Shared: the record the relay rendered and the record the dashboard
    /// shows are one allocation.
    Request(Arc<RequestRecord>),
}

/// Observers must return promptly. They run on the request path.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event);
}
impl<F: Fn(Event) + Send + Sync> EventSink for F {
    fn emit(&self, event: Event) {
        self(event);
    }
}
#[derive(Default)]
pub struct Telemetry {
    sink: Option<Arc<dyn EventSink>>,
    up: AtomicU64,
    down: AtomicU64,
    dropped: AtomicU64,
    flights: Flights,
}
impl std::fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telemetry")
            .field("socket_bytes", &self.socket_bytes())
            .finish_non_exhaustive()
    }
}
impl Telemetry {
    pub fn new(sink: impl EventSink + 'static) -> Self {
        Self {
            sink: Some(Arc::new(sink)),
            ..Self::default()
        }
    }
    pub fn emit(&self, event: Event) {
        if let Some(sink) = &self.sink {
            sink.emit(event);
        }
    }
    pub fn channel(capacity: usize) -> (Arc<Self>, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        let telemetry = Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let weak = weak.clone();
            Self::new(move |event| {
                if tx.try_send(event).is_err()
                    && let Some(telemetry) = weak.upgrade()
                {
                    telemetry.dropped.fetch_add(1, Ordering::Relaxed);
                }
            })
        });
        (telemetry, rx)
    }
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    pub fn log(&self, level: Level, message: &str) {
        self.emit(Event::Log {
            stamp: crate::time::stamp(),
            level,
            message: message.to_owned(),
        });
    }
    pub fn info(&self, message: &str) {
        self.log(Level::Info, message);
    }
    pub fn warn(&self, message: &str) {
        self.log(Level::Warning, message);
    }
    pub fn error(&self, message: &str) {
        self.log(Level::Error, message);
    }
    pub(crate) fn add_socket_up(&self, n: usize) {
        self.up.fetch_add(n as u64, Ordering::Relaxed);
    }
    pub(crate) fn add_socket_down(&self, n: usize) {
        self.down.fetch_add(n as u64, Ordering::Relaxed);
    }
    /// The requests this instance's forwarders have counted and not yet
    /// finished relaying.
    pub fn flights(&self) -> &Flights {
        &self.flights
    }
    pub fn socket_bytes(&self) -> (u64, u64) {
        (
            self.up.load(Ordering::Relaxed),
            self.down.load(Ordering::Relaxed),
        )
    }
}
