//! `--report`: what the recorder has, read back out of `db.sqlite3`.
//!
//! Read-only in the sense that matters: the file is opened through the
//! recorder's own path (same busy timeout, same schema check) but nothing is
//! ever written, migrated or created. A forwarder that has not recorded yet
//! reads as "nothing recorded", not as an empty database.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, params};

use crate::logfmt;
use crate::store;

/// How many trouble lines are shown: the most recent ones in the window.
const TROUBLE: usize = 20;

/// The window's per-model totals. `2xx`, `3xx`, `4xx` and `5xx` are the same
/// classes the log colors by, so they add up to `requests`.
const AGGREGATE: &str = "SELECT
    model,
    COUNT(*),
    SUM(status < 300),
    SUM(status >= 300 AND status < 400),
    SUM(status >= 400 AND status < 500),
    SUM(status >= 500),
    SUM(complete = 0),
    SUM(dns_ms IS NULL AND tcp_ms IS NULL AND tls_ms IS NULL),
    SUM(body_len),
    SUM(wire_len),
    SUM(received),
    SUM(received_wire)
  FROM requests
  WHERE ts_unix >= ?1 AND (?2 IS NULL OR model = ?2)
  GROUP BY model
  ORDER BY COUNT(*) DESC";

const TTFB_SAMPLES: &str = "SELECT model, ttfb_ms FROM requests
  WHERE ts_unix >= ?1 AND (?2 IS NULL OR model = ?2) AND ttfb_ms IS NOT NULL
  ORDER BY model, ttfb_ms";

const UPLOAD_SAMPLES: &str = "SELECT model, upload_ms FROM requests
  WHERE ts_unix >= ?1 AND (?2 IS NULL OR model = ?2) AND upload_ms IS NOT NULL
  ORDER BY model, upload_ms";

const CONNECT_SAMPLES: &str = "SELECT model, dns_ms + tcp_ms + tls_ms FROM requests
  WHERE ts_unix >= ?1 AND (?2 IS NULL OR model = ?2) AND dns_ms IS NOT NULL
  ORDER BY model, dns_ms";

const TROUBLE_REQUESTS: &str = "SELECT ts_unix, status, model, method, path, received, complete
  FROM requests
  WHERE ts_unix >= ?1 AND (?2 IS NULL OR model = ?2) AND (status >= 400 OR complete = 0)
  ORDER BY ts_unix DESC LIMIT 20";

const TROUBLE_LOGS: &str = "SELECT ts_unix, level, message FROM logs
  WHERE ts_unix >= ?1 AND level >= 1
  ORDER BY ts_unix DESC LIMIT 20";

/// Render the recorded window. `since` is the width measured back from now,
/// and `model` narrows the request tables to one upstream.
pub fn render(db: &Path, since: Duration, model: Option<&str>) -> Result<String, String> {
    let Some(connection) = store::open_existing(db)? else {
        return Err(nothing_yet(db));
    };
    let cutoff = logfmt::epoch() - since.as_secs_f64();

    let aggregates = load_aggregates(&connection, cutoff, model)?;
    let ttfb = Samples::load(&connection, TTFB_SAMPLES, cutoff, model)?;
    let upload = Samples::load(&connection, UPLOAD_SAMPLES, cutoff, model)?;
    let connect = Samples::load(&connection, CONNECT_SAMPLES, cutoff, model)?;

    let mut out = String::new();
    out.push_str(&format!("portway — {}\n", db.display()));
    out.push_str(&format!(
        "window  {} .. {}  ({}, {})\n",
        logfmt::datetime(cutoff),
        logfmt::datetime(logfmt::epoch()),
        logfmt::span(since),
        model.unwrap_or("all models"),
    ));

    if aggregates.is_empty() {
        out.push('\n');
        out.push_str(&match model {
            Some(name) => format!("no requests for model {name} in this window\n"),
            None => "no requests recorded in this window\n".to_string(),
        });
    } else {
        out.push('\n');
        out.push_str(&volume_table(&aggregates));
        out.push('\n');
        out.push_str(&timing_table(&aggregates, [&ttfb, &upload, &connect]));
    }

    out.push('\n');
    let trouble = trouble(&connection, cutoff, model)?;
    out.push_str(&format!("trouble (last {TROUBLE} in the window)\n"));
    if trouble.is_empty() {
        out.push_str("  none\n");
    } else {
        for line in trouble {
            out.push_str(&line);
            out.push('\n');
        }
    }
    Ok(out)
}

fn nothing_yet(db: &Path) -> String {
    format!(
        "no database at {}; the forwarder has not recorded anything yet",
        db.display()
    )
}

#[derive(Debug, Clone)]
struct Aggregate {
    model: String,
    requests: i64,
    ok: i64,
    redirect: i64,
    client: i64,
    server: i64,
    truncated: i64,
    reused: i64,
    body: i64,
    wire: i64,
    received: i64,
    received_wire: i64,
}

fn load_aggregates(
    connection: &Connection,
    cutoff: f64,
    model: Option<&str>,
) -> Result<Vec<Aggregate>, String> {
    let mut statement = connection.prepare(AGGREGATE).map_err(db_error)?;
    let rows = statement
        .query_map(params![cutoff, model], |row| {
            Ok(Aggregate {
                model: row.get(0)?,
                requests: row.get(1)?,
                ok: row.get(2)?,
                redirect: row.get(3)?,
                client: row.get(4)?,
                server: row.get(5)?,
                truncated: row.get(6)?,
                reused: row.get(7)?,
                body: row.get(8)?,
                wire: row.get(9)?,
                received: row.get(10)?,
                received_wire: row.get(11)?,
            })
        })
        .map_err(db_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(db_error)
}

/// One ordered sample list per model, plus the same values pooled across the
/// models — which is what the `total` row's percentile is taken from.
#[derive(Default)]
struct Samples {
    per_model: HashMap<String, Vec<f64>>,
    pooled: Vec<f64>,
}

impl Samples {
    fn load(
        connection: &Connection,
        sql: &str,
        cutoff: f64,
        model: Option<&str>,
    ) -> Result<Self, String> {
        let mut samples = Samples::default();
        let mut statement = connection.prepare(sql).map_err(db_error)?;
        let rows = statement
            .query_map(params![cutoff, model], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })
            .map_err(db_error)?;
        for row in rows {
            let (model, value) = row.map_err(db_error)?;
            samples.per_model.entry(model).or_default().push(value);
            samples.pooled.push(value);
        }
        samples.pooled.sort_by(f64::total_cmp);
        Ok(samples)
    }

    fn of(&self, model: &str) -> &[f64] {
        self.per_model.get(model).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Nearest rank, the same rule the dashboard's rolling window uses: the
/// smallest sample at or above the quantile.
fn percentile(samples: &[f64], quantile: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let rank = ((quantile * samples.len() as f64).ceil() as usize).clamp(1, samples.len()) - 1;
    Some(samples[rank])
}

fn mean(samples: &[f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    Some(samples.iter().sum::<f64>() / samples.len() as f64)
}

fn volume_table(aggregates: &[Aggregate]) -> String {
    let header = [
        "model", "reqs", "2xx", "3xx", "4xx", "5xx", "trunc", "reused",
    ];
    let mut rows: Vec<Vec<String>> = aggregates.iter().map(volume_row).collect();

    let total = totals(aggregates);
    rows.push(volume_row(&total));
    table(&header, &rows)
}

fn volume_row(row: &Aggregate) -> Vec<String> {
    [
        row.model.clone(),
        row.requests.to_string(),
        row.ok.to_string(),
        row.redirect.to_string(),
        row.client.to_string(),
        row.server.to_string(),
        row.truncated.to_string(),
        row.reused.to_string(),
    ]
    .to_vec()
}

fn totals(aggregates: &[Aggregate]) -> Aggregate {
    let mut total = Aggregate {
        model: "total".to_string(),
        requests: 0,
        ok: 0,
        redirect: 0,
        client: 0,
        server: 0,
        truncated: 0,
        reused: 0,
        body: 0,
        wire: 0,
        received: 0,
        received_wire: 0,
    };
    for row in aggregates {
        total.requests += row.requests;
        total.ok += row.ok;
        total.redirect += row.redirect;
        total.client += row.client;
        total.server += row.server;
        total.truncated += row.truncated;
        total.reused += row.reused;
        total.body += row.body;
        total.wire += row.wire;
        total.received += row.received;
        total.received_wire += row.received_wire;
    }
    total
}

fn timing_table(aggregates: &[Aggregate], samples: [&Samples; 3]) -> String {
    let header = [
        "model",
        "up raw",
        "up wire",
        "saved",
        "down",
        "ttfb p50",
        "ttfb p95",
        "up p50",
        "up p95",
        "conn mean",
    ];
    let [ttfb, upload, connect] = samples;

    let mut rows: Vec<Vec<String>> = Vec::with_capacity(aggregates.len() + 1);
    for row in aggregates {
        rows.push(timing_row(
            &row.model,
            row.body,
            row.wire,
            row.received,
            ttfb.of(&row.model),
            upload.of(&row.model),
            connect.of(&row.model),
        ));
    }
    let total = totals(aggregates);
    rows.push(timing_row(
        "total",
        total.body,
        total.wire,
        total.received,
        &ttfb.pooled,
        &upload.pooled,
        &connect.pooled,
    ));
    table(&header, &rows)
}

fn timing_row(
    model: &str,
    body: i64,
    wire: i64,
    received: i64,
    ttfb: &[f64],
    upload: &[f64],
    connect: &[f64],
) -> Vec<String> {
    let saved = if body == 0 {
        "-".to_string()
    } else {
        format!("{}%", (body - wire) * 100 / body)
    };
    [
        model.to_string(),
        logfmt::human(body as u64),
        logfmt::human(wire as u64),
        saved,
        logfmt::human(received as u64),
        ms(percentile(ttfb, 0.50)),
        ms(percentile(ttfb, 0.95)),
        ms(percentile(upload, 0.50)),
        ms(percentile(upload, 0.95)),
        ms(mean(connect)),
    ]
    .to_vec()
}

/// The `_ms` columns are milliseconds; every duration in the report is printed
/// the way the log prints one.
fn ms(value: Option<f64>) -> String {
    match value {
        Some(ms) => logfmt::human_time(ms / 1000.0),
        None => "-".to_string(),
    }
}

/// A line the report has to explain: a request that failed or was cut short,
/// or a log record WARNING and up.
enum Trouble {
    Request {
        ts: f64,
        status: i64,
        model: String,
        method: String,
        path: String,
        received: i64,
        complete: bool,
    },
    Log {
        ts: f64,
        level: i64,
        message: String,
    },
}

impl Trouble {
    fn ts(&self) -> f64 {
        match self {
            Trouble::Request { ts, .. } | Trouble::Log { ts, .. } => *ts,
        }
    }

    fn model_width(&self) -> usize {
        match self {
            Trouble::Request { model, .. } => model.chars().count(),
            Trouble::Log { .. } => 0,
        }
    }

    /// `2026-09-20 18:01:44  502  model-alpha  POST /v1/chat/completions  (truncated after 1.2KB)`,
    /// or the same shape with the level and message of a log record.
    fn render(&self, model_width: usize) -> String {
        match self {
            Trouble::Request {
                ts,
                status,
                model,
                method,
                path,
                received,
                complete,
            } => {
                let cut = if *complete {
                    String::new()
                } else {
                    format!("  (truncated after {})", logfmt::human(*received as u64))
                };
                format!(
                    "{}  {status}  {model:<model_width$}  {method} {path}{cut}",
                    logfmt::datetime(*ts)
                )
            }
            Trouble::Log { ts, level, message } => format!(
                "{}  {}  {message}",
                logfmt::datetime(*ts),
                logfmt::Level::from_stored(*level).name()
            ),
        }
    }
}

/// Both sources are read newest first, merged oldest first, and cut to the
/// most recent `TROUBLE` lines — the tail is the part that is still news.
fn trouble(
    connection: &Connection,
    cutoff: f64,
    model: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut entries: Vec<Trouble> = Vec::new();

    let mut statement = connection.prepare(TROUBLE_REQUESTS).map_err(db_error)?;
    let rows = statement
        .query_map(params![cutoff, model], |row| {
            Ok(Trouble::Request {
                ts: row.get(0)?,
                status: row.get(1)?,
                model: row.get(2)?,
                method: row.get(3)?,
                path: row.get(4)?,
                received: row.get(5)?,
                complete: row.get(6)?,
            })
        })
        .map_err(db_error)?;
    for row in rows {
        entries.push(row.map_err(db_error)?);
    }

    let mut statement = connection.prepare(TROUBLE_LOGS).map_err(db_error)?;
    let rows = statement
        .query_map(params![cutoff], |row| {
            Ok(Trouble::Log {
                ts: row.get(0)?,
                level: row.get(1)?,
                message: row.get(2)?,
            })
        })
        .map_err(db_error)?;
    for row in rows {
        entries.push(row.map_err(db_error)?);
    }

    entries.sort_by(|a, b| a.ts().total_cmp(&b.ts()));
    let entries = &entries[entries.len().saturating_sub(TROUBLE)..];
    let width = entries.iter().map(Trouble::model_width).max().unwrap_or(0);
    Ok(entries.iter().map(|entry| entry.render(width)).collect())
}

/// Left-aligns the first column, right-aligns every other cell, two spaces
/// apart. Widths come from the content, so the tables stay aligned whatever
/// the model names are.
fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }
    }
    let line = |cells: &[String]| -> String {
        let mut text = String::new();
        for (index, cell) in cells.iter().enumerate() {
            if index > 0 {
                text.push_str("  ");
            }
            let width = widths[index];
            if index == 0 {
                text.push_str(&format!("{cell:<width$}"));
            } else {
                text.push_str(&format!("{cell:>width$}"));
            }
        }
        text.trim_end().to_string()
    };

    let mut out = String::new();
    let head: Vec<String> = header.iter().map(|cell| cell.to_string()).collect();
    out.push_str(&line(&head));
    out.push('\n');
    for row in rows {
        out.push_str(&line(row));
        out.push('\n');
    }
    out
}

fn db_error(err: rusqlite::Error) -> String {
    format!("sqlite: {err}")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use http::Method;

    use super::*;
    use crate::forwarder::Coding;
    use crate::logfmt::Level;
    use crate::telemetry::{Event, RequestRecord};

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("portway-report-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(model: &str, status: u16, complete: bool) -> RequestRecord {
        RequestRecord {
            stamp: "23:41:02".to_string(),
            model: model.to_string(),
            method: Method::POST,
            path: "/v1/chat/completions".to_string(),
            status,
            dns: Some(0.001),
            tcp: Some(0.029),
            tls: Some(0.047),
            body_len: 2048,
            wire_len: 512,
            coding: Coding::Zstd,
            upload: Some(0.010),
            ttfb: 0.5,
            received: 1024,
            received_wire: 256,
            received_agent: 1024,
            upstream_encoding: "gzip".to_string(),
            agent_encoding: None,
            download: Some(0.020),
            complete,
            usage: None,
        }
    }

    /// Every row for `label`, as the whitespace-separated cells the tables
    /// drew — the volume table's first, the timing table's second.
    fn rows(text: &str, label: &str) -> Vec<Vec<String>> {
        let found: Vec<Vec<String>> = text
            .lines()
            .filter(|line| line.split_whitespace().next() == Some(label))
            .map(|line| line.split_whitespace().map(str::to_string).collect())
            .collect();
        assert!(!found.is_empty(), "no {label} row in:\n{text}");
        found
    }

    fn row(text: &str, label: &str) -> Vec<String> {
        rows(text, label).remove(0)
    }

    fn populated(name: &str) -> std::path::PathBuf {
        let dir = dir(name);
        let store = store::spawn(&dir, 0).unwrap();
        let sender = store.sender();
        sender
            .send(Event::Request(Arc::new(record("model-alpha", 200, true))))
            .unwrap();
        let mut reused = record("model-alpha", 200, true);
        reused.dns = None;
        reused.tcp = None;
        reused.tls = None;
        reused.upload = None;
        reused.body_len = 7_000_000;
        reused.wire_len = 2_000_000;
        sender.send(Event::Request(Arc::new(reused))).unwrap();
        sender
            .send(Event::Request(Arc::new(record("model-zeta", 500, false))))
            .unwrap();
        sender
            .send(Event::Log {
                stamp: "23:41:02".to_string(),
                level: Level::Warning,
                message: "/health.request_encodings missing: api.example.test".to_string(),
            })
            .unwrap();
        // INFO is the traffic log: recorded, but never part of the trouble
        // list.
        sender
            .send(Event::Log {
                stamp: "23:41:02".to_string(),
                level: Level::Info,
                message: "model-alpha: zstd negotiated".to_string(),
            })
            .unwrap();
        store.shutdown();
        dir
    }

    #[test]
    fn the_report_sums_a_window() {
        let dir = populated("window");
        let text = render(&dir.join(store::DB_FILE), Duration::from_secs(86_400), None).unwrap();

        assert!(text.contains("window  "), "{text}");
        assert!(text.contains("(24h, all models)"), "{text}");

        // reqs 2xx 3xx 4xx 5xx trunc reused
        assert_eq!(
            row(&text, "model-alpha"),
            ["model-alpha", "2", "2", "0", "0", "0", "0", "1"]
        );
        assert_eq!(
            row(&text, "model-zeta"),
            ["model-zeta", "1", "0", "0", "0", "1", "1", "0"]
        );
        assert_eq!(
            row(&text, "total"),
            ["total", "3", "2", "0", "0", "1", "1", "1"]
        );

        // up raw from the body sizes, and the pooled percentiles behind it.
        let timing = rows(&text, "total");
        assert_eq!(timing.len(), 2, "{timing:?}");
        let timing = &timing[1];
        assert_eq!(timing.len(), 10, "{timing:?}");
        assert_eq!(timing[1], logfmt::human(2048 + 7_000_000 + 2048));
        assert_eq!(timing[3], "71%");

        // The one 4xx/5xx/trouble row is the truncated 500.
        assert!(text.contains("trouble (last 20 in the window)"), "{text}");
        assert!(text.contains("500  model-zeta"), "{text}");
        assert!(text.contains("(truncated after 1KB)"), "{text}");
        assert!(
            text.contains("WARNING  /health.request_encodings missing"),
            "{text}"
        );
        assert!(
            !text.contains("INFO  model-alpha: zstd negotiated"),
            "an INFO line reached the trouble list:\n{text}"
        );
    }

    #[test]
    fn a_model_filter_leaves_one_row_plus_the_total() {
        let dir = populated("filter");
        let text = render(
            &dir.join(store::DB_FILE),
            Duration::from_secs(86_400),
            Some("model-zeta"),
        )
        .unwrap();

        assert!(text.contains("(24h, model-zeta)"), "{text}");
        assert_eq!(
            row(&text, "model-zeta"),
            ["model-zeta", "1", "0", "0", "0", "1", "1", "0"]
        );
        assert_eq!(
            row(&text, "total"),
            ["total", "1", "0", "0", "0", "1", "1", "0"]
        );
        assert!(
            !text.lines().any(|line| line.starts_with("model-alpha")),
            "{text}"
        );
    }

    #[test]
    fn an_empty_database_says_so() {
        let dir = dir("empty");
        store::spawn(&dir, 0).unwrap().shutdown();
        let text = render(&dir.join(store::DB_FILE), Duration::from_secs(3600), None).unwrap();
        assert!(
            text.contains("no requests recorded in this window"),
            "{text}"
        );
        assert!(text.contains("(1h, all models)"), "{text}");
        assert!(text.trim_end().ends_with("  none"), "{text}");
    }

    #[test]
    fn a_missing_database_names_the_path() {
        let dir = dir("missing");
        let db = dir.join(store::DB_FILE);
        let err = render(&db, Duration::from_secs(3600), None).unwrap_err();
        assert!(err.contains("no database at"), "{err}");
        assert!(err.contains(&db.display().to_string()), "{err}");
        assert!(
            !db.exists(),
            "the report created the file it was asked to read"
        );
    }
}
