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

use std::ops::{Deref, DerefMut};

use crate::board::{self, Board, push_capped};
pub use crate::board::{
    BARS, ModelRow, SAMPLES, SCALES, Series, TRAFFIC_SECONDS, Totals, Traffic, mean, percentile,
};
use crate::flights::FlightView;
use crate::logfmt::{self, Level};
use crate::router::Router;
use crate::spend;
use crate::telemetry::{Event, RequestRecord};
use crate::tui::theme::{TERMINAL, Theme};

/// Event pane backlog. ~10k lines is minutes of a busy agent session and a
/// couple of MB at worst.
pub const EVENT_CAPACITY: usize = 10_000;
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
            Column::Model => "the model the turn named; - when it named none",
            Column::Route => "method and path, + . on a fresh dial",
            Column::Sizes => "raw → wire, ratio, upload time",
            Column::Ttfb => "first byte of the answer",
            Column::Down => "decoded → wire, ratio, download time",
            Column::Tokens => "in (% cached) → out, what the engine counted",
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

    fn upstream(&self) -> Option<&str> {
        match self {
            Entry::Request(record) => Some(&record.upstream),
            Entry::Log { .. } => None,
        }
    }

    /// A model catalog fetch answered with a 2xx in full: counted, but not a
    /// line. One that failed, was cut or was redirected stays.
    fn is_quiet(&self) -> bool {
        match self {
            Entry::Request(record) => {
                board::catalog(&record.method, &record.path)
                    && (200..300).contains(&record.status)
                    && record.complete
            }
            Entry::Log { .. } => false,
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
    /// One route: a mount, a model, or the single upstream.
    Upstream(String),
}

impl Filter {
    pub fn label(&self) -> &str {
        match self {
            Filter::All => "all",
            Filter::Trouble => "trouble",
            Filter::Upstream(name) => name,
        }
    }

    fn accepts(&self, entry: &Entry) -> bool {
        if entry.is_quiet() {
            return false;
        }
        match self {
            Filter::All => true,
            Filter::Trouble => entry.is_trouble(),
            Filter::Upstream(name) => entry.upstream() == Some(name.as_str()),
        }
    }
}

pub struct State {
    pub remote: Option<crate::remote::wire::RemoteState>,
    pub prices: crate::config::Prices,
    entries: VecDeque<Row>,
    /// Sequence numbers of the entries the current filter admits, in order.
    filtered: VecDeque<u64>,
    next_seq: u64,
    /// Last observed request per route, independent of event filters/eviction.
    model_recency: BTreeMap<String, u64>,
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
    /// What everything is drawn in.
    pub theme: &'static Theme,
    /// The theme picker: which theme is under the cursor while it is open,
    /// and the one it was opened over, which closing without keeping puts
    /// back. The dashboard wears the theme under the cursor meanwhile.
    pub themes: bool,
    pub themes_at: usize,
    pub themes_kept: &'static Theme,
    /// Set by the first quit keystroke while requests are still in flight.
    pub confirm_quit: bool,
    pub scale: usize,
    /// The numbers: counters, samples and charts, shared with the web console.
    pub board: Board,
    /// Oldest first, sampled from the owning forwarder every 250ms.
    pub flights: Vec<FlightView>,
    /// False only when an attached viewer cannot obtain a current snapshot.
    pub flights_available: bool,
    /// The `f` dialog listing them, which holds the keyboard while it is up.
    pub flights_open: bool,
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
}

impl State {
    pub fn new() -> Self {
        State {
            remote: None,
            prices: Default::default(),
            entries: VecDeque::new(),
            filtered: VecDeque::new(),
            next_seq: 0,
            model_recency: BTreeMap::new(),
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
            theme: &TERMINAL,
            themes: false,
            themes_at: 0,
            themes_kept: &TERMINAL,
            confirm_quit: false,
            scale: SCALES[0],
            board: Board::new(),
            flights: Vec::new(),
            flights_available: true,
            flights_open: false,
            db: None,
            usage_open: false,
            usage_range: spend::Range::Today,
            usage_rates: false,
            usage: None,
            usage_error: None,
            usage_read: None,
        }
    }

    pub fn push(&mut self, event: Event) {
        if self.remote.is_none() {
            self.board.observe(&event);
        }
        let entry = match event {
            Event::Log {
                stamp,
                level,
                message,
            } => Entry::Log {
                stamp,
                level,
                message,
            },
            Event::Request(record) => {
                self.model_recency
                    .insert(record.upstream.clone(), self.next_seq);
                if let Some(id) = record.flight {
                    self.flights.retain(|flight| flight.id != id);
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

    /// Remote events populate the list only; counters and charts are absolute
    /// values from the console, not a second tally of its event backlog.
    pub fn apply_remote(&mut self, update: crate::remote::Update) {
        use crate::remote::{Update, wire};
        match update {
            Update::Snapshot(snapshot) => {
                self.entries.clear();
                self.filtered.clear();
                self.next_seq = 0;
                self.model_recency.clear();
                self.cursor = 0;
                self.top = 0;
                self.detail = false;
                for event in snapshot.events {
                    self.push(event.into_event());
                }
                let remote = self.remote.as_mut().expect("remote feed");
                remote.seq = snapshot.seq;
                remote.generation = snapshot.generation;
                remote.coding = if snapshot.header.mode == "attached" {
                    "attached console".into()
                } else {
                    snapshot.header.coding
                };
                remote.uptime = snapshot.header.uptime_s;
                remote.sampled = Instant::now();
                remote.bars_total = snapshot.bars_total;
                remote.traffic(snapshot.traffic, true);
                self.board.bars = snapshot
                    .bars
                    .into_iter()
                    .rev()
                    .take(BARS)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                self.remote_metrics(snapshot.metrics);
                self.apply_remote(Update::Flights(snapshot.flights));
            }
            Update::Tick(tick) => {
                let remote = self.remote.as_mut().expect("remote feed");
                remote.uptime = tick.uptime_s;
                remote.sampled = Instant::now();
                wire::append_bars(
                    &mut self.board.bars,
                    remote.bars_total,
                    tick.bars_total,
                    tick.bars_push,
                );
                remote.bars_total = tick.bars_total;
                remote.traffic(tick.traffic_tail, false);
                self.remote_metrics(tick.metrics);
            }
            Update::Event(event) => {
                let remote = self.remote.as_mut().expect("remote feed");
                if event.seq <= remote.seq {
                    return;
                }
                remote.seq = event.seq;
                if let wire::EventData::Request(record) = &event.data
                    && let Some(id) = record.flight
                {
                    remote.original_flights.retain(|flight| flight.id != id);
                }
                self.push(event.into_event());
            }
            Update::Flights(flights) => {
                self.flights_available = flights.is_some();
                if let Some(flights) = flights {
                    self.board.totals.in_flight = flights.total;
                    self.flights = flights
                        .list
                        .into_iter()
                        .take(crate::live::MAX_FLIGHTS)
                        .map(|flight| flight.0)
                        .collect();
                } else {
                    self.flights.clear();
                }
                let remote = self.remote.as_mut().expect("remote feed");
                remote.original_flights = self.flights.clone();
                remote.flights_at = Instant::now();
            }
            Update::Usage(range, result) => {
                if !self.usage_open || range != self.usage_range {
                    return;
                }
                match result {
                    Ok(usage) => {
                        self.remote.as_mut().expect("remote feed").usage_title = Some(usage.title);
                        self.usage = Some(usage.table);
                        self.usage_error = None;
                    }
                    Err(error) => {
                        self.usage = None;
                        self.usage_error = Some(error);
                    }
                }
            }
            Update::Connection(connected, message) => {
                let remote = self.remote.as_mut().expect("remote feed");
                remote.connected = connected;
                remote.message = message;
                if !connected {
                    remote.original_flights.clear();
                    self.flights.clear();
                    self.flights_available = false;
                }
            }
        }
    }

    fn remote_metrics(&mut self, metrics: crate::remote::wire::Metrics) {
        self.board.totals = metrics.totals;
        self.board.seen = metrics.counts.seen;
        self.board.ok = metrics.counts.ok;
        self.board.redirected = metrics.counts.redirected;
        self.board.client_errors = metrics.counts.client_errors;
        self.board.server_errors = metrics.counts.server_errors;
        self.board.reused = metrics.counts.reused;
        self.board.truncated = metrics.counts.truncated;
        self.board.coverage = metrics
            .coverage
            .and_then(|n| Duration::try_from_secs_f64(n).ok());
        let remote = self.remote.as_mut().expect("remote feed");
        remote.latency = metrics.latency;
        remote.statuses.clear();
        self.board.models = metrics
            .models
            .into_iter()
            .map(|model| {
                let status = model.status.clone();
                let row = model.into_row();
                remote.statuses.insert(row.name.clone(), status);
                row
            })
            .collect();
        self.board
            .traffic
            .load(&remote.traffic.iter().copied().collect::<Vec<_>>());
    }

    /// The server sends flight updates when progress changes. Ages advance
    /// from receipt using a monotonic clock, independent of clock skew.
    pub fn advance_remote(&mut self) {
        let Some(remote) = &self.remote else { return };
        if !remote.connected || !self.flights_available {
            return;
        }
        let elapsed = remote.flights_at.elapsed().as_secs_f64();
        self.flights = remote
            .original_flights
            .iter()
            .cloned()
            .map(|mut flight| {
                flight.age += elapsed;
                flight.idle += elapsed;
                flight
            })
            .collect();
    }

    pub fn tick(&mut self, router: &Router) {
        self.board.tick(router);
        self.flights = router.telemetry().flights().views();
        self.flights_available = true;
    }

    pub fn tick_recorded(&mut self, window: &crate::watch::Window) {
        self.board.tick_recorded(window);
        self.flights.clear();
    }

    /// DB records have no flight IDs. Replace the live list as a whole, also
    /// across server restarts, and leave every historical counter untouched.
    pub fn apply_live(&mut self, snapshot: Option<&crate::live::Snapshot>) {
        self.flights_available = snapshot.is_some();
        self.flights = snapshot.map_or_else(Vec::new, |snapshot| snapshot.flights.clone());
        self.totals.in_flight = snapshot.map_or(0, |snapshot| snapshot.total);
        for model in &mut self.models {
            model.view.in_flight = snapshot
                .and_then(|snapshot| snapshot.upstreams.get(&model.name).copied())
                .unwrap_or(0);
        }
    }

    /// Keep the dashboard compact: active models first, then the most recent
    /// completed requests. Counters without retained events come last; unused
    /// configured routes do not occupy a row. Never reorder the full model
    /// list, which also drives event filtering and aggregate counters.
    pub fn recent_models(&self) -> Vec<&ModelRow> {
        let active = |row: &ModelRow| self.flights_available && row.view.in_flight > 0;
        let mut rows: Vec<_> = self
            .models
            .iter()
            .filter(|row| {
                active(row) || row.view.requests > 0 || self.model_recency.contains_key(&row.name)
            })
            .collect();
        rows.sort_by(|a, b| {
            active(b)
                .cmp(&active(a))
                .then_with(|| {
                    self.model_recency
                        .get(&b.name)
                        .cmp(&self.model_recency.get(&a.name))
                })
                .then_with(|| a.name.cmp(&b.name))
        });
        rows.truncate(3);
        rows
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
        if self.remote.is_some() {
            return;
        }
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
        if self.remote.is_some() {
            self.usage = None;
            self.usage_error = Some("loading remote usage…".into());
            return;
        }
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

    /// `all -> trouble` and then once through the routes.
    pub fn cycle_filter(&mut self) {
        let names: Vec<&str> = self
            .board
            .models
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        let next = match &self.filter {
            Filter::All => Filter::Trouble,
            Filter::Trouble => match names.first() {
                Some(name) => Filter::Upstream((*name).to_string()),
                None => Filter::All,
            },
            Filter::Upstream(current) => {
                let at = names.iter().position(|name| name == current);
                match at.and_then(|at| names.get(at + 1)) {
                    Some(name) => Filter::Upstream((*name).to_string()),
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

/// The view reads the numbers as if they were its own: `state.totals` is
/// `state.board.totals`. Inside this file the board is always named.
impl Deref for State {
    type Target = Board;
    fn deref(&self) -> &Board {
        &self.board
    }
}

impl DerefMut for State {
    fn deref_mut(&mut self) -> &mut Board {
        &mut self.board
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
