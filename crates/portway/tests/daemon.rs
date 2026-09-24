//! `--daemon`, against the real binary: the launcher's readiness handshake,
//! the control flags, and the rows the whole session leaves behind.
//!
//! The upstream is the loopback mock from `common`, aimed at the child through
//! `PORTWAY_UPSTREAMS`, so nothing here needs the internet. The
//! launcher forks, so this test drives three processes: the forwarder, its
//! parent (which exits right away) and the mock, which lives in this one.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use bytes::Bytes;
use common::{Fwd, Health, Reply, upstream};
use portway::store::{DB_FILE, PID_FILE};
use rusqlite::{Connection, OpenFlags};

const BIN: &str = env!("CARGO_BIN_EXE_portway");

/// One data dir per test: the daemon and the assertions never share a file
/// with another test, and a rerun starts clean.
fn data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("portway-daemon-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A port nothing is listening on, for the daemon to bind.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn launch(dir: &Path, port: u16, upstreams: &str) -> Output {
    std::fs::create_dir_all(dir).unwrap();
    let config = dir.join("test.toml");
    let models: std::collections::BTreeMap<_, _> = upstreams
        .split(',')
        .map(|entry| entry.split_once('=').unwrap())
        .collect();
    std::fs::write(
        &config,
        format!(
            "[models]\n{}",
            models
                .iter()
                .map(|(k, v)| format!("{k:?} = {v:?}\n"))
                .collect::<String>()
        ),
    )
    .unwrap();
    Command::new(BIN)
        .arg("--config")
        .arg(config)
        .arg("--daemon")
        .arg("--data-dir")
        .arg(dir)
        .arg("--port")
        .arg(port.to_string())
        .output()
        .expect("the forwarder binary runs")
}

/// The one-shot flags, which take the pid file and nothing else.
fn oneshot(dir: &Path, flag: &str) -> Output {
    Command::new(BIN)
        .arg(flag)
        .arg("--data-dir")
        .arg(dir)
        .output()
        .expect("the forwarder binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

/// `daemon started (pid N), logging to …` → N.
fn launched_pid(output: &Output) -> i32 {
    let line = stdout(output);
    let pid = line
        .split_once("daemon started (pid ")
        .and_then(|(_, rest)| rest.split_once(')'))
        .unwrap_or_else(|| panic!("no pid in {line:?}"))
        .0;
    pid.parse().unwrap_or_else(|_| panic!("no pid in {line:?}"))
}

fn alive(pid: i32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The daemon binds its listener before it reports ready, so this is normally
/// instant; the loop is for a slow machine, not for a race.
async fn wait_for_listener(port: u16) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the daemon never bound port {port}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The log file is written by the daemon's own thread; a line appears shortly
/// after whatever produced it.
async fn wait_for_log(dir: &Path, needle: &str) -> String {
    let path = dir.join(portway::store::LOG_FILE);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no {needle:?} in {}:\n{text}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn open_db(dir: &Path) -> Connection {
    Connection::open_with_flags(dir.join(DB_FILE), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("the recorder created db.sqlite3")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_starts_serves_records_and_stops() {
    let upstream = upstream(Health::Json(vec!["zstd"]), Reply::Ok).await;
    let dir = data_dir("lifecycle");
    let port = free_port();
    let upstreams = format!("model-zeta={}", upstream.base);

    let launched = launch(&dir, port, &upstreams);
    assert!(
        launched.status.success(),
        "launcher: {} / {}",
        stdout(&launched),
        stderr(&launched)
    );
    let pid = launched_pid(&launched);
    let _cleanup = Cleanup(dir.clone());
    assert!(
        stdout(&launched).contains(&dir.join(portway::store::LOG_FILE).display().to_string()),
        "the launcher names the log file: {}",
        stdout(&launched)
    );

    // The pid file holds the pid the launcher was told about.
    let stored: i32 = std::fs::read_to_string(dir.join(PID_FILE))
        .expect("the daemon wrote its pid file")
        .trim()
        .parse()
        .unwrap();
    assert_eq!(stored, pid);

    wait_for_listener(port).await;
    let forwarder = Fwd {
        base: format!("http://127.0.0.1:{port}"),
    };
    let health = forwarder.get("/__portway/health").await;
    assert_eq!(health.status, 200);
    assert_eq!(health.json()["mode"], "router");

    // Comfortably over --min-bytes, so the negotiated coding is exercised too.
    let body = Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "model-zeta",
            "messages": [{"role": "user", "content": "daemon test ".repeat(200)}],
        }))
        .unwrap(),
    );
    let answer = forwarder.post("/v1/chat/completions", body).await;
    assert_eq!(answer.status, 200, "{:?}", answer.json());
    assert_eq!(upstream.calls().len(), 1, "the mock upstream saw the relay");

    let status = oneshot(&dir, "--status");
    assert!(status.status.success(), "{}", stderr(&status));
    assert_eq!(stdout(&status), format!("portway: running (pid {pid})"));

    let reload = oneshot(&dir, "--reload");
    assert!(reload.status.success(), "{}", stderr(&reload));
    wait_for_log(&dir, "SIGHUP: log reopened").await;

    let stop = oneshot(&dir, "--stop");
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert_eq!(stdout(&stop), format!("portway: stopped (pid {pid})"));
    assert!(
        !dir.join(PID_FILE).exists(),
        "the pid file outlived the daemon"
    );
    assert!(!alive(pid), "pid {pid} is still running");

    let not_running = oneshot(&dir, "--status");
    assert!(!not_running.status.success());
    assert!(
        stderr(&not_running).contains("not running"),
        "{}",
        stderr(&not_running)
    );

    // What the session left behind: the relay, and the lines the log printed.
    let db = open_db(&dir);
    let (model, code, complete, body_len, wire_len, received, coding): (
        String,
        i64,
        i64,
        i64,
        i64,
        i64,
        String,
    ) = db
        .query_row(
            "SELECT model, status, complete, body_len, wire_len, received, coding FROM requests",
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
                ))
            },
        )
        .expect("the relay is recorded");
    assert_eq!(model, "model-zeta");
    assert_eq!(code, 200);
    assert_eq!(complete, 1);
    assert!(body_len > 1024, "body_len {body_len}");
    assert!(wire_len < body_len, "{body_len} went out as {wire_len}");
    assert!(received > 0, "received {received}");
    assert_eq!(coding, "zstd");

    let logs: Vec<(i64, String)> = db
        .prepare("SELECT level, message FROM logs ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!logs.is_empty(), "the log lines were recorded");
    assert!(
        logs.iter()
            .any(|(_, message)| message.contains("SIGHUP: log reopened")),
        "{logs:?}"
    );
    assert!(
        logs.iter().any(|(_, message)| message.contains("stopping")),
        "{logs:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_daemon_on_the_same_data_dir_is_refused() {
    let upstream = upstream(Health::JsonBare, Reply::Ok).await;
    let dir = data_dir("second");
    let upstreams = format!("model-zeta={}", upstream.base);

    let first = launch(&dir, free_port(), &upstreams);
    assert!(first.status.success(), "{}", stderr(&first));
    let pid = launched_pid(&first);

    let second = launch(&dir, free_port(), &upstreams);
    assert!(!second.status.success(), "a second daemon started");
    let refused = stderr(&second);
    assert!(
        refused.contains(&format!("already running (pid {pid})")),
        "{refused}"
    );
    assert!(
        refused.contains(PID_FILE),
        "the pid file is named: {refused}"
    );

    let stop = oneshot(&dir, "--stop");
    assert!(stop.status.success(), "{}", stderr(&stop));
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_without_a_daemon_exits_nonzero() {
    let dir = data_dir("no-pid");

    let stop = oneshot(&dir, "--stop");
    assert_eq!(stop.status.code(), Some(1), "{}", stdout(&stop));
    assert!(stderr(&stop).contains("no pid file"), "{}", stderr(&stop));

    let status = oneshot(&dir, "--status");
    assert_eq!(status.status.code(), Some(1));
    assert!(
        stderr(&status).contains("no pid file"),
        "{}",
        stderr(&status)
    );
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = oneshot(&self.0, "--stop");
    }
}
