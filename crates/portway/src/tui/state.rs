//! Everything the dashboard shows, with no terminal in sight.
//!
//! Fed by two sources: the telemetry channel (one entry per request or log
//! record) and a 250ms sample — of the per-model counters `/__portway/stats`
//! serves when this process owns the forwarder, or of another forwarder's
//! window when it does not. Cumulative totals come from the counters rather
//! than from summing events, so the HUD and the JSON can never disagree; a
//! window has no counters but the events it replayed, and adds those up
//! instead.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::forwarder::{Coding, StatsView};
use crate::logfmt::{self, Level};
use crate::router::Router;
use crate::telemetry::{Event, RequestRecord};
use crate::tui::spend;
use crate::watch;

/// Event pane backlog. ~10k lines is minutes of a busy agent session and a
/// couple of MB at worst.
pub const EVENT_CAPACITY: usize = 10_000;
/// Rolling window for the latency percentiles.
pub const SAMPLES: usize = 512;
/// Per-request bars kept for the compression chart.
pub const BARS: usize = 1024;
/// One hour of one-second traffic buckets.
pub const TRAFFIC_SECONDS: usize = 3600;
/// Bucket widths `t` cycles through, in seconds.
pub const SCALES: [usize; 3] = [1, 10, 60];
/// How often the usage screen reads the day back. Its query is a fresh
/// connection and a scan of everything since local midnight, so it is nothing
/// like the 250ms sample the counters are.
pub const USAGE_REFRESH: Duration = Duration::from_secs(5);

/// One field a request line can carry, in the order the line draws them.
///
/// The set is what `--event-columns` names, what the picker toggles and what
/// the settings file remembers; the order itself is the layout's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Column {
    /// Local `HH:MM:SS` of the moment the relay ended.
    Time,
    /// The status the agent was answered with.
    Status,
    /// One cell, glued to its neighbours: `✂` when the relay ended before the
    /// upstream body did, nothing otherwise.
    Cut,
    Model,
    /// The route, with the `.` that marks a fresh dial glued to it.
    Route,
    /// Body as sent and as it went out, the ratio when it was compressed, and
    /// how long the upload took.
    Sizes,
    Ttfb,
    /// Response bytes and how long the body took.
    Down,
    /// The counts the engine reported for the answer. What the prefix cache
    /// read rides the prompt count in parentheses.
    Tokens,
}

/// Every column, in draw order.
pub const COLUMNS: [Column; 9] = [
    Column::Time,
    Column::Status,
    Column::Cut,
    Column::Model,
    Column::Route,
    Column::Sizes,
    Column::Ttfb,
    Column::Down,
    Column::Tokens,
];

impl Column {
    pub fn name(self) -> &'static str {
        match self {
            Column::Time => "time",
            Column::Status => "status",
            Column::Cut => "cut",
            Column::Model => "model",
            Column::Route => "route",
            Column::Sizes => "sizes",
            Column::Ttfb => "ttfb",
            Column::Down => "down",
            Column::Tokens => "tokens",
        }
    }

    /// What the picker says beside the name.
    pub fn note(self) -> &'static str {
        match self {
            Column::Time => "when the relay ended",
            Column::Status => "the code the agent got",
            Column::Cut => "one cell: ✂ on a cut relay",
            Column::Model => "the upstream the turn went to",
            Column::Route => "method and path, + . on a fresh dial",
            Column::Sizes => "raw → wire, ratio, upload time",
            Column::Ttfb => "first byte of the answer",
            Column::Down => "response bytes and time",
            Column::Tokens => "in (cached) → out, what the engine counted",
        }
    }

    pub fn from_name(name: &str) -> Option<Column> {
        COLUMNS.into_iter().find(|column| column.name() == name)
    }
}

/// Which fields a request line carries. A set rather than a sequence — the
/// order is the layout's — so it is one word wide and copying it is free.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Columns(u16);

impl Columns {
    /// Every field: what a line carries until something says otherwise.
    pub const ALL: Columns = Columns(0b1_1111_1111);

    pub fn contains(self, column: Column) -> bool {
        self.0 & (1 << column as u16) != 0
    }

    pub fn toggle(&mut self, column: Column) {
        self.0 ^= 1 << column as u16;
    }

    /// The names of what is on, in draw order: what the settings file holds.
    pub fn names(self) -> String {
        COLUMNS
            .into_iter()
            .filter(|column| self.contains(*column))
            .map(Column::name)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// A comma-separated list, the way the flag and the settings file write
    /// one. Unknown names are an error: a typo must not silently drop a field.
    pub fn parse(list: &str) -> Result<Columns, String> {
        let mut columns = Columns(0);
        for name in list
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let column = Column::from_name(name)
                .ok_or_else(|| format!("unknown column {name:?}: {}", Columns::ALL.names()))?;
            columns.0 |= 1 << column as u16;
        }
        Ok(columns)
    }
}

pub enum Entry {
    Log {
        stamp: String,
        level: Level,
        message: String,
    },
    Request(Arc<RequestRecord>),
}

impl Entry {
    fn is_trouble(&self) -> bool {
        match self {
            Entry::Log { level, .. } => *level >= Level::Warning,
            Entry::Request(record) => record.status >= 400 || !record.complete,
        }
    }

    fn model(&self) -> Option<&str> {
        match self {
            Entry::Request(record) => Some(&record.model),
            Entry::Log { .. } => None,
        }
    }
}

/// Entries carry a monotonic sequence number so the viewport can stay anchored
/// to a line while the ring evicts older ones underneath it.
pub struct Row {
    pub seq: u64,
    pub entry: Entry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    /// 4xx/5xx, truncated streams, and WARNING or worse.
    Trouble,
    Model(String),
}

impl Filter {
    pub fn label(&self) -> &str {
        match self {
            Filter::All => "all",
            Filter::Trouble => "trouble",
            Filter::Model(name) => name,
        }
    }

    fn accepts(&self, entry: &Entry) -> bool {
        match self {
            Filter::All => true,
            Filter::Trouble => entry.is_trouble(),
            Filter::Model(name) => entry.model() == Some(name.as_str()),
        }
    }
}

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

impl Traffic {
    fn new() -> Self {
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

pub struct State {
    pub prices: crate::config::Prices,
    entries: VecDeque<Row>,
    /// Sequence numbers of the entries the current filter admits, in order.
    filtered: VecDeque<u64>,
    next_seq: u64,
    pub filter: Filter,
    /// Viewport pinned to the newest line. Any upward move leaves it.
    pub follow: bool,
    /// Highlighted line, and the line at the top of the viewport.
    cursor: u64,
    top: u64,
    /// Rows the event pane last had room for; scrolling needs it.
    pub viewport: usize,
    pub detail: bool,
    pub help: bool,
    /// The column picker: which field is under the cursor while it is open.
    pub picker: bool,
    pub picker_at: usize,
    /// What a request line carries.
    pub columns: Columns,
    /// Set by the first quit keystroke while requests are still in flight.
    pub confirm_quit: bool,
    pub scale: usize,
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
    /// The database the usage screen reads the day back out of. `None` leaves
    /// it with nothing to draw but the reason.
    pub db: Option<PathBuf>,
    /// The usage screen: up or down, what it is measuring, what the last read
    /// found, and when it happened.
    pub usage_open: bool,
    pub usage_range: spend::Range,
    /// The rates popup, over the usage screen: what each upstream charges per
    /// million tokens, which is the one thing a per-row column has no room
    /// for beside the money it produced.
    pub usage_rates: bool,
    pub usage: Option<spend::Table>,
    pub usage_error: Option<String>,
    usage_read: Option<Instant>,
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
}

impl State {
    pub fn new() -> Self {
        State {
            prices: Default::default(),
            entries: VecDeque::new(),
            filtered: VecDeque::new(),
            next_seq: 0,
            filter: Filter::All,
            follow: true,
            cursor: 0,
            top: 0,
            viewport: 1,
            detail: false,
            help: false,
            picker: false,
            picker_at: 0,
            columns: Columns::ALL,
            confirm_quit: false,
            scale: SCALES[0],
            models: Vec::new(),
            totals: Totals::default(),
            traffic: Traffic::new(),
            started: Instant::now(),
            recorded: false,
            models_tally: BTreeMap::new(),
            retried_identity: 0,
            aborted: 0,
            coverage: None,
            db: None,
            usage_open: false,
            usage_range: spend::Range::Today,
            usage_rates: false,
            usage: None,
            usage_error: None,
            usage_read: None,
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
        }
    }

    pub fn push(&mut self, event: Event) {
        let entry = match event {
            Event::Log {
                stamp,
                level,
                message,
            } => {
                if self.recorded {
                    self.tally_line(&message);
                }
                Entry::Log {
                    stamp,
                    level,
                    message,
                }
            }
            Event::Request(record) => {
                self.tally(&record);
                if self.recorded {
                    self.tally_model(&record);
                }
                Entry::Request(record)
            }
        };
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.filter.accepts(&entry) {
            push_capped(&mut self.filtered, seq, EVENT_CAPACITY);
        }
        if self.entries.len() == EVENT_CAPACITY {
            let evicted = self.entries.pop_front().map(|row| row.seq).unwrap_or(0);
            if self.filtered.front() == Some(&evicted) {
                self.filtered.pop_front();
            }
        }
        self.entries.push_back(Row { seq, entry });
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
        }
    }

    /// Resample the per-model counters and the socket throughput.
    pub fn tick(&mut self, router: &Router) {
        self.models = router
            .models()
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
        self.coverage = Some(window.coverage);
    }

    // ----------------------------------------------------------- usage screen

    /// Put the usage screen up, reading the day in on the way.
    pub fn open_usage(&mut self) {
        self.usage_open = true;
        self.read_usage();
    }

    /// Put the dashboard back. What was read stays, so the next `u` draws a
    /// screenful at once and refreshes behind it; the popup does not, because
    /// it is a question that was asked of the screen it covers.
    pub fn close_usage(&mut self) {
        self.usage_open = false;
        self.usage_rates = false;
    }

    /// Re-read the day while the screen is up: the counts move as answers land,
    /// and a screen that quietly went stale is the one thing it must not be.
    pub fn refresh_usage(&mut self) {
        if !self.usage_open
            || self
                .usage_read
                .is_some_and(|at| at.elapsed() < USAGE_REFRESH)
        {
            return;
        }
        self.read_usage();
    }

    /// Move the usage screen's window a step and read it back at once: choosing a
    /// range is a question, and the answer should not wait for the next refresh.
    pub fn step_usage_range(&mut self, delta: isize) {
        self.usage_range = self.usage_range.step(delta);
        self.read_usage();
    }

    /// What the screen is showing is one of `spend::Range`'s windows — a local
    /// midnight to now, the day before, or a week of days — rather than the
    /// last N hours, half of which would be a different day's traffic.
    fn read_usage(&mut self) {
        let Some(db) = self.db.clone() else {
            self.usage = None;
            self.usage_error = Some("no database to read: none was passed in".to_string());
            return;
        };
        let (since, until) = self.usage_range.window(logfmt::epoch());
        match spend::load(&db, since, until, &self.prices) {
            Ok(table) => {
                self.usage = Some(table);
                self.usage_error = None;
            }
            Err(message) => {
                self.usage = None;
                self.usage_error = Some(message);
            }
        }
        self.usage_read = Some(Instant::now());
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

    // ------------------------------------------------------------ event pane

    pub fn len(&self) -> usize {
        self.filtered.len()
    }

    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    fn entry(&self, seq: u64) -> Option<&Entry> {
        let first = self.entries.front()?.seq;
        let index = seq.checked_sub(first)? as usize;
        self.entries.get(index).map(|row| &row.entry)
    }

    /// Where `seq` sits in the filtered list, clamped to what survived.
    fn position(&self, seq: u64) -> usize {
        let at = self.filtered.partition_point(|candidate| *candidate < seq);
        at.min(self.filtered.len().saturating_sub(1))
    }

    fn cursor_position(&self) -> usize {
        if self.follow {
            self.filtered.len().saturating_sub(1)
        } else {
            self.position(self.cursor)
        }
    }

    /// The slice of the filtered list the pane shows: `(first index, rows)`.
    ///
    /// The anchor can be evicted or filtered away between frames, so it is
    /// re-clamped here rather than trusted; the window always contains the
    /// cursor.
    fn window(&self) -> (usize, usize) {
        let height = self.viewport.max(1);
        let len = self.filtered.len();
        if self.follow {
            return (len.saturating_sub(height), height);
        }
        let cursor = self.cursor_position();
        let first = self
            .position(self.top)
            .min(cursor)
            .max((cursor + 1).saturating_sub(height))
            .min(len.saturating_sub(height));
        (first, height)
    }

    /// The lines to draw, oldest first, with the highlighted one flagged.
    pub fn visible(&self) -> Vec<(&Entry, bool)> {
        let (first, height) = self.window();
        let cursor = self.cursor_position();
        self.filtered
            .iter()
            .enumerate()
            .skip(first)
            .take(height)
            .filter_map(|(index, seq)| {
                self.entry(*seq)
                    .map(|entry| (entry, !self.follow && index == cursor))
            })
            .collect()
    }

    /// The highlighted request, for the detail popup.
    pub fn selected(&self) -> Option<&RequestRecord> {
        let seq = *self.filtered.get(self.cursor_position())?;
        match self.entry(seq)? {
            Entry::Request(record) => Some(record.as_ref()),
            Entry::Log { .. } => None,
        }
    }

    /// Move the cursor by `delta` lines. Walking off the bottom resumes
    /// following, which is how the pane gets back to live traffic.
    pub fn scroll(&mut self, delta: isize) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        let current = self.cursor_position() as isize;
        let next = current + delta;
        if next >= len as isize - 1 && delta > 0 {
            self.follow = true;
            return;
        }
        let next = next.clamp(0, len as isize - 1) as usize;
        self.follow = false;
        self.cursor = self.filtered[next];
        let height = self.viewport.max(1);
        let top = self.position(self.top);
        let top = top.min(next).max((next + 1).saturating_sub(height));
        self.top = self.filtered[top.min(len - 1)];
    }

    pub fn page(&mut self, pages: isize) {
        self.scroll(pages * self.viewport.max(1) as isize);
    }

    pub fn to_oldest(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.follow = false;
        self.cursor = self.filtered[0];
        self.top = self.cursor;
    }

    pub fn to_newest(&mut self) {
        self.follow = true;
    }

    /// Lines hidden below the viewport — what scrolling back down would
    /// reveal, which is not the same as what arrived while it was pinned.
    pub fn below(&self) -> usize {
        let (first, height) = self.window();
        self.filtered.len().saturating_sub(first + height)
    }

    pub fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.filtered = self
            .entries
            .iter()
            .filter(|row| self.filter.accepts(&row.entry))
            .map(|row| row.seq)
            .collect();
        // The old anchor may not be in the new list at all.
        self.follow = true;
    }

    /// `all -> trouble` and then once through the models.
    pub fn cycle_filter(&mut self) {
        let names: Vec<&str> = self.models.iter().map(|row| row.name.as_str()).collect();
        let next = match &self.filter {
            Filter::All => Filter::Trouble,
            Filter::Trouble => match names.first() {
                Some(name) => Filter::Model((*name).to_string()),
                None => Filter::All,
            },
            Filter::Model(current) => {
                let at = names.iter().position(|name| name == current);
                match at.and_then(|at| names.get(at + 1)) {
                    Some(name) => Filter::Model((*name).to_string()),
                    None => Filter::All,
                }
            }
        };
        self.set_filter(next);
    }

    pub fn cycle_scale(&mut self) {
        let at = SCALES.iter().position(|scale| *scale == self.scale);
        self.scale = SCALES[(at.unwrap_or(0) + 1) % SCALES.len()];
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
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

fn push_capped<T>(queue: &mut VecDeque<T>, value: T, cap: usize) {
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

    /// The set is what the flag, the file and the picker all speak: a name
    /// round-trips, and a typo is an error rather than a field going missing.
    #[test]
    fn a_column_set_round_trips_through_its_names() {
        assert_eq!(Columns::ALL.names().split(',').count(), COLUMNS.len());
        let some = Columns::parse(" time , route,tokens ").unwrap();
        assert!(some.contains(Column::Time));
        assert!(some.contains(Column::Route));
        assert!(some.contains(Column::Tokens));
        assert!(!some.contains(Column::Status));
        assert_eq!(some.names(), "time,route,tokens");
        assert_eq!(Columns::parse(&some.names()).unwrap(), some);

        let err = Columns::parse("time,uri").unwrap_err();
        assert!(err.contains("uri"), "{err}");
        assert!(err.contains("route"), "{err}");

        // Toggling twice is where it started, and starts from nothing at all.
        let mut columns = Columns::parse("").unwrap();
        assert_eq!(columns.names(), "");
        for column in COLUMNS {
            columns.toggle(column);
        }
        assert_eq!(columns.names(), Columns::ALL.names());
    }
}
