//! Application event fan-out. Core observations are injected, not global.
use crate::logfmt::{self, Level};
pub use portway_core::telemetry::{Event, RequestRecord, Telemetry};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
/// Where events go: the dashboard when it owns the terminal, the web console
/// when one is served, the recorder in every mode. All are optional — a mode
/// installs what it has, and a mode with none is a test.
#[derive(Default)]
pub struct Sinks {
    pub tui: Option<Sender<Event>>,
    pub web: Option<Sender<Event>>,
    pub store: Option<SyncSender<Event>>,
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
/// the viewers' channels are unbounded and the recorder's is sent to with
/// `try_send` — a full queue drops the event and counts it.
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
        let _ = viewer.send(event.clone());
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
