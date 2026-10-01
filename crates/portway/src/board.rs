//! The numbers both dashboards draw: per-model counters, the HUD totals, the
//! latency samples, the body chart and the socket throughput.
//!
//! Fed by two sources: the telemetry channel (one entry per request or log
//! record) and a 250ms sample — of the per-model counters `/__portway/stats`
//! serves when this process owns the forwarder, or of another forwarder's
//! window when it does not. Cumulative totals come from the counters rather
//! than from summing events, so the HUD and the JSON can never disagree; a
//! window has no counters but the events it replayed, and adds those up
//! instead.
//!
//! The labels that explain a number — the coding a model negotiated, why it
//! is not compressing, how a route is shown — live here too, so the terminal
//! and the browser cannot word the same state two ways.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use http::Method;

use crate::forwarder::{Coding, CompressionIssue, StatsView};
use crate::router::Router;
use crate::telemetry::{Event, RequestRecord};
use crate::watch;

/// Rolling window for the latency percentiles.
pub const SAMPLES: usize = 512;
/// Per-request bars kept for the compression chart.
pub const BARS: usize = 1024;
/// One hour of one-second traffic buckets.
pub const TRAFFIC_SECONDS: usize = 3600;
/// Bucket widths `t` cycles through, in seconds.
pub const SCALES: [usize; 3] = [1, 10, 60];
#[derive(Clone)]
pub struct ModelRow {
    pub name: String,
    pub view: StatsView,
}

/// The per-model counters added up, for the HUD.
#[derive(Default, Clone)]
pub struct Totals {
    pub requests: u64,
    pub encoded: u64,
    pub in_flight: u64,
    pub body_bytes: u64,
    pub wire_bytes: u64,
    pub down_bytes: u64,
    pub down_wire_bytes: u64,
    /// What the hop actually sent the agent once it re-encoded; 0 on a replay
    /// of rows recorded before the counters existed, so the header hides it.
    pub agent_bytes: u64,
    pub retried_identity: u64,
    pub aborts: u64,
    pub upstream_errors: u64,
    pub idle_conns: usize,
}

/// Bytes/second per column, newest on the right.
pub struct Series {
    pub up: Vec<u64>,
    pub down: Vec<u64>,
}

/// Socket throughput in one-second buckets, aggregated on the way out.
pub struct Traffic {
    up: VecDeque<u64>,
    down: VecDeque<u64>,
    /// Cumulative counters as of the last sample, to difference against.
    last: (u64, u64),
    /// Index of the second the back bucket describes.
    second: u64,
}

impl Default for Traffic {
    fn default() -> Self {
        Self::new()
    }
}

impl Traffic {
    pub fn new() -> Self {
        Traffic {
            up: VecDeque::from([0]),
            down: VecDeque::from([0]),
            last: (0, 0),
            second: 0,
        }
    }

    /// Fold the counters' growth into the bucket for `elapsed`, opening empty
    /// buckets for any second that went by without a sample.
    fn sample(&mut self, elapsed: u64, totals: (u64, u64)) {
        while self.second < elapsed {
            push_capped(&mut self.up, 0, TRAFFIC_SECONDS);
            push_capped(&mut self.down, 0, TRAFFIC_SECONDS);
            self.second += 1;
        }
        let grew = (
            totals.0.saturating_sub(self.last.0),
            totals.1.saturating_sub(self.last.1),
        );
        self.last = totals;
        if let Some(slot) = self.up.back_mut() {
            *slot += grew.0;
        }
        if let Some(slot) = self.down.back_mut() {
            *slot += grew.1;
        }
    }

    /// Replace the buckets with a window read out of the database, oldest
    /// first. A window is sampled whole rather than accumulated, so whatever
    /// was on the chart is not added to — it is the chart.
    pub fn load(&mut self, buckets: &[(u64, u64)]) {
        self.up.clear();
        self.down.clear();
        let skip = buckets.len().saturating_sub(TRAFFIC_SECONDS);
        for (up, down) in buckets.iter().skip(skip) {
            self.up.push_back(*up);
            self.down.push_back(*down);
        }
    }

    /// Index of the second the newest bucket describes, counted from the
    /// board's start: what lets a client tell a bucket still filling from a
    /// new one.
    pub fn second(&self) -> u64 {
        self.second
    }

    /// The one-second buckets, oldest first: `(up, down)`.
    pub fn buckets(&self) -> (&VecDeque<u64>, &VecDeque<u64>) {
        (&self.up, &self.down)
    }

    /// The last `width` columns of `scale`-second buckets, as a rate so the
    /// chart's shape does not jump when the bucket width changes.
    pub fn series(&self, scale: usize, width: usize) -> Series {
        Series {
            up: rate(&self.up, scale, width),
            down: rate(&self.down, scale, width),
        }
    }
}

fn rate(buckets: &VecDeque<u64>, scale: usize, width: usize) -> Vec<u64> {
    if width == 0 || scale == 0 {
        return Vec::new();
    }
    let wanted = width * scale;
    let skip = buckets.len().saturating_sub(wanted);
    let tail: Vec<u64> = buckets.iter().skip(skip).copied().collect();
    // Right-align: a short history leaves the left columns empty rather than
    // stretching a few seconds across the whole chart.
    let mut out = vec![0u64; width.saturating_sub(tail.len().div_ceil(scale))];
    for chunk in tail.chunks(scale) {
        out.push(chunk.iter().sum::<u64>() / scale as u64);
    }
    out
}

pub struct Board {
    pub models: Vec<ModelRow>,
    pub totals: Totals,
    pub traffic: Traffic,
    pub started: Instant,
    /// Set when the rows come from another forwarder: the per-model counters
    /// below are then the only ones there are, and the log lines the window
    /// replayed are the only place its retries and aborts can be counted.
    pub recorded: bool,
    models_tally: BTreeMap<String, StatsView>,
    retried_identity: u64,
    aborted: u64,
    /// How far back the rows on screen reach, when they were read rather than
    /// served: the live process knows its own uptime instead.
    pub coverage: Option<Duration>,
    /// Counts the events carry but the atomics do not.
    pub seen: u64,
    pub ok: u64,
    pub redirected: u64,
    pub client_errors: u64,
    pub server_errors: u64,
    pub reused: u64,
    pub truncated: u64,
    pub ttfb: VecDeque<f64>,
    pub upload: VecDeque<f64>,
    pub handshake: VecDeque<f64>,
    /// `(raw, wire)` of every request that carried a body.
    pub bars: VecDeque<(u64, u64)>,
    /// Bars ever pushed, so a reader can ask for the ones it has not seen.
    pub bars_pushed: u64,
}

impl Default for Board {
    fn default() -> Self {
        Self::new()
    }
}

impl Board {
    pub fn new() -> Self {
        Board {
            models: Vec::new(),
            totals: Totals::default(),
            traffic: Traffic::new(),
            started: Instant::now(),
            recorded: false,
            models_tally: BTreeMap::new(),
            retried_identity: 0,
            aborted: 0,
            coverage: None,
            seen: 0,
            ok: 0,
            redirected: 0,
            client_errors: 0,
            server_errors: 0,
            reused: 0,
            truncated: 0,
            ttfb: VecDeque::new(),
            upload: VecDeque::new(),
            handshake: VecDeque::new(),
            bars: VecDeque::new(),
            bars_pushed: 0,
        }
    }

    /// Count one event. Requests always feed the samples; on a replayed
    /// window they also feed the per-model tally, and log lines the two
    /// counters only the log ever carried.
    pub fn observe(&mut self, event: &Event) {
        match event {
            Event::Log { message, .. } => {
                if self.recorded {
                    self.tally_line(message);
                }
            }
            Event::Request(record) => {
                self.tally(record);
                if self.recorded {
                    self.tally_model(record);
                }
            }
        }
    }

    fn tally(&mut self, record: &RequestRecord) {
        self.seen += 1;
        match record.status {
            status if status < 300 => self.ok += 1,
            status if status < 400 => self.redirected += 1,
            status if status < 500 => self.client_errors += 1,
            _ => self.server_errors += 1,
        }
        if record.reused() {
            self.reused += 1;
        }
        if !record.complete {
            self.truncated += 1;
        }
        push_capped(&mut self.ttfb, record.ttfb, SAMPLES);
        if let Some(upload) = record.upload {
            push_capped(&mut self.upload, upload, SAMPLES);
        }
        if let Some(handshake) = record.handshake() {
            push_capped(&mut self.handshake, handshake, SAMPLES);
        }
        if record.body_len > 0 {
            // zstd can round up on incompressible input; a bar never exceeds
            // its own raw size.
            let wire = record.wire_len.min(record.body_len);
            push_capped(&mut self.bars, (record.body_len, wire), BARS);
            self.bars_pushed += 1;
        }
    }

    /// Resample the per-model counters and the socket throughput.
    pub fn tick(&mut self, router: &Router) {
        self.models = router
            .routes()
            .iter()
            .map(|(name, forwarder)| ModelRow {
                name: name.clone(),
                view: forwarder.view(),
            })
            .collect();
        self.totals = absorb(&self.models);
        self.traffic.sample(
            self.started.elapsed().as_secs(),
            router.telemetry().socket_bytes(),
        );
    }

    /// The same 250ms sample when the counters are another forwarder's rows:
    /// the model table is the tally the replayed events kept, and the window
    /// itself is the only source for the per-second bytes.
    pub fn tick_recorded(&mut self, window: &watch::Window) {
        self.models = self
            .models_tally
            .iter()
            .map(|(name, view)| ModelRow {
                name: name.clone(),
                view: view.clone(),
            })
            .collect();
        let mut totals = absorb(&self.models);
        // Nothing per model can know these — the line names the route, not the
        // upstream — so they are counted off the replayed lines instead.
        totals.retried_identity = self.retried_identity;
        totals.aborts = self.aborted;
        self.totals = totals;
        self.traffic.load(&window.traffic);
        self.traffic.second = self.started.elapsed().as_secs();
        self.coverage = Some(window.coverage);
    }

    /// One replayed request, added to the model it went to. The fields the
    /// running process keeps in memory — in flight, idle connections, whether
    /// a dictionary is in use — stay at zero: no row can carry them.
    fn tally_model(&mut self, record: &RequestRecord) {
        let view = self.models_tally.entry(record.model.clone()).or_default();
        view.requests += 1;
        if record.coding != Coding::None {
            view.encoded_requests += 1;
        }
        view.body_bytes += record.body_len;
        view.wire_bytes += record.wire_len;
        view.down_bytes += record.received;
        view.down_wire_bytes += record.received_wire;
        view.agent_bytes += record.received_agent;
        if record.status >= 500 {
            view.upstream_errors += 1;
        }
        // What the model is doing now, which is what the column is for.
        view.coding = record.coding;
    }

    /// The two counters the request path only ever wrote to the log, read back
    /// out of the lines the window replayed. Both messages are built in
    /// `forwarder.rs` and `relay.rs`.
    fn tally_line(&mut self, message: &str) {
        if message.ends_with("agent left before the first byte") {
            self.aborted += 1;
        } else if message.ends_with("resending identity") {
            self.retried_identity += 1;
        }
    }
}

/// The per-model counters added up, which is what the HUD's totals are in
/// either mode.
fn absorb(models: &[ModelRow]) -> Totals {
    let mut totals = Totals::default();
    for row in models {
        totals.requests += row.view.requests;
        totals.encoded += row.view.encoded_requests;
        totals.in_flight += row.view.in_flight;
        totals.body_bytes += row.view.body_bytes;
        totals.wire_bytes += row.view.wire_bytes;
        totals.down_bytes += row.view.down_bytes;
        totals.down_wire_bytes += row.view.down_wire_bytes;
        totals.agent_bytes += row.view.agent_bytes;
        totals.retried_identity += row.view.retried_identity;
        totals.aborts += row.view.client_aborts;
        totals.upstream_errors += row.view.upstream_errors;
        totals.idle_conns += row.view.idle_conns;
    }
    totals
}

pub(crate) fn push_capped<T>(queue: &mut VecDeque<T>, value: T, cap: usize) {
    if queue.len() >= cap {
        queue.pop_front();
    }
    queue.push_back(value);
}

/// Nearest-rank percentile over a rolling window. Sorting 512 floats four
/// times a second costs nothing and keeps the window honest.
pub fn percentile(samples: &VecDeque<f64>, quantile: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = samples.iter().copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted.get(at).copied()
}

pub fn mean(samples: &VecDeque<f64>) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    Some(samples.iter().sum::<f64>() / samples.len() as f64)
}

/// `zstd+dcz`, `zstd`, or `identity`: what a model's uploads go out as.
pub fn coding_label(coding: Coding, dict: bool) -> String {
    match coding.name() {
        Some(name) if dict => format!("{name}+dcz"),
        Some(name) => name.to_string(),
        None => "identity".to_string(),
    }
}

/// How a route is shown: the four the forwarder is built for as
/// `POST ../completions`, marked known so they can be drawn quietly, and
/// anything else whole, which is worth a second look. The query never shows.
pub fn route(method: &Method, path: &str) -> (String, bool) {
    let path = path.split('?').next().unwrap_or(path);
    let known = matches!(
        (method, path),
        (&Method::POST, "/v1/chat/completions")
            | (&Method::POST, "/v1/completions")
            | (&Method::POST, "/v1/embeddings")
            | (&Method::GET, "/v1/models")
    );
    let shown = if known {
        format!("{method} ../{}", path.rsplit('/').next().unwrap_or(path))
    } else {
        format!("{method} {path}")
    };
    (shown, known)
}

pub fn compression_status(view: &StatsView) -> String {
    let issue = if view.coding == Coding::None {
        view.identity_reason
            .map(|reason| (reason, view.identity_backoff_secs))
    } else if !view.dict {
        view.dict_backoff_reason
            .map(|reason| (reason, view.dict_backoff_secs))
    } else {
        None
    };
    let Some((reason, seconds)) = issue else {
        return if view.last_probe_ok == Some(false) {
            "probe failed; coding kept".into()
        } else {
            String::new()
        };
    };
    let label = match reason {
        CompressionIssue::NotNegotiated => return "not negotiated".into(),
        CompressionIssue::ConfiguredOff => return "compression off in config".into(),
        CompressionIssue::ProbeFailed => return "probe failed".into(),
        CompressionIssue::NoSupportedCoding => return "no supported coding".into(),
        CompressionIssue::EncodingRefused => "415 backoff",
        CompressionIssue::DictionaryRefused => "dcz 415 backoff",
        CompressionIssue::HashMismatch => "dcz hash mismatch",
    };
    if seconds > 0 {
        format!("{label}; {seconds}s")
    } else {
        format!("{label}; probe due")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window is sampled whole rather than accumulated, so loading one
    /// replaces whatever the chart was holding — and the window that does not
    /// fit keeps its newest seconds, which are the ones on the right.
    #[test]
    fn a_loaded_window_is_the_chart_until_the_next_one() {
        let mut traffic = Traffic::new();
        traffic.load(&[(10, 1), (20, 2), (30, 3)]);
        assert_eq!(traffic.series(1, 3).up, vec![10, 20, 30]);
        assert_eq!(traffic.series(1, 3).down, vec![1, 2, 3]);

        traffic.load(&[(40, 4)]);
        // Right-aligned: a window with one second in it leaves the left
        // columns empty rather than stretching that second across the chart.
        assert_eq!(traffic.series(1, 3).up, vec![0, 0, 40]);

        let long: Vec<(u64, u64)> = (0..TRAFFIC_SECONDS as u64 + 3)
            .map(|second| (second, second))
            .collect();
        traffic.load(&long);
        let series = traffic.series(1, TRAFFIC_SECONDS);
        assert_eq!(series.up.len(), TRAFFIC_SECONDS);
        assert_eq!(series.up[0], 3);
        assert_eq!(series.up[TRAFFIC_SECONDS - 1], TRAFFIC_SECONDS as u64 + 2);
    }

    /// A reload's counters restart, but a status line never loses a reason
    /// it had: the backoff wins over the probe, and a zero wait says so.
    #[test]
    fn compression_status_words_every_issue() {
        let mut view = StatsView {
            coding: Coding::None,
            identity_reason: Some(CompressionIssue::EncodingRefused),
            identity_backoff_secs: 42,
            ..StatsView::default()
        };
        assert_eq!(compression_status(&view), "415 backoff; 42s");
        view.identity_backoff_secs = 0;
        assert_eq!(compression_status(&view), "415 backoff; probe due");
        view.identity_reason = None;
        view.last_probe_ok = Some(false);
        assert_eq!(compression_status(&view), "probe failed; coding kept");
    }

    #[test]
    fn known_routes_are_shortened_and_the_query_never_shows() {
        assert_eq!(
            route(&Method::POST, "/v1/chat/completions?x=1"),
            ("POST ../completions".to_string(), true)
        );
        assert_eq!(
            route(&Method::GET, "/health"),
            ("GET /health".to_string(), false)
        );
        assert_eq!(coding_label(Coding::Zstd, true), "zstd+dcz");
        assert_eq!(coding_label(Coding::None, true), "identity");
    }
}
