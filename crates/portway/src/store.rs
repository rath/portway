//! Everything that touches `db.sqlite3`: the data directory, the schema, and
//! the one thread that writes to it.
//!
//! The request path never waits for a disk. `telemetry::emit` hands the event
//! to a bounded channel and moves on; the thread below owns the process's only
//! connection, so it never touches the tokio runtime and a report can read the
//! file while the forwarder writes it.
//!
//! What lands here is exactly what the log line already carried — sizes,
//! timings, statuses, model names, token counts. Bodies, headers and keys are
//! never written, the same invariant the log line has.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, Transaction, params};

use crate::logfmt::{self, Level};
use crate::telemetry::Event;

pub const DB_FILE: &str = "db.sqlite3";
pub const PID_FILE: &str = "portway.pid";
pub const LOG_FILE: &str = "portway.log";

/// Bounded on purpose: a queue that can grow without bound is a leak in a
/// process that outlives agent sessions by design.
const QUEUE: usize = 4096;
/// How long the writer waits for the first event of a batch.
const POLL: Duration = Duration::from_millis(200);
const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SCHEMA_VERSION: i64 = 5;

/// `ts_unix` is the epoch second the row reached the store, not the request's
/// own clock: the writer's clock is the one the window is measured against.
///
/// The four `*_tokens` columns are what the upstream reported for this
/// answer, and stay NULL on an answer that reported nothing — an engine that
/// was not asked for usage, or a stream that was cut short before its last
/// chunk. They are the only numbers here that came from the engine rather than
/// from the wire.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS requests (
  id INTEGER PRIMARY KEY,
  ts_unix REAL NOT NULL,
  model TEXT NOT NULL,
  method TEXT NOT NULL,
  path TEXT NOT NULL,
  status INTEGER NOT NULL,
  dns_ms REAL,
  tcp_ms REAL,
  tls_ms REAL,
  body_len INTEGER NOT NULL,
  wire_len INTEGER NOT NULL,
  coding TEXT NOT NULL,
  upload_ms REAL,
  ttfb_ms REAL NOT NULL,
  received INTEGER NOT NULL,
  received_wire INTEGER NOT NULL,
  upstream_encoding TEXT NOT NULL,
  download_ms REAL,
  complete INTEGER NOT NULL,
  prompt_tokens INTEGER,
  cached_tokens INTEGER,
  completion_tokens INTEGER,
  reasoning_tokens INTEGER,
  received_agent INTEGER NOT NULL DEFAULT 0,
  upstream TEXT NOT NULL DEFAULT '',
  tier TEXT
);
CREATE INDEX IF NOT EXISTS requests_ts ON requests(ts_unix);
CREATE TABLE IF NOT EXISTS logs (
  id INTEGER PRIMARY KEY,
  ts_unix REAL NOT NULL,
  level INTEGER NOT NULL,
  message TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS logs_ts ON logs(ts_unix);
";

/// v1 -> v2: the token counts. `ALTER TABLE` appends, so a file that came up
/// through here has the same layout as one created at v2, and every row
/// recorded before the upgrade keeps NULLs.
const MIGRATE_V1: &str = "
ALTER TABLE requests ADD COLUMN prompt_tokens INTEGER;
ALTER TABLE requests ADD COLUMN cached_tokens INTEGER;
ALTER TABLE requests ADD COLUMN completion_tokens INTEGER;
ALTER TABLE requests ADD COLUMN reasoning_tokens INTEGER;
";

/// v2 -> v3: the agent leg's own size. `ALTER TABLE` appends here too, and a
/// row written by an older build reads as `0`, which the dashboard shows as
/// "no figure" rather than "nothing was sent".
const MIGRATE_V2: &str = "
ALTER TABLE requests ADD COLUMN received_agent INTEGER NOT NULL DEFAULT 0;
";

/// v3 -> v4: the route apart from the model. Until here `model` held the
/// route's name — `upstream` in single-upstream mode, whatever the request
/// asked for — so the old column is copied into the new one, and `model`
/// keeps what it had: for a `[models]` route that is the model, and for a
/// single upstream it is the word `upstream`, which no later row repeats.
const MIGRATE_V3: &str = "
ALTER TABLE requests ADD COLUMN upstream TEXT NOT NULL DEFAULT '';
UPDATE requests SET upstream = model;
";

/// v4 -> v5: the tier the request named (its `speed`, or else its
/// `service_tier`), NULL where it named none.
/// Every row recorded before the upgrade reads as having named none, so it is
/// priced at its model's standard rates whatever class it actually ran in.
const MIGRATE_V4: &str = "
ALTER TABLE requests ADD COLUMN tier TEXT;
";

const INSERT_REQUEST: &str = "INSERT INTO requests (
  ts_unix, model, method, path, status, dns_ms, tcp_ms, tls_ms, body_len,
  wire_len, coding, upload_ms, ttfb_ms, received, received_wire,
  upstream_encoding, download_ms, complete, prompt_tokens, cached_tokens,
  completion_tokens, reasoning_tokens, received_agent, upstream, tier
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

const INSERT_LOG: &str = "INSERT INTO logs (ts_unix, level, message) VALUES (?, ?, ?)";

/// `$XDG_CONFIG_HOME/portway`, or `$HOME/.config/portway`
/// (which is what the documented path resolves to). `--data-dir` wins.
pub fn data_dir(override_dir: Option<&Path>) -> Result<PathBuf, String> {
    data_dir_from(
        override_dir,
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// The environment, passed in rather than read, so the rules below are
/// testable without touching a process-wide variable.
fn data_dir_from(
    override_dir: Option<&Path>,
    xdg: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(dir) = override_dir {
        return Ok(dir.to_path_buf());
    }
    // A relative XDG_CONFIG_HOME is ignored, per the spec.
    if let Some(base) = xdg
        && base.is_absolute()
    {
        return Ok(base.join("portway"));
    }
    match home {
        Some(home) => Ok(home.join(".config").join("portway")),
        None => Err("no HOME set: pass --data-dir".to_string()),
    }
}

/// `create_dir_all`, with 0700 on everything it creates — the database holds
/// the paths an agent asked for and how long they took.
pub fn ensure_dir(dir: &Path) -> Result<(), String> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|err| format!("--data-dir {}: {err}", dir.display()))
}

/// Open `<dir>/db.sqlite3`, creating the data dir and the schema when they are
/// new, and start the writer thread. A forwarder asked to record must not run
/// unrecorded, so this fails before the listener is bound.
pub fn spawn(dir: &Path, retention_days: u32) -> Result<Store, String> {
    ensure_dir(dir)?;
    let connection = open_create(&dir.join(DB_FILE))?;
    let (sender, receiver) = sync_channel(QUEUE);
    let stop = Arc::new(AtomicBool::new(false));
    let thread = std::thread::Builder::new()
        .name("store".to_string())
        .spawn({
            let stop = Arc::clone(&stop);
            move || writer(connection, &receiver, &stop, retention_days)
        })
        .map_err(|err| format!("store thread: {err}"))?;
    Ok(Store {
        sender,
        stop,
        thread: Some(thread),
    })
}

pub struct Store {
    sender: SyncSender<Event>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Store {
    /// A clone of the recorder's channel; `telemetry::Sinks` carries it.
    pub fn sender(&self) -> SyncSender<Event> {
        self.sender.clone()
    }

    /// Drain what is queued, commit, join the thread.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Open the recorded file for reading. Returns `None` when there is nothing
/// recorded yet — no file, or a file without the schema — and never creates or
/// migrates what it finds.
pub fn open_existing(db: &Path) -> Result<Option<Connection>, String> {
    if !db.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|err| format!("{}: {err}", db.display()))?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(|err| format!("{}: {err}", db.display()))?;
    if schema_version(&connection)? == 0 {
        return Ok(None);
    }
    Ok(Some(connection))
}

fn open_create(db: &Path) -> Result<Connection, String> {
    restrict(db).map_err(|err| format!("{}: {err}", db.display()))?;
    let mut connection = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .map_err(|err| format!("{}: {err}", db.display()))?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(|err| format!("{}: {err}", db.display()))?;
    // `journal_mode` answers with a row, so it cannot go through
    // `pragma_update`; the answer is the mode that ended up in effect.
    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|err| format!("{}: {err}", db.display()))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(format!("{}: journal_mode = {mode}, not WAL", db.display()));
    }
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(|err| format!("{}: {err}", db.display()))?;

    let version = schema_version(&connection)?;
    if version > SCHEMA_VERSION {
        return Err(format!(
            "{} was written by a newer portway (schema {version}, this build reads {SCHEMA_VERSION})",
            db.display()
        ));
    }
    if version < SCHEMA_VERSION {
        // One transaction: the tables and the version that announces them move
        // together, so a file caught halfway is left at the version it was.
        let tx = connection
            .transaction()
            .map_err(|err| format!("{}: {err}", db.display()))?;
        let steps = match version {
            0 => SCHEMA.to_string(),
            1 => format!("{MIGRATE_V1}{MIGRATE_V2}{MIGRATE_V3}{MIGRATE_V4}"),
            2 => format!("{MIGRATE_V2}{MIGRATE_V3}{MIGRATE_V4}"),
            3 => format!("{MIGRATE_V3}{MIGRATE_V4}"),
            _ => MIGRATE_V4.to_string(),
        };
        tx.execute_batch(&steps)
            .map_err(|err| format!("{}: {err}", db.display()))?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|err| format!("{}: {err}", db.display()))?;
        tx.commit()
            .map_err(|err| format!("{}: {err}", db.display()))?;
    }
    Ok(connection)
}

/// Owner-only access for the database and any WAL sidecars already on disk.
///
/// `ensure_dir` only tightens a directory it creates, and `--data-dir` may name
/// one that is already open to the group or the world. The file is created 0600
/// before SQLite sees it, so it is never readable under the umask's mode even
/// briefly; an existing file is narrowed in place. SQLite gives the `-wal` and
/// `-shm` files it creates the database file's mode, so only sidecars left by
/// an earlier build need the second step.
fn restrict(db: &Path) -> io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(db)?;
    let owner_only = fs::Permissions::from_mode(0o600);
    fs::set_permissions(db, owner_only.clone())?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db.as_os_str().to_owned();
        sidecar.push(suffix);
        match fs::set_permissions(&sidecar, owner_only.clone()) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
    }
    Ok(())
}

fn schema_version(connection: &Connection) -> Result<i64, String> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|err| format!("user_version: {err}"))
}

/// Whether the file carries the four token columns. A reader that selects them
/// has to fall back on `NULL`s for anything older — a run of this build against
/// a database some older build is still writing to, which is never migrated
/// because a reader may not write.
pub fn has_token_columns(connection: &Connection) -> Result<bool, String> {
    const TOKENS_SINCE: i64 = 2;
    Ok(schema_version(connection)? >= TOKENS_SINCE)
}

/// Whether the file carries the agent leg's own size. Same rule as the token
/// columns: a reader of an older file selects `0` instead of the column.
pub fn has_agent_column(connection: &Connection) -> Result<bool, String> {
    const AGENT_SINCE: i64 = 3;
    Ok(schema_version(connection)? >= AGENT_SINCE)
}

/// Whether the file keeps the route apart from the model. A reader of an
/// older file takes `model` for both, which is what that column was.
pub fn has_upstream_column(connection: &Connection) -> Result<bool, String> {
    const UPSTREAM_SINCE: i64 = 4;
    Ok(schema_version(connection)? >= UPSTREAM_SINCE)
}

/// Whether the file keeps the tier a request named. A reader of an
/// older file selects `NULL`, which is what the upgrade gives those rows.
pub fn has_tier_column(connection: &Connection) -> Result<bool, String> {
    const TIER_SINCE: i64 = 5;
    Ok(schema_version(connection)? >= TIER_SINCE)
}

/// The writer thread. Nothing here is allowed to end the process: a disk that
/// stops answering must degrade the record, not the proxy.
fn writer(
    mut connection: Connection,
    rx: &Receiver<Event>,
    stop: &AtomicBool,
    retention_days: u32,
) {
    // `None` is "not pruned yet", so `--retention-days` applies at startup and
    // every 24h after it.
    let mut last_prune: Option<Instant> = None;
    loop {
        let batch = collect(rx, POLL);
        if !batch.is_empty() {
            write_batch(&mut connection, &batch);
        }
        if retention_days > 0 && last_prune.is_none_or(|at| at.elapsed() >= PRUNE_EVERY) {
            prune(&connection, retention_days);
            last_prune = Some(Instant::now());
        }
        if stop.load(Ordering::Acquire) {
            let batch = collect(rx, Duration::ZERO);
            if !batch.is_empty() {
                write_batch(&mut connection, &batch);
            }
            return;
        }
    }
}

/// The first event, waiting up to `timeout`, then whatever else is already
/// queued.
fn collect(rx: &Receiver<Event>, timeout: Duration) -> Vec<Event> {
    let mut batch = Vec::new();
    match rx.recv_timeout(timeout) {
        Ok(event) => batch.push(event),
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return batch,
    }
    while let Ok(event) = rx.try_recv() {
        batch.push(event);
    }
    batch
}

/// One transaction per batch: a report reading the file sees whole batches,
/// and the cost of a commit is not paid per event.
fn write_batch(connection: &mut Connection, batch: &[Event]) {
    let tx = match connection.transaction() {
        Ok(tx) => tx,
        Err(err) => return failure(&err),
    };
    if let Err(err) = insert(&tx, batch).and_then(|()| tx.commit()) {
        failure(&err);
    }
}

fn insert(tx: &Transaction<'_>, batch: &[Event]) -> rusqlite::Result<()> {
    let mut request = tx.prepare(INSERT_REQUEST)?;
    let mut log = tx.prepare(INSERT_LOG)?;
    for event in batch {
        match event {
            Event::Request(record) => {
                request.execute(params![
                    logfmt::epoch(),
                    record.model,
                    record.method.as_str(),
                    record.path,
                    i64::from(record.status),
                    ms(record.dns),
                    ms(record.tcp),
                    ms(record.tls),
                    record.body_len as i64,
                    record.wire_len as i64,
                    record.coding.name().unwrap_or("identity"),
                    ms(record.upload),
                    record.ttfb * 1000.0,
                    record.received as i64,
                    record.received_wire as i64,
                    record.upstream_encoding,
                    ms(record.download),
                    i64::from(record.complete),
                    record.usage.map(|usage| usage.prompt as i64),
                    record
                        .usage
                        .and_then(|usage| usage.cached)
                        .map(|c| c as i64),
                    record.usage.map(|usage| usage.completion as i64),
                    record
                        .usage
                        .and_then(|usage| usage.reasoning)
                        .map(|r| r as i64),
                    record.received_agent as i64,
                    record.upstream,
                    record.tier,
                ])?;
            }
            Event::Log { level, message, .. } => {
                log.execute(params![logfmt::epoch(), *level as i64, message])?;
            }
        }
    }
    Ok(())
}

/// Seconds to the `_ms` column, or NULL for a phase that did not happen.
fn ms(seconds: Option<f64>) -> Option<f64> {
    seconds.map(|value| value * 1000.0)
}

fn prune(connection: &Connection, retention_days: u32) {
    let cutoff = logfmt::epoch() - f64::from(retention_days) * 86_400.0;
    let mut removed = 0;
    for table in ["requests", "logs"] {
        match connection.execute(
            &format!("DELETE FROM {table} WHERE ts_unix < ?1"),
            params![cutoff],
        ) {
            Ok(rows) => removed += rows,
            Err(err) => failure(&err),
        }
    }
    if removed > 0 {
        logfmt::from_store(
            Level::Info,
            &format!("retention: deleted {removed} rows older than {retention_days}d"),
        );
    }
}

/// A failure here goes back to the dashboard and the console, never into the
/// queue it came out of.
fn failure(err: &rusqlite::Error) {
    logfmt::from_store(Level::Error, &format!("sqlite: {err}"));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use http::Method;

    use super::*;
    use crate::forwarder::Coding;
    use crate::telemetry::RequestRecord;
    use crate::usage::Usage;

    /// One directory per test: the writer thread and the assertions never
    /// share a file with another test. `spawn` creates it.
    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("portway-store-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn record(status: u16, complete: bool) -> RequestRecord {
        RequestRecord {
            stamp: "23:41:02".to_string(),
            upstream: "model-alpha".to_string(),
            model: "model-alpha".to_string(),
            tier: Some("tier-a".to_string()),
            method: Method::POST,
            path: "/v1/chat/completions".to_string(),
            status,
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
            complete,
            usage: Some(Usage {
                prompt: 18234,
                cached: Some(18200),
                completion: 891,
                reasoning: Some(742),
            }),
            flight: None,
        }
    }

    /// A pooled connection reports no handshake at all, so those columns stay
    /// NULL rather than 0; a stream that ends early never sees the engine's
    /// usage chunk, so those columns do too.
    fn reused(mut record: RequestRecord) -> RequestRecord {
        record.dns = None;
        record.tcp = None;
        record.tls = None;
        record.upload = None;
        record.download = None;
        record.usage = None;
        record
    }

    fn open_read(db: &Path) -> Connection {
        Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
    }

    #[test]
    fn events_land_in_the_right_tables() {
        let dir = dir("events");
        let store = spawn(&dir, 0).unwrap();
        let sender = store.sender();
        sender
            .send(Event::Log {
                stamp: "23:41:02".to_string(),
                level: Level::Warning,
                message: "upstream 503".to_string(),
            })
            .unwrap();
        sender
            .send(Event::Request(Arc::new(record(200, true))))
            .unwrap();
        sender
            .send(Event::Request(Arc::new(reused(record(502, false)))))
            .unwrap();
        store.shutdown();

        let db = open_read(&dir.join(DB_FILE));
        let (model, method, path, status, coding, complete, dns, ttfb, body): (
            String,
            String,
            String,
            i64,
            String,
            i64,
            Option<f64>,
            f64,
            i64,
        ) = db
            .query_row(
                "SELECT model, method, path, status, coding, complete, dns_ms, ttfb_ms, body_len
                 FROM requests WHERE complete = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(model, "model-alpha");
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(status, 200);
        assert_eq!(coding, "zstd");
        assert_eq!(complete, 1);
        assert_eq!(body, 700_000);
        assert!((dns.unwrap() - 1.0).abs() < 1e-9, "dns_ms {dns:?}");
        assert!((ttfb - 17_440.0).abs() < 1e-9, "ttfb_ms {ttfb}");

        let tokens: (i64, i64, i64, i64) = db
            .query_row(
                "SELECT prompt_tokens, cached_tokens, completion_tokens, reasoning_tokens
                 FROM requests WHERE complete = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(tokens, (18_234, 18_200, 891, 742));

        // The second row reused its connection and never finished receiving
        // its answer: every column that only a fresh dial, an upload or the
        // engine's own counts can fill stays NULL rather than 0.
        let (coding, filled): (String, i64) = db
            .query_row(
                "SELECT coding,
                        COALESCE(dns_ms IS NOT NULL OR tcp_ms IS NOT NULL
                                 OR tls_ms IS NOT NULL OR upload_ms IS NOT NULL
                                 OR download_ms IS NOT NULL
                                 OR prompt_tokens IS NOT NULL
                                 OR cached_tokens IS NOT NULL
                                 OR completion_tokens IS NOT NULL
                                 OR reasoning_tokens IS NOT NULL, 0)
                 FROM requests WHERE complete = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(coding, "zstd");
        assert_eq!(filled, 0);

        let (level, message, ts): (i64, String, f64) = db
            .query_row("SELECT level, message, ts_unix FROM logs", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(level, Level::Warning as i64);
        assert_eq!(message, "upstream 503");
        assert!(ts > 1_700_000_000.0, "ts_unix {ts}");
    }

    #[test]
    fn retention_deletes_rows_older_than_the_window() {
        let dir = dir("retention");
        let store = spawn(&dir, 0).unwrap();
        store
            .sender()
            .send(Event::Request(Arc::new(record(200, true))))
            .unwrap();
        store
            .sender()
            .send(Event::Log {
                stamp: "23:41:02".to_string(),
                level: Level::Info,
                message: "hello".to_string(),
            })
            .unwrap();
        store.shutdown();

        let db = Connection::open(dir.join(DB_FILE)).unwrap();
        db.execute("UPDATE requests SET ts_unix = 0", []).unwrap();
        db.execute("UPDATE logs SET ts_unix = 0", []).unwrap();
        drop(db);

        let store = spawn(&dir, 1).unwrap();
        store.shutdown();

        let db = open_read(&dir.join(DB_FILE));
        for table in ["requests", "logs"] {
            let count: i64 = db
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} kept rows past the window");
        }
    }

    /// A `--data-dir` the user made keeps its own mode, so the database has to
    /// narrow itself: when it is new, and when an earlier build left it (and its
    /// sidecars, held open here by a reader across the restart) readable.
    #[test]
    fn the_database_is_owner_only_in_a_directory_portway_did_not_create() {
        let dir = dir("mode");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let path = |suffix: &str| dir.join(format!("{DB_FILE}{suffix}"));
        let mode = |suffix: &str| fs::metadata(path(suffix)).unwrap().permissions().mode() & 0o777;
        let files = ["", "-wal", "-shm"];

        let store = spawn(&dir, 0).unwrap();
        for suffix in files {
            assert_eq!(mode(suffix), 0o600, "new {DB_FILE}{suffix}");
        }

        let reader = Connection::open(path("")).unwrap();
        let _: i64 = reader
            .query_row("SELECT count(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        store.shutdown();
        for suffix in files {
            fs::set_permissions(path(suffix), fs::Permissions::from_mode(0o644)).unwrap();
        }

        let store = spawn(&dir, 0).unwrap();
        for suffix in files {
            assert_eq!(
                mode(suffix),
                0o600,
                "existing {DB_FILE}{suffix} stayed readable"
            );
        }
        store.shutdown();
        drop(reader);
    }

    #[test]
    fn a_newer_schema_version_refuses_to_open() {
        let dir = dir("version");
        spawn(&dir, 0).unwrap().shutdown();

        let db = Connection::open(dir.join(DB_FILE)).unwrap();
        db.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(db);

        let err = match spawn(&dir, 0) {
            Err(err) => err,
            Ok(_) => panic!("a newer schema version was accepted"),
        };
        assert!(err.contains("newer portway"), "{err}");
    }

    /// An existing install is a file this build did not create: the upgrade
    /// adds the later columns in place, keeps every row, and leaves the ones
    /// recorded before it NULL (or at the added column's default).
    #[test]
    fn a_version_one_file_gains_the_later_columns() {
        let dir = dir("upgrade");
        fs::create_dir_all(&dir).unwrap();
        let db = Connection::open(dir.join(DB_FILE)).unwrap();
        db.execute_batch(
            "CREATE TABLE requests (
               id INTEGER PRIMARY KEY, ts_unix REAL NOT NULL, model TEXT NOT NULL,
               method TEXT NOT NULL, path TEXT NOT NULL, status INTEGER NOT NULL,
               dns_ms REAL, tcp_ms REAL, tls_ms REAL, body_len INTEGER NOT NULL,
               wire_len INTEGER NOT NULL, coding TEXT NOT NULL, upload_ms REAL,
               ttfb_ms REAL NOT NULL, received INTEGER NOT NULL,
               received_wire INTEGER NOT NULL, upstream_encoding TEXT NOT NULL,
               download_ms REAL, complete INTEGER NOT NULL
             );
             CREATE TABLE logs (
               id INTEGER PRIMARY KEY, ts_unix REAL NOT NULL, level INTEGER NOT NULL,
               message TEXT NOT NULL
             );
             INSERT INTO requests (ts_unix, model, method, path, status, body_len,
               wire_len, coding, ttfb_ms, received, received_wire,
               upstream_encoding, complete)
             VALUES (0, 'model-zeta', 'POST', '/v1/chat/completions', 200, 1, 1,
               'identity', 1, 1, 1, 'identity', 1);",
        )
        .unwrap();
        db.pragma_update(None, "user_version", 1).unwrap();
        drop(db);

        let store = spawn(&dir, 0).unwrap();
        store
            .sender()
            .send(Event::Request(Arc::new(record(200, true))))
            .unwrap();
        store.shutdown();

        let db = open_read(&dir.join(DB_FILE));
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let before: Option<i64> = db
            .query_row(
                "SELECT prompt_tokens FROM requests WHERE model = 'model-zeta'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, None, "a row from before the upgrade has none");
        let after: (i64, i64) = db
            .query_row(
                "SELECT prompt_tokens, reasoning_tokens FROM requests
                 WHERE model = 'model-alpha'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(after, (18_234, 742));

        // The v3 column rode along in the same upgrade: the row from before it
        // takes the default, and the new one keeps what the hop actually sent.
        let agent: (i64, i64) = db
            .query_row(
                "SELECT (SELECT received_agent FROM requests WHERE model = 'model-zeta'),
                        (SELECT received_agent FROM requests WHERE model = 'model-alpha')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(agent, (0, 40));

        // And the v4 column: the old row's route is the name its model
        // column held, the new row's is what the forwarder said.
        let upstream: (String, String) = db
            .query_row(
                "SELECT (SELECT upstream FROM requests WHERE model = 'model-zeta'),
                        (SELECT upstream FROM requests WHERE model = 'model-alpha')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(upstream, ("model-zeta".into(), "model-alpha".into()));

        // And the v5 column: the old row named no tier, the new one keeps
        // the one its request named.
        let tier: (Option<String>, Option<String>) = db
            .query_row(
                "SELECT (SELECT tier FROM requests WHERE model = 'model-zeta'),
                        (SELECT tier FROM requests WHERE model = 'model-alpha')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(tier, (None, Some("tier-a".into())));
    }

    #[test]
    fn the_data_dir_follows_xdg_then_home() {
        let override_dir = Path::new("/tmp/elsewhere");
        assert_eq!(
            data_dir_from(Some(override_dir), None, None).unwrap(),
            override_dir
        );

        let xdg = data_dir_from(
            None,
            Some(PathBuf::from("/xdg")),
            Some(PathBuf::from("/home/me")),
        )
        .unwrap();
        assert_eq!(xdg, Path::new("/xdg/portway"));

        // A relative XDG_CONFIG_HOME is not a place to keep a database.
        let relative = data_dir_from(
            None,
            Some(PathBuf::from("relative")),
            Some(PathBuf::from("/home/me")),
        )
        .unwrap();
        assert_eq!(relative, Path::new("/home/me/.config/portway"));

        let err = data_dir_from(None, None, None).unwrap_err();
        assert!(err.contains("--data-dir"), "{err}");
    }
}
