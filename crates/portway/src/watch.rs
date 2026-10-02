//! Watching a forwarder this process did not start.
//!
//! `--tui` takes the terminal and the listener together, so a second copy on a
//! port that is already serving cannot start one. Instead of failing it reads
//! what the running instance recorded: the tail of `db.sqlite3` replayed into
//! the same dashboard, plus a snapshot of the window for the one thing the
//! rows have to be asked for in bulk — the per-second bytes the traffic chart
//! draws.
//!
//! Read-only in the sense that matters: the file is opened through the
//! recorder's own path and nothing is ever written, migrated or created. A
//! window is all a database holds — a request has no row until its relay ends
//! — so what describes *now* (in flight, pooled idle connections, the coding
//! this process negotiated) is not here at all. The attached TUI supplements
//! these rows through the independent `live` socket client.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use http::Method;
use rusqlite::Connection;
use rusqlite::types::Value;

use crate::forwarder::Coding;
use crate::logfmt::{self, Level};
use crate::store;
use crate::telemetry::{Event, RequestRecord};
use crate::usage::Usage;

/// How much history the dashboard loads, and how wide its window then is: one
/// hour, which is exactly what the traffic chart holds.
pub const WINDOW: Duration = Duration::from_secs(3600);
/// How often the file is read again. An agent turn lasts seconds, so a second
/// of lag is invisible next to the request it is about to report.
const POLL: Duration = Duration::from_millis(1000);
/// A port that cannot answer within this is not a forwarder worth attaching to.
/// The connect itself is loopback, and a refused one comes back at once.
const PROBE: Duration = Duration::from_millis(500);

/// The recorded rows, most of them as the recorder wrote them. Token counts
/// only came at schema v2 and the named tier at v5, so the older layouts read
/// them back as NULL.
const REQUEST_COLUMNS: &str = "id, ts_unix, model, method, path, status,
    dns_ms, tcp_ms, tls_ms, body_len, wire_len, coding, upload_ms, ttfb_ms,
    received, received_wire, upstream_encoding, download_ms, complete,
    prompt_tokens, cached_tokens, completion_tokens, reasoning_tokens,
    received_agent, upstream, tier";
const REQUEST_COLUMNS_V4: &str = "id, ts_unix, model, method, path, status,
    dns_ms, tcp_ms, tls_ms, body_len, wire_len, coding, upload_ms, ttfb_ms,
    received, received_wire, upstream_encoding, download_ms, complete,
    prompt_tokens, cached_tokens, completion_tokens, reasoning_tokens,
    received_agent, upstream, NULL";
const REQUEST_COLUMNS_V3: &str = "id, ts_unix, model, method, path, status,
    dns_ms, tcp_ms, tls_ms, body_len, wire_len, coding, upload_ms, ttfb_ms,
    received, received_wire, upstream_encoding, download_ms, complete,
    prompt_tokens, cached_tokens, completion_tokens, reasoning_tokens,
    received_agent, model, NULL";
const REQUEST_COLUMNS_V2: &str = "id, ts_unix, model, method, path, status,
    dns_ms, tcp_ms, tls_ms, body_len, wire_len, coding, upload_ms, ttfb_ms,
    received, received_wire, upstream_encoding, download_ms, complete,
    prompt_tokens, cached_tokens, completion_tokens, reasoning_tokens, 0, model,
    NULL";
const REQUEST_COLUMNS_V1: &str = "id, ts_unix, model, method, path, status,
    dns_ms, tcp_ms, tls_ms, body_len, wire_len, coding, upload_ms, ttfb_ms,
    received, received_wire, upstream_encoding, download_ms, complete,
    NULL, NULL, NULL, NULL, 0, model, NULL";
const LOG_COLUMNS: &str = "id, ts_unix, level, message";

/// Bytes on the wire per second, which is the one shape only SQL can give: the
/// events carry sizes, not the second they happened in.
const SECONDS: &str = "SELECT CAST(ts_unix AS INTEGER), SUM(wire_len),
    SUM(received_wire)
  FROM requests WHERE ts_unix >= ?1 GROUP BY 1 ORDER BY 1";
/// How far back this view reaches: the oldest row still inside the window.
const OLDEST: &str = "SELECT MIN(ts_unix) FROM requests WHERE ts_unix >= ?1";

/// Whether a forwarder is already serving on this port — the thing that turns
/// `--tui` into watching instead of starting a second one.
///
/// One blocking probe, off the request path and off the runtime. Anything that
/// answers with the stats shape is a forwarder; anything else on the port is
/// somebody else's business, and the bind error downstream reports that better
/// than this can.
pub fn forwarder_on(host: &str, port: u16) -> bool {
    use std::io::{Read, Write};

    let host = match host {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "::1",
        other => other,
    };
    let Ok(ip) = host.trim_start_matches('[').trim_end_matches(']').parse() else {
        return false;
    };
    let address = std::net::SocketAddr::new(ip, port);
    let Ok(mut socket) = std::net::TcpStream::connect_timeout(&address, PROBE) else {
        return false;
    };
    let _ = socket.set_read_timeout(Some(PROBE));
    let _ = socket.set_write_timeout(Some(PROBE));
    let request = format!(
        "GET {stats} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n",
        stats = crate::router::STATS_PATH,
    );
    if socket.write_all(request.as_bytes()).is_err() {
        return false;
    }
    // Only enough to see a status line and the start of the JSON body.
    let mut answer = Vec::new();
    let mut chunk = [0u8; 512];
    while answer.len() < 4096 {
        match socket.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => answer.extend_from_slice(&chunk[..read]),
        }
    }
    let text = String::from_utf8_lossy(&answer);
    text.starts_with("HTTP/1.") && text.contains("\"upstreams\"")
}

/// What the dashboard's 250ms tick reads while it is not the one serving: the
/// traffic chart's own window, and how far back the rows on screen reach.
///
/// Everything else the dashboard shows arrives as replayed events, so the HUD
/// and the model table add up the same way they do in the live process.
#[derive(Clone, Default)]
pub struct Window {
    /// Bytes per second, `(up, down)`, oldest first.
    pub traffic: Vec<(u64, u64)>,
    /// Age of the oldest row still inside the window.
    pub coverage: Duration,
}

/// The reading side of a database: one connection, the schema's own column
/// list, and the row ids the tail picks up from.
struct Reader {
    connection: Connection,
    tokens: bool,
    agent: bool,
    upstream: bool,
    tier: bool,
}

/// What a row is read by: a window cutoff, or the id the last poll stopped at.
#[derive(Clone, Copy)]
enum Key {
    Since(f64),
    After(i64),
}

/// One row, read back as the event it was recorded from. The record is boxed
/// because a window is read whole: the rows pile up while the backfill is
/// replayed, and a log row has no business paying for a request's shape.
enum Stored {
    Request {
        id: i64,
        ts: f64,
        record: Box<RequestRecord>,
    },
    Log {
        id: i64,
        ts: f64,
        level: Level,
        message: String,
    },
}

impl Stored {
    fn id(&self) -> i64 {
        match self {
            Stored::Request { id, .. } | Stored::Log { id, .. } => *id,
        }
    }

    fn ts(&self) -> f64 {
        match self {
            Stored::Request { ts, .. } | Stored::Log { ts, .. } => *ts,
        }
    }

    fn event(self) -> Event {
        match self {
            Stored::Request { record, .. } => Event::Request(Arc::new(*record)),
            Stored::Log {
                ts, level, message, ..
            } => Event::Log {
                // The row's own second, not the one this replay happens in.
                stamp: logfmt::clock(ts),
                level,
                message,
            },
        }
    }
}

impl Reader {
    fn open(db: &Path) -> Result<Option<Reader>, String> {
        let Some(connection) = store::open_existing(db)? else {
            return Ok(None);
        };
        let tokens = store::has_token_columns(&connection)?;
        let agent = store::has_agent_column(&connection)?;
        let upstream = store::has_upstream_column(&connection)?;
        let tier = store::has_tier_column(&connection)?;
        Ok(Some(Reader {
            connection,
            tokens,
            agent,
            upstream,
            tier,
        }))
    }

    /// Every row recorded at or after `cutoff`, both tables.
    fn since(&self, cutoff: f64) -> Result<(Vec<Stored>, Vec<Stored>), String> {
        Ok((
            self.requests(Key::Since(cutoff))?,
            self.logs(Key::Since(cutoff))?,
        ))
    }

    /// What arrived since the last poll, by row id.
    fn tail(&self, request_id: i64, log_id: i64) -> Result<(Vec<Stored>, Vec<Stored>), String> {
        Ok((
            self.requests(Key::After(request_id))?,
            self.logs(Key::After(log_id))?,
        ))
    }

    fn requests(&self, key: Key) -> Result<Vec<Stored>, String> {
        let columns = match (self.tokens, self.agent, self.upstream, self.tier) {
            (true, true, true, true) => REQUEST_COLUMNS,
            (true, true, true, false) => REQUEST_COLUMNS_V4,
            (true, true, false, _) => REQUEST_COLUMNS_V3,
            (true, false, _, _) => REQUEST_COLUMNS_V2,
            _ => REQUEST_COLUMNS_V1,
        };
        let sql = read("requests", columns, key);
        let param = match key {
            Key::Since(cutoff) => Value::Real(cutoff),
            Key::After(id) => Value::Integer(id),
        };
        let mut statement = self.connection.prepare(&sql).map_err(db_error)?;
        let rows = statement
            .query_map([param], |row| {
                let ts: f64 = row.get(1)?;
                let prompt: Option<i64> = row.get(19)?;
                let completion: Option<i64> = row.get(21)?;
                Ok(Stored::Request {
                    id: row.get(0)?,
                    ts,
                    record: Box::new(RequestRecord {
                        stamp: logfmt::clock(ts),
                        upstream: row.get(24)?,
                        model: row.get(2)?,
                        tier: row.get(25)?,
                        method: Method::from_bytes(row.get::<_, String>(3)?.as_bytes())
                            .unwrap_or(Method::POST),
                        path: row.get(4)?,
                        status: row.get::<_, i64>(5)? as u16,
                        dns: seconds(row.get(6)?),
                        tcp: seconds(row.get(7)?),
                        tls: seconds(row.get(8)?),
                        body_len: row.get::<_, i64>(9)? as u64,
                        wire_len: row.get::<_, i64>(10)? as u64,
                        coding: Coding::from_stored(&row.get::<_, String>(11)?),
                        upload: seconds(row.get(12)?),
                        ttfb: row.get::<_, f64>(13)? / 1000.0,
                        received: row.get::<_, i64>(14)? as u64,
                        received_wire: row.get::<_, i64>(15)? as u64,
                        received_agent: row.get::<_, i64>(23)? as u64,
                        upstream_encoding: row.get(16)?,
                        agent_encoding: None,
                        download: seconds(row.get(17)?),
                        complete: row.get::<_, i64>(18)? != 0,
                        // Both counts or neither, the same rule the scanner
                        // applies to what the engine reported.
                        usage: match (prompt, completion) {
                            (Some(prompt), Some(completion)) => Some(Usage {
                                prompt: prompt as u64,
                                cached: row.get::<_, Option<i64>>(20)?.map(|n| n as u64),
                                completion: completion as u64,
                                reasoning: row.get::<_, Option<i64>>(22)?.map(|n| n as u64),
                            }),
                            _ => None,
                        },
                        flight: None,
                    }),
                })
            })
            .map_err(db_error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)
    }

    fn logs(&self, key: Key) -> Result<Vec<Stored>, String> {
        let sql = read("logs", LOG_COLUMNS, key);
        let param = match key {
            Key::Since(cutoff) => Value::Real(cutoff),
            Key::After(id) => Value::Integer(id),
        };
        let mut statement = self.connection.prepare(&sql).map_err(db_error)?;
        let rows = statement
            .query_map([param], |row| {
                Ok(Stored::Log {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    level: Level::from_stored(row.get(2)?),
                    message: row.get(3)?,
                })
            })
            .map_err(db_error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)
    }

    /// The per-second bytes of the window, oldest first, one bucket per second
    /// ending with the one this sample was taken in.
    fn traffic(&self, cutoff: f64, seconds: usize) -> Result<Vec<(u64, u64)>, String> {
        let last = logfmt::epoch() as u64;
        let first = last - (seconds as u64 - 1);
        let mut buckets = vec![(0u64, 0u64); seconds];
        let mut statement = self.connection.prepare(SECONDS).map_err(db_error)?;
        let rows = statement
            .query_map([cutoff], |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            })
            .map_err(db_error)?;
        for row in rows {
            let (second, up, down) = row.map_err(db_error)?;
            match second.checked_sub(first) {
                Some(at) if (at as usize) < seconds => buckets[at as usize] = (up, down),
                _ => {}
            }
        }
        Ok(buckets)
    }

    /// How far back the window reaches, in seconds.
    fn coverage(&self, cutoff: f64) -> Result<Duration, String> {
        let oldest: Option<f64> = self
            .connection
            .query_row(OLDEST, [cutoff], |row| row.get(0))
            .map_err(db_error)?;
        Ok(oldest
            .map(|oldest| {
                Duration::from_secs_f64((logfmt::epoch() - oldest).clamp(0.0, WINDOW.as_secs_f64()))
            })
            .unwrap_or_default())
    }

    /// The highest row id in each table, or 0 for a table with nothing in it.
    ///
    /// The tail is keyed on those ids, and SQLite hands out the ids a table
    /// freed once it is empty: a retention pass that prunes the last row would
    /// otherwise leave a viewer waiting for ids that are never coming again.
    /// Asked every poll so the two ways of seeing nothing — "nothing new" and
    /// "what we were reading is gone" — can be told apart.
    fn latest(&self) -> Result<(i64, i64), String> {
        let high = |table: &str| -> Result<i64, String> {
            self.connection
                .query_row(&format!("SELECT MAX(id) FROM {table}"), [], |row| {
                    Ok(row.get::<_, Option<i64>>(0)?.unwrap_or(0))
                })
                .map_err(db_error)
        };
        Ok((high("requests")?, high("logs")?))
    }
}

fn seconds(ms: Option<f64>) -> Option<f64> {
    ms.map(|value| value / 1000.0)
}

/// A window read of one table: in the window, or after the last row seen —
/// both ordered by the id the recorder assigns, which is the arrival order the
/// dashboard's own sequence numbers shadow.
fn read(table: &str, columns: &str, key: Key) -> String {
    let by = match key {
        Key::Since(_) => "ts_unix >= ?1",
        Key::After(_) => "id > ?1",
    };
    format!("SELECT {columns} FROM {table} WHERE {by} ORDER BY id")
}

fn db_error(err: rusqlite::Error) -> String {
    format!("sqlite: {err}")
}

/// The poller: backfill once, then hand the dashboard whatever is new and
/// resample the window.
pub struct Watch {
    window: Arc<Mutex<Window>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watch {
    /// The window the render loop samples every 250ms.
    pub fn window(&self) -> Arc<Mutex<Window>> {
        Arc::clone(&self.window)
    }

    /// Stop polling and join the thread.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start reading `db`, with the last [`WINDOW`] of it handed to `events` before
/// this returns: the dashboard's first frame already has the hour on it.
///
/// A database that is not there yet is not an error — the forwarder this is
/// watching may be about to record for the first time — and it is reported on
/// the dashboard rather than to the terminal that is about to be taken over.
pub fn spawn(db: &Path, events: Sender<Event>) -> Result<Watch, String> {
    let reader = Reader::open(db)?;
    let start = Instant::now();
    let (mut requests, mut logs) = match &reader {
        Some(reader) => {
            let cutoff = logfmt::epoch() - WINDOW.as_secs_f64();
            reader.since(cutoff)?
        }
        None => (Vec::new(), Vec::new()),
    };
    // Both reads are ordered by the id the recorder assigned, so the last row
    // of each is where the tail picks up from.
    let request_id = requests.last().map(Stored::id).unwrap_or(0);
    let log_id = logs.last().map(Stored::id).unwrap_or(0);
    // The two tables are recorded apart but happened together: one replay.
    requests.append(&mut logs);
    requests.sort_by(|left, right| left.ts().total_cmp(&right.ts()));
    for row in requests {
        if events.send(row.event()).is_err() {
            break;
        }
    }

    let window = Arc::new(Mutex::new(Window::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = std::thread::Builder::new()
        .name("watch".to_string())
        .spawn({
            let window = Arc::clone(&window);
            let stop = Arc::clone(&stop);
            move || poll(reader, events, &window, &stop, start, request_id, log_id)
        })
        .map_err(|err| format!("watch thread: {err}"))?;
    Ok(Watch {
        window,
        stop,
        thread: Some(thread),
    })
}

/// Re-read the window: the per-second bytes the traffic chart draws, and how
/// far back the rows inside it reach.
fn sample(reader: &Reader, window: &Mutex<Window>) {
    let cutoff = logfmt::epoch() - WINDOW.as_secs_f64();
    let sample = reader
        .traffic(cutoff, WINDOW.as_secs() as usize)
        .and_then(|traffic| {
            reader
                .coverage(cutoff)
                .map(|coverage| Window { traffic, coverage })
        });
    match sample {
        Ok(fresh) => *window.lock().unwrap() = fresh,
        Err(message) => logfmt::error(&format!("watching: {message}")),
    }
}

fn poll(
    reader: Option<Reader>,
    events: Sender<Event>,
    window: &Mutex<Window>,
    stop: &AtomicBool,
    start: Instant,
    mut request_id: i64,
    mut log_id: i64,
) {
    let Some(reader) = reader else {
        // Nothing recorded, and a reader may not create what it reads. Said
        // once, on the screen the caller is about to look at.
        logfmt::warn("no database to read yet: is the forwarder using the same --data-dir?");
        while !stop.load(Ordering::Acquire) {
            std::thread::sleep(POLL);
        }
        return;
    };

    // Sampled before the first sleep: the dashboard's first frame then has the
    // hour on it, not an empty chart the poll would fill a second later.
    sample(&reader, window);
    let mut last = start;
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(POLL.saturating_sub(last.elapsed()));
        last = Instant::now();

        // Never forward, only back: the ids can be smaller than the ones we
        // remember only when the rows they named are gone.
        match reader.latest() {
            Ok((requests, logs)) => {
                request_id = request_id.min(requests);
                log_id = log_id.min(logs);
            }
            Err(message) => {
                logfmt::error(&format!("watching: {message}"));
                continue;
            }
        }

        let (mut fresh, mut fresh_logs) = match reader.tail(request_id, log_id) {
            Ok(rows) => rows,
            Err(message) => {
                // A failed read is a dashboard line, never the end of the
                // process: the file may be mid-checkpoint, or briefly locked
                // by a checkpointing writer.
                logfmt::error(&format!("watching: {message}"));
                continue;
            }
        };
        if let Some(row) = fresh.last() {
            request_id = row.id();
        }
        if let Some(row) = fresh_logs.last() {
            log_id = row.id();
        }
        fresh.append(&mut fresh_logs);
        if !fresh.is_empty() {
            fresh.sort_by(|left, right| left.ts().total_cmp(&right.ts()));
            for row in fresh {
                if events.send(row.event()).is_err() {
                    return; // the dashboard is gone
                }
            }
        }

        sample(&reader, window);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// One directory per test, the same rule the recorder's tests keep.
    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("portway-watch-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn record(record: RequestRecord) -> Event {
        Event::Request(Arc::new(record))
    }

    fn request(stamp: &str) -> RequestRecord {
        RequestRecord {
            stamp: stamp.to_string(),
            upstream: "model-alpha".to_string(),
            model: "model-alpha".to_string(),
            tier: Some("tier-a".to_string()),
            method: Method::POST,
            path: "/v1/chat/completions".to_string(),
            status: 200,
            dns: Some(0.001),
            tcp: Some(0.029),
            tls: Some(0.047),
            body_len: 700_000,
            wire_len: 210_000,
            coding: Coding::Zstd,
            upload: Some(0.012),
            ttfb: 17.44,
            received: 521,
            received_wire: 98,
            received_agent: 40,
            upstream_encoding: "gzip".to_string(),
            agent_encoding: None,
            download: Some(0.018),
            complete: true,
            usage: Some(Usage {
                prompt: 18_234,
                cached: Some(18_200),
                completion: 891,
                reasoning: Some(742),
            }),
            flight: None,
        }
    }

    /// The whole point of the reader: what the recorder wrote comes back as the
    /// record the dashboard would have drawn live, down to the microseconds and
    /// the engine's own counts.
    #[test]
    fn a_recorded_row_reads_back_as_the_record_that_wrote_it() {
        let dir = dir("roundtrip");
        let recorder = store::spawn(&dir, 0).unwrap();
        recorder.sender().send(record(request("23:41:02"))).unwrap();
        recorder.shutdown();

        let reader = Reader::open(&dir.join(store::DB_FILE)).unwrap().unwrap();
        let (rows, logs) = reader.since(0.0).unwrap();
        assert!(logs.is_empty());
        assert_eq!(rows.len(), 1);
        let Stored::Request { record, .. } = &rows[0] else {
            panic!("a request row read back as a log row");
        };
        let mut written = request("23:41:02");
        // The stamp is the row's own clock, not the one the relay stamped: a
        // batch can reach the disk a moment after the answer did.
        written.stamp.clone_from(&record.stamp);
        assert_eq!(record.model, written.model);
        assert_eq!(record.tier, written.tier);
        assert_eq!(record.method, written.method);
        assert_eq!(record.path, written.path);
        assert_eq!(record.status, written.status);
        assert_eq!(record.dns, written.dns);
        assert_eq!(record.tcp, written.tcp);
        assert_eq!(record.tls, written.tls);
        assert_eq!(record.body_len, written.body_len);
        assert_eq!(record.wire_len, written.wire_len);
        assert_eq!(record.coding, written.coding);
        assert_eq!(record.upload, written.upload);
        assert_eq!(record.ttfb, written.ttfb);
        assert_eq!(record.received, written.received);
        assert_eq!(record.received_wire, written.received_wire);
        assert_eq!(record.upstream_encoding, written.upstream_encoding);
        assert_eq!(record.download, written.download);
        assert_eq!(record.complete, written.complete);
        assert_eq!(record.usage, written.usage);
    }

    /// A row that reported no tokens comes back with none: the four columns
    /// stay NULL, and the popup has nothing to print.
    #[test]
    fn a_row_without_counts_reads_back_without_them() {
        let dir = dir("no-usage");
        let recorder = store::spawn(&dir, 0).unwrap();
        let mut quiet = request("23:41:02");
        quiet.usage = None;
        recorder.sender().send(record(quiet)).unwrap();
        recorder.shutdown();

        let reader = Reader::open(&dir.join(store::DB_FILE)).unwrap().unwrap();
        let (rows, _) = reader.since(0.0).unwrap();
        let Stored::Request { record, .. } = &rows[0] else {
            panic!("a request row read back as a log row");
        };
        assert!(record.usage.is_none());
    }

    /// A log row is replayed as its own kind of entry, at its own level — the
    /// events pane shows the run's negotiation and trouble line for line.
    #[test]
    fn a_recorded_line_reads_back_with_its_level() {
        let dir = dir("logs");
        let recorder = store::spawn(&dir, 0).unwrap();
        recorder
            .sender()
            .send(Event::Log {
                stamp: "23:41:02".to_string(),
                level: Level::Warning,
                message: "415 for zstd: resending identity".to_string(),
            })
            .unwrap();
        recorder.shutdown();

        let reader = Reader::open(&dir.join(store::DB_FILE)).unwrap().unwrap();
        let (requests, logs) = reader.since(0.0).unwrap();
        assert!(requests.is_empty());
        assert_eq!(logs.len(), 1);
        match logs.into_iter().next().unwrap().event() {
            Event::Log { level, message, .. } => {
                assert_eq!(level, Level::Warning);
                assert_eq!(message, "415 for zstd: resending identity");
            }
            Event::Request(_) => panic!("a log row replayed as a request"),
        }
    }

    /// Only what is new: the tail is keyed on the row ids, so a poll never
    /// replays a line the dashboard already has.
    #[test]
    fn the_tail_only_carries_what_arrived_since_the_last_poll() {
        let dir = dir("tail");
        let recorder = store::spawn(&dir, 0).unwrap();
        let sender = recorder.sender();
        sender.send(record(request("23:41:02"))).unwrap();
        sender
            .send(Event::Log {
                stamp: "23:41:03".to_string(),
                level: Level::Info,
                message: "first".to_string(),
            })
            .unwrap();
        recorder.shutdown();

        let reader = Reader::open(&dir.join(store::DB_FILE)).unwrap().unwrap();
        let (requests, logs) = reader.since(0.0).unwrap();
        let request_id = requests[0].id();
        let log_id = logs[0].id();
        let (fresh, fresh_logs) = reader.tail(request_id, log_id).unwrap();
        assert!(fresh.is_empty());
        assert!(fresh_logs.is_empty());

        let (rows, _) = reader.since(0.0).unwrap();
        assert_eq!(rows.len(), 1);
    }

    /// A file an older build is still writing to has no token columns; a reader
    /// may not migrate it, so it reads them as absent.
    #[test]
    fn a_version_one_file_reads_without_its_token_columns() {
        let dir = dir("v1");
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join(store::DB_FILE);
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE requests (
                   id INTEGER PRIMARY KEY, ts_unix REAL NOT NULL, model TEXT NOT NULL,
                   method TEXT NOT NULL, path TEXT NOT NULL, status INTEGER NOT NULL,
                   dns_ms REAL, tcp_ms REAL, tls_ms REAL, body_len INTEGER NOT NULL,
                   wire_len INTEGER NOT NULL, coding TEXT NOT NULL, upload_ms REAL,
                   ttfb_ms REAL NOT NULL, received INTEGER NOT NULL,
                   received_wire INTEGER NOT NULL, upstream_encoding TEXT NOT NULL,
                   download_ms REAL, complete INTEGER NOT NULL);
                 CREATE TABLE logs (
                   id INTEGER PRIMARY KEY, ts_unix REAL NOT NULL, level INTEGER NOT NULL,
                   message TEXT NOT NULL);
                 PRAGMA user_version = 1;
                 INSERT INTO requests (ts_unix, model, method, path, status, body_len,
                   wire_len, coding, ttfb_ms, received, received_wire,
                   upstream_encoding, complete)
                 VALUES (strftime('%s','now'), 'model-alpha', 'POST',
                   '/v1/chat/completions', 200, 700, 210, 'zstd', 17440, 521, 98,
                   'gzip', 1);",
            )
            .unwrap();
        drop(connection);

        let reader = Reader::open(&db).unwrap().unwrap();
        assert!(!reader.tokens);
        let (rows, _) = reader.since(0.0).unwrap();
        let Stored::Request { record, .. } = &rows[0] else {
            panic!("a request row read back as a log row");
        };
        assert_eq!(record.model, "model-alpha");
        // Before v4 the model column was the route: it reads as both.
        assert_eq!(record.upstream, "model-alpha");
        assert_eq!(record.coding, Coding::Zstd);
        assert_eq!(record.ttfb, 17.44);
        assert!(record.usage.is_none());
    }

    /// The chart is drawn from the rows' own seconds, right up to the second
    /// the sample was taken in.
    #[test]
    fn the_window_lands_a_row_in_its_own_second() {
        let dir = dir("seconds");
        let start = logfmt::epoch() as u64;
        let recorder = store::spawn(&dir, 0).unwrap();
        recorder.sender().send(record(request("23:41:02"))).unwrap();
        recorder.shutdown();

        let reader = Reader::open(&dir.join(store::DB_FILE)).unwrap().unwrap();
        let buckets = reader
            .traffic(logfmt::epoch() - WINDOW.as_secs_f64(), 60)
            .unwrap();
        let lag = (logfmt::epoch() as u64 - start) as usize;
        assert_eq!(buckets.len(), 60);
        let up: u64 = buckets.iter().map(|(up, _)| up).sum();
        let down: u64 = buckets.iter().map(|(_, down)| down).sum();
        assert_eq!(up, 210_000);
        assert_eq!(down, 98);
        // The row's second is the last one the window ends on, or as many
        // before it as the clock moved on while this test read it back.
        let at = buckets
            .iter()
            .rposition(|bucket| *bucket != (0, 0))
            .unwrap();
        assert_eq!(buckets[at], (210_000, 98));
        assert!(buckets.len() - 1 - at <= lag, "row {at} of 60, {lag}s late");
    }

    /// The probe is the whole attach decision: a forwarder answers with the
    /// stats shape, a stranger on the port does not, and an empty port is
    /// `false` either way.
    #[test]
    fn only_a_forwarder_answers_the_probe() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let thread = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = std::io::Read::read(&mut socket, &mut request);
            let _ = std::io::Write::write_all(
                &mut socket,
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"upstreams\":{\"model-zeta\":{}}}",
            );
        });
        assert!(forwarder_on("127.0.0.1", port));
        thread.join().unwrap();

        let stranger = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = stranger.local_addr().unwrap().port();
        let thread = std::thread::spawn(move || {
            let (mut socket, _) = stranger.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = std::io::Read::read(&mut socket, &mut request);
            let _ = std::io::Write::write_all(&mut socket, b"HTTP/1.1 404 Not Found\r\n\r\n");
        });
        assert!(!forwarder_on("127.0.0.1", port));
        thread.join().unwrap();

        // Nothing listening: the refused connect decides it, and at once.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        assert!(!forwarder_on("127.0.0.1", port));
    }

    /// A prune that empties the tables frees the row ids the tail was keyed on:
    /// the reader has to notice, or it waits forever for ids that will not come
    /// back.
    #[test]
    fn an_emptied_table_reports_no_row_to_pick_up_from() {
        let dir = dir("pruned");
        let recorder = store::spawn(&dir, 0).unwrap();
        recorder.sender().send(record(request("23:41:02"))).unwrap();
        recorder
            .sender()
            .send(Event::Log {
                stamp: "23:41:03".to_string(),
                level: Level::Info,
                message: "first".to_string(),
            })
            .unwrap();
        recorder.shutdown();

        let reader = Reader::open(&dir.join(store::DB_FILE)).unwrap().unwrap();
        let (requests, logs) = reader.latest().unwrap();
        assert_eq!(requests, 1);
        assert_eq!(logs, 1);

        // What `--retention-days` does to a file nothing is writing to.
        reader
            .connection
            .execute_batch("DELETE FROM requests; DELETE FROM logs;")
            .unwrap();
        assert_eq!(reader.latest().unwrap(), (0, 0));
        let (fresh, fresh_logs) = reader.tail(0, 0).unwrap();
        assert!(fresh.is_empty() && fresh_logs.is_empty());
    }
}
