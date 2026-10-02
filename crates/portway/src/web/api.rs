//! The console's JSON, one builder per shape. The page reads nothing else, so
//! this file and `webui/static/js/api.js` are the whole contract between the
//! two. Durations are seconds, sizes are bytes, `null` is "not measured".
//! Every label the terminal dashboard words for itself — routes, codings,
//! compression status, usage notes, report lines — is worded here by the same
//! code, so the two cannot disagree.

use serde_json::{Value, json};

use crate::board::{self, Board, ModelRow};
use crate::flights::FlightView;
use crate::logfmt;
use crate::report::{self, Report, Trouble};
use crate::spend;
use crate::telemetry::{Event, RequestRecord};

/// Flights one frame carries; the rest is counted in `more`.
pub const FLIGHTS_SHOWN: usize = 200;

pub fn event(seq: u64, ts: f64, event: &Event) -> Value {
    match event {
        Event::Request(record) => request(seq, ts, record),
        Event::Log {
            stamp,
            level,
            message,
        } => json!({
            "seq": seq,
            "kind": "log",
            "ts": ts,
            "stamp": stamp,
            "level": level.name(),
            "message": message,
            "trouble": *level >= logfmt::Level::Warning,
        }),
    }
}

fn request(seq: u64, ts: f64, record: &RequestRecord) -> Value {
    let (route, known) = board::route(&record.method, &record.path);
    json!({
        "seq": seq,
        "kind": "request",
        "ts": ts,
        "stamp": record.stamp,
        "upstream": record.upstream,
        "model": record.model,
        "service_tier": record.service_tier,
        "method": record.method.as_str(),
        "path": record.path.split('?').next().unwrap_or(&record.path),
        "route": route,
        "route_known": known,
        "status": record.status,
        "dns": record.dns,
        "tcp": record.tcp,
        "tls": record.tls,
        "reused": record.reused(),
        "handshake": record.handshake(),
        "body_len": record.body_len,
        "wire_len": record.wire_len,
        "coding": record.coding.name(),
        "upload": record.upload,
        "ttfb": record.ttfb,
        "received": record.received,
        "received_wire": record.received_wire,
        "received_agent": record.received_agent,
        "upstream_encoding": record.upstream_encoding,
        "agent_encoding": record.agent_encoding,
        "download": record.download,
        "complete": record.complete,
        "usage": record.usage.map(|usage| json!({
            "prompt": usage.prompt,
            "cached": usage.cached,
            "completion": usage.completion,
            "reasoning": usage.reasoning,
        })),
        "trouble": record.status >= 400 || !record.complete,
        "flight": record.flight,
    })
}

pub fn flight(view: &FlightView) -> Value {
    let (route, known) = board::route(&view.method, &view.path);
    json!({
        "id": view.id,
        "upstream": view.upstream,
        "model": view.model,
        "method": view.method.as_str(),
        "path": view.path,
        "route": route,
        "route_known": known,
        "started_unix": view.started_unix,
        "age_s": view.age,
        "idle_s": view.idle,
        "phase": view.phase.name(),
        "body_len": view.body_len,
        "wire_len": view.wire_len,
        "coding": view.coding.name(),
        "status": view.status,
        "ttfb": view.ttfb,
        "received": view.received,
        "received_wire": view.received_wire,
        "received_agent": view.received_agent,
        "retries": view.retries,
        "upload_s": view.upload,
    })
}

/// The oldest `FLIGHTS_SHOWN`, which are the ones worth looking at.
pub fn flights(at_unix: f64, views: &[FlightView]) -> Value {
    json!({
        "at_unix": at_unix,
        "total": views.len(),
        "more": views.len().saturating_sub(FLIGHTS_SHOWN),
        "list": views.iter().take(FLIGHTS_SHOWN).map(flight).collect::<Vec<_>>(),
    })
}

pub fn totals(board: &Board) -> Value {
    let totals = &board.totals;
    json!({
        "requests": totals.requests,
        "encoded": totals.encoded,
        "in_flight": totals.in_flight,
        "body_bytes": totals.body_bytes,
        "wire_bytes": totals.wire_bytes,
        "down_bytes": totals.down_bytes,
        "down_wire_bytes": totals.down_wire_bytes,
        "agent_bytes": totals.agent_bytes,
        "retried_identity": totals.retried_identity,
        "aborts": totals.aborts,
        "upstream_errors": totals.upstream_errors,
        "idle_conns": totals.idle_conns,
    })
}

pub fn counts(board: &Board) -> Value {
    json!({
        "seen": board.seen,
        "ok": board.ok,
        "redirected": board.redirected,
        "client_errors": board.client_errors,
        "server_errors": board.server_errors,
        "reused": board.reused,
        "truncated": board.truncated,
    })
}

/// The HUD's rolling percentiles, by the dashboard's rule rather than the
/// report's: the two are different questions and stay answered apart.
pub fn latency(board: &Board) -> Value {
    json!({
        "ttfb": {
            "p50": board::percentile(&board.ttfb, 0.50),
            "p95": board::percentile(&board.ttfb, 0.95),
            "n": board.ttfb.len(),
        },
        "upload": {
            "p50": board::percentile(&board.upload, 0.50),
            "p95": board::percentile(&board.upload, 0.95),
            "n": board.upload.len(),
        },
        "handshake_mean": board::mean(&board.handshake),
    })
}

pub fn model(row: &ModelRow) -> Value {
    let view = &row.view;
    json!({
        "name": row.name,
        "coding": view.coding.name(),
        "dict": view.dict,
        "coding_label": board::coding_label(view.coding, view.dict),
        "requests": view.requests,
        "encoded": view.encoded_requests,
        "in_flight": view.in_flight,
        "body": view.body_bytes,
        "wire": view.wire_bytes,
        "saved": view.saved_bytes().max(0),
        "down": view.down_bytes,
        "down_wire": view.down_wire_bytes,
        "down_saved": view.down_saved_bytes().max(0),
        "agent": view.agent_bytes,
        "idle": view.idle_conns,
        "errors": view.upstream_errors,
        "aborts": view.client_aborts,
        "retried_identity": view.retried_identity,
        "status": board::compression_status(view),
    })
}

pub fn models(board: &Board) -> Value {
    Value::Array(board.models.iter().map(model).collect())
}

/// The last `count` bars, oldest first.
pub fn bars(board: &Board, count: usize) -> Value {
    let skip = board.bars.len().saturating_sub(count);
    Value::Array(
        board
            .bars
            .iter()
            .skip(skip)
            .map(|(raw, wire)| json!([raw, wire]))
            .collect(),
    )
}

/// The last `count` one-second buckets, ending at the second `end` names.
pub fn traffic(board: &Board, count: usize) -> Value {
    let (up, down) = board.traffic.buckets();
    let tail = |buckets: &std::collections::VecDeque<u64>| {
        let skip = buckets.len().saturating_sub(count);
        buckets.iter().skip(skip).copied().collect::<Vec<_>>()
    };
    json!({
        "end": board.traffic.second(),
        "up": tail(up),
        "down": tail(down),
    })
}

pub fn usage_ranges() -> Value {
    Value::Array(
        spend::Range::ALL
            .iter()
            .map(|range| json!({"key": range.key(), "label": range.label()}))
            .collect(),
    )
}

pub fn usage(range: spend::Range, table: &spend::Table) -> Value {
    json!({
        "range": range.key(),
        "ranges": usage_ranges(),
        "since": table.since,
        "until": table.until,
        // Both ends carry their date, as on the terminal's usage screen.
        "title": format!(
            "usage — {} .. {}",
            logfmt::datetime(table.since),
            logfmt::datetime(table.until)
        ),
        "rows": table.rows.iter().map(usage_row).collect::<Vec<_>>(),
        "total": usage_row(&table.total),
        "unpriced": table.unpriced,
        "blind": table.blind,
        "cut": table.cut,
        "notes": spend::notes(table),
    })
}

fn usage_row(row: &spend::Row) -> Value {
    json!({
        "model": row.model,
        "tier": row.tier,
        "requests": row.requests,
        "prompt": row.prompt,
        "cached": row.cached,
        "unreported": row.unreported,
        "completion": row.completion,
        "reasoning": row.reasoning,
        "uncached": row.uncached(),
        "hit_rate": row.hit_rate(),
        "charge": row.charge.map(|charge| json!({
            "input": charge.input,
            "cache_read": charge.cache_read,
            "output": charge.output,
            "total": charge.total(),
        })),
        "cost": row.cost(),
    })
}

/// The report as data, and as the exact `--report` text when asked for.
pub fn report(report: &Report, text: bool) -> Value {
    json!({
        "db": report.db.display().to_string(),
        "from": report.from,
        "to": report.to,
        "window": format!(
            "{} .. {}",
            logfmt::datetime(report.from),
            logfmt::datetime(report.to)
        ),
        "span": logfmt::span(report.since),
        "model": report.model,
        "rows": report.rows.iter().map(report_row).collect::<Vec<_>>(),
        "total": report.total.as_ref().map(report_row),
        "trouble": report.trouble.iter().map(|line| {
            let mut entry = match &line.entry {
                Trouble::Request { ts, status, model, method, path, received, complete } => json!({
                    "ts": ts,
                    "kind": "request",
                    "status": status,
                    "model": model,
                    "method": method,
                    "path": path,
                    "received": received,
                    "complete": complete,
                }),
                Trouble::Log { ts, level, message } => json!({
                    "ts": ts,
                    "kind": "log",
                    "level": logfmt::Level::from_stored(*level).name(),
                    "message": message,
                }),
            };
            entry["text"] = Value::String(line.text.clone());
            entry
        }).collect::<Vec<_>>(),
        "text": text.then(|| report::render_text(report)),
    })
}

/// Timings are stored in milliseconds; the page gets seconds like everywhere.
fn report_row(row: &report::Row) -> Value {
    let seconds = |ms: Option<f64>| ms.map(|ms| ms / 1000.0);
    let counts = &row.counts;
    json!({
        "model": counts.model,
        "requests": counts.requests,
        "ok": counts.ok,
        "redirect": counts.redirect,
        "client": counts.client,
        "server": counts.server,
        "truncated": counts.truncated,
        "reused": counts.reused,
        "body": counts.body,
        "wire": counts.wire,
        "received": counts.received,
        "received_wire": counts.received_wire,
        "saved": row.saved(),
        "down_saved": row.down_saved(),
        "ttfb_p50": seconds(row.ttfb_p50),
        "ttfb_p95": seconds(row.ttfb_p95),
        "up_p50": seconds(row.up_p50),
        "up_p95": seconds(row.up_p95),
        "conn_mean": seconds(row.conn_mean),
    })
}

pub fn error(message: &str) -> Value {
    json!({ "error": message })
}
