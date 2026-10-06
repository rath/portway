//! The request log line, the humanizers and the ACK-based upload timing.
//!
//! Own test binary, and serialized inside it: the log capture is process-wide,
//! so a concurrent test would interleave its own lines into the buffer.

mod common;

use tokio::sync::{Mutex, MutexGuard};

use bytes::Bytes;
use common::{Health, Reply, forwarder, upstream};
use portway::clock::PhaseClock;
use portway::logfmt;
use regex::Regex;

static SERIAL: Mutex<()> = Mutex::const_new(());

/// Held across the awaits of a whole async test, so it must not be a blocking
/// lock.
async fn serialized() -> MutexGuard<'static, ()> {
    SERIAL.lock().await
}

fn serialized_blocking() -> MutexGuard<'static, ()> {
    SERIAL.blocking_lock()
}

/// Records must stay plain for the regex assertions, whatever the runner's
/// stderr looks like.
async fn plain_capture() -> MutexGuard<'static, ()> {
    let guard = serialized().await;
    logfmt::set_color(false);
    logfmt::start_capture();
    guard
}

fn line_starting(lines: &[String], prefix: &str) -> String {
    let stamped = Regex::new(r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d ").unwrap();
    lines
        .iter()
        .map(|line| stamped.replace(line, "").into_owned())
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line in {lines:#?}"))
}

#[tokio::test]
async fn the_request_log_reports_conn_phases_ratio_and_times() {
    let _guard = plain_capture().await;
    let up = upstream(Health::Json(vec!["zstd", "gzip"]), Reply::Ok).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;

    let body = Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "model-alpha",
            "messages": [{"role": "user", "content": "에이전트 컨텍스트 ".repeat(5000)}],
        }))
        .unwrap(),
    );
    fwd.post("/v1/chat/completions", body).await;
    fwd.get("/metrics-tiny?model=model-alpha").await;
    let lines = logfmt::take_capture();

    let post = line_starting(&lines, "POST ");
    assert!(
        Regex::new(concat!(
            r"^POST /v1/chat/completions -> 200 \| conn (reused|dns \S+ tcp \S+) \| ",
            r"up \S+ -> \S+ \(zstd, -\d+%\) \S+ \| ttfb \S+ \| down \S+ \(identity\) \S+ \| gap \S+$",
        ))
        .unwrap()
        .is_match(&post),
        "{post}"
    );

    // No request body: the up segment ends at the coding, no trailing time.
    let get = line_starting(&lines, "GET ");
    assert!(
        Regex::new(concat!(
            r"^GET \S+ -> 200 \| conn (reused|dns \S+ tcp \S+) \| up 0B -> 0B \(identity\) \| ",
            r"ttfb \S+ \| down \S+ \(identity\) \S+ \| gap \S+$",
        ))
        .unwrap()
        .is_match(&get),
        "{get}"
    );
}

/// The counts the engine reported are the one part of the line that did not
/// come off the wire as a size; a detail the engine did not report is left out,
/// and an upstream that reports nothing adds no segment at all.
#[tokio::test]
async fn the_request_log_prints_the_counts_the_engine_reported() {
    let _guard = plain_capture().await;
    let buffered = upstream(Health::Json(vec!["zstd"]), Reply::UsageJson).await;
    let streamed = upstream(Health::Json(vec!["zstd"]), Reply::UsageStream).await;
    let fwd = forwarder(
        &[
            ("model-alpha", &buffered.base),
            ("model-zeta", &streamed.base),
        ],
        &[],
    )
    .await;

    for model in ["model-alpha", "model-zeta"] {
        fwd.post(
            "/v1/chat/completions",
            Bytes::from(format!(r#"{{"model":"{model}","messages":[]}}"#)),
        )
        .await;
    }
    let lines = logfmt::take_capture();

    let posts: Vec<String> = lines
        .iter()
        .map(|line| {
            Regex::new(r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d ")
                .unwrap()
                .replace(line, "")
                .into_owned()
        })
        .filter(|line| line.starts_with("POST "))
        .collect();
    assert_eq!(posts.len(), 2, "{posts:#?}");
    assert!(
        posts[0].ends_with("| tok 18234 in (18200 cached) -> 891 out (742 reasoning)"),
        "every number the engine sent: {}",
        posts[0]
    );
    assert!(
        posts[1].ends_with("| tok 18234 in -> 891 out (742 reasoning)"),
        "no cache detail was reported, so none is printed: {}",
        posts[1]
    );
}

#[tokio::test]
async fn an_upstream_error_and_a_415_keep_their_levels() {
    let _guard = plain_capture().await;
    let up = upstream(Health::Json(vec!["zstd"]), Reply::RejectEncoded).await;
    let fwd = forwarder(
        &[
            ("model-alpha", &up.base),
            ("model-zeta", &common::dead_base()),
        ],
        &[],
    )
    .await;
    let big = Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "model-alpha",
            "messages": [{"role": "user", "content": "x".repeat(4000)}],
        }))
        .unwrap(),
    );
    fwd.post("/v1/chat/completions", big).await;
    fwd.post(
        "/v1/chat/completions",
        Bytes::from(r#"{"model":"model-zeta"}"#),
    )
    .await;
    let lines = logfmt::take_capture();

    assert!(
        lines
            .iter()
            .any(|l| l.contains("WARNING 415 for zstd: resending identity, encoding OFF")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("ERROR POST /v1/chat/completions -> upstream error")),
        "{lines:#?}"
    );
}

#[test]
fn log_helpers_format_plain_and_ansi() {
    let _guard = serialized_blocking();
    logfmt::set_color(false);
    assert_eq!(logfmt::human_time(0.0123), "12ms");
    assert_eq!(logfmt::human_time(17.441), "17.44s");
    assert_eq!(logfmt::human_time(95.4), "1m35.4s");
    assert_eq!(logfmt::human(0), "0B");
    assert_eq!(logfmt::human(51), "51B");
    assert_eq!(logfmt::human(52), "0.1KB");
    assert_eq!(logfmt::human(322), "0.3KB");
    assert_eq!(logfmt::human(512), "0.5KB");
    assert_eq!(logfmt::human(1024), "1KB");
    assert_eq!(logfmt::human(705_331), "689KB");
    assert_eq!(logfmt::human(5 << 20), "5.0MB");
    assert_eq!(logfmt::status(404), "404");

    logfmt::set_color(true);
    assert_eq!(logfmt::status(200), "\x1b[32m200\x1b[0m");
    assert_eq!(logfmt::status(503), "\x1b[31m503\x1b[0m");
    logfmt::set_color(false);
}

#[test]
fn the_log_formatter_drops_the_info_level() {
    let _guard = serialized_blocking();
    logfmt::set_color(false);
    let info = logfmt::format_record(logfmt::Level::Info, "hello world");
    assert!(info.ends_with(" hello world"));
    assert!(!info.contains("INFO"));
    // A written line carries its date, so a file spanning days can be read.
    assert!(
        Regex::new(r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d hello world$")
            .unwrap()
            .is_match(&info),
        "{info}"
    );
    assert!(logfmt::format_record(logfmt::Level::Warning, "careful").contains("WARNING careful"));
}

#[tokio::test]
async fn the_drain_watch_records_the_ack_zero_crossing() {
    use std::os::fd::AsRawFd;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    {
        use std::io::{Read, Write};
        (&client).write_all(&[b'x'; 4096]).unwrap();
        let mut sink = vec![0u8; 4096];
        server.read_exact(&mut sink).unwrap();
    }

    let clock = std::sync::Arc::new(PhaseClock::new());
    clock.set_fd(client.as_raw_fd());
    clock.mark_upload_started();
    portway::ack::watch_drain(std::sync::Arc::clone(&clock)).await;
    assert!(
        clock.upload_wall().is_some(),
        "the send queue should have drained"
    );
    assert!(clock.upload_wall().unwrap() >= 0.0);
}

#[tokio::test]
async fn upload_wall_falls_back_when_the_counter_is_unreachable() {
    // No fd was ever captured: nothing to watch, nothing to report.
    let blind = std::sync::Arc::new(PhaseClock::new());
    portway::ack::watch_drain(std::sync::Arc::clone(&blind)).await;
    assert_eq!(blind.upload_wall(), None);

    // A closed fd fails the counter read; the write block is what is left.
    let dead = std::sync::Arc::new(PhaseClock::new());
    dead.set_fd(-1);
    dead.mark_upload_started();
    dead.mark_body_handed();
    assert!(dead.mark_upload_finished());
    portway::ack::watch_drain(std::sync::Arc::clone(&dead)).await;
    assert!(
        dead.upload_wall().is_some(),
        "write-block time is the fallback"
    );
}

#[test]
fn the_pool_keeps_connections_for_five_minutes() {
    // httpx silently ignored AsyncClient(limits=...) once a custom transport
    // was passed, and the 5s default expiry re-paid dns+tcp+tls on nearly
    // every request after a few idle seconds. These are ours, so they hold.
    assert_eq!(portway::pool::IDLE_TIMEOUT.as_secs(), 300);
    assert_eq!(portway::pool::MAX_IDLE, 8);
}
