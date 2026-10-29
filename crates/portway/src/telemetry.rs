//! Application event fan-out. Core observations are injected, not global.
use crate::logfmt::{self, Level};
pub use portway_core::telemetry::{Event, RequestRecord, Telemetry};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
/// Where events go: the dashboard when it owns the terminal, the web console
/// when one is served, the recorder in every mode. All are optional — a mode
/// installs what it has, and a mode with none is a test.
#[derive(Default)]
pub struct Sinks {
    pub tui: Option<ViewerSender>,
    pub web: Option<ViewerSender>,
    pub store: Option<SyncSender<Event>>,
}

/// Live dashboards must not retain an unlimited backlog when rendering stalls.
pub const VIEWER_QUEUE: usize = 4096;
/// Leave time for sampling, input and shutdown even under continuous traffic.
pub const VIEWER_BATCH: usize = 256;

pub struct ViewerSender {
    kind: ViewerKind,
    dropped: AtomicU64,
}

enum ViewerKind {
    Live(SyncSender<Event>),
    /// Database backfill is queued before the viewer starts. Preserve it all;
    /// this sender is never used by the live forwarding process.
    Recorded(Sender<Event>),
}

impl From<Sender<Event>> for ViewerSender {
    fn from(sender: Sender<Event>) -> Self {
        Self {
            kind: ViewerKind::Recorded(sender),
            dropped: AtomicU64::new(0),
        }
    }
}

pub fn viewer_channel() -> (ViewerSender, Receiver<Event>) {
    let (sender, receiver) = std::sync::mpsc::sync_channel(VIEWER_QUEUE);
    (
        ViewerSender {
            kind: ViewerKind::Live(sender),
            dropped: AtomicU64::new(0),
        },
        receiver,
    )
}

impl ViewerSender {
    fn send(&self, event: &Event) {
        let ViewerKind::Live(sender) = &self.kind else {
            if let ViewerKind::Recorded(sender) = &self.kind {
                let _ = sender.send(event.clone());
            }
            return;
        };
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let warning = Event::Log {
                stamp: logfmt::stamp(),
                level: Level::Warning,
                message: format!(
                    "dashboard: {dropped} live events skipped while its queue was full; event totals may be incomplete"
                ),
            };
            if let Err(TrySendError::Full(_)) = sender.try_send(warning) {
                self.dropped.fetch_add(dropped + 1, Ordering::Relaxed);
                return;
            }
        }
        if let Err(TrySendError::Full(_)) = sender.try_send(event.clone()) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Process-wide, like the log it replaces. Set once, before the first request.
static SINKS: OnceLock<Sinks> = OnceLock::new();

/// Events the recorder had no room for. The proxy outranks the record, so the
/// send never blocks and never grows without bound.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Route events to `sinks`. Returns false when sinks were already installed.
pub fn install(sinks: Sinks) -> bool {
    SINKS.set(sinks).is_ok()
}

/// Whether a dashboard owns the terminal, which is what decides if the console
/// line is still printed.
pub fn tui_installed() -> bool {
    SINKS.get().and_then(|sinks| sinks.tui.as_ref()).is_some()
}

/// Hand `event` to every installed sink. A request path never waits for one:
/// live viewers and the recorder use bounded, nonblocking queues. A full
/// queue drops the event and counts it.
pub fn emit(event: Event) {
    let Some(sinks) = SINKS.get() else {
        return;
    };
    viewers(sinks, &event);
    if let Some(store) = &sinks.store
        && let Err(TrySendError::Full(_)) = store.try_send(event)
    {
        let total = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
        if total.is_multiple_of(1000) {
            logfmt::from_store(
                Level::Warning,
                &format!("store: {total} events dropped, the recorder is behind"),
            );
        }
    }
}

/// Hand `event` to the dashboards alone. The recorder's own records use this:
/// a failing write that queued another write would be a loop.
pub fn emit_viewers(event: Event) {
    if let Some(sinks) = SINKS.get() {
        viewers(sinks, &event);
    }
}

fn viewers(sinks: &Sinks, event: &Event) {
    for viewer in [&sinks.tui, &sinks.web].into_iter().flatten() {
        viewer.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stalled_live_viewer_is_bounded_and_reports_its_missing_events() {
        let (sender, events) = viewer_channel();
        let event = Event::Log {
            stamp: "00:00:00".into(),
            level: Level::Info,
            message: "event".into(),
        };
        for _ in 0..VIEWER_QUEUE + 3 {
            sender.send(&event);
        }
        assert_eq!(events.try_iter().count(), VIEWER_QUEUE);
        sender.send(&event);
        let recovered: Vec<_> = events.try_iter().collect();
        assert!(
            matches!(&recovered[0], Event::Log { level: Level::Warning, message, .. } if message.contains("3 live events skipped"))
        );
        assert_eq!(recovered.len(), 2);
    }
}

/// The CLI owns a single runtime. Embedders use their own Telemetry instead.
pub fn core() -> Arc<Telemetry> {
    static CORE: OnceLock<Arc<Telemetry>> = OnceLock::new();
    Arc::clone(CORE.get_or_init(|| {
        Arc::new(Telemetry::new(|event: Event| {
            if !tui_installed() {
                match &event {
                    Event::Request(record) => {
                        logfmt::console(Level::Info, &crate::request_log::render(record))
                    }
                    Event::Log { level, message, .. } => logfmt::console(*level, message),
                }
            }
            emit(event);
        }))
    }))
}
