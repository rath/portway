//! `--web`, against the real binary: the door (host, origin, CSRF, token and
//! cookie), the live snapshot and stream, the in-flight registry seen from
//! the outside, stop, and the console a daemon hosts or attaches to.

#![cfg(feature = "web")]

mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use bytes::Bytes;
use common::{Health, Reply, free_port, upstream};
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use portway::store::{DB_FILE, LOG_FILE};
use portway::web::WEB_FILE;

const BIN: &str = env!("CARGO_BIN_EXE_portway");

fn data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("portway-web-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(dir: &Path, upstream: &str) -> PathBuf {
    let path = dir.join("test.toml");
    std::fs::write(&path, format!("[models]\n\"model-web\" = {upstream:?}\n")).unwrap();
    path
}

/// What the launch line says: `http://127.0.0.1:PORT/#token=HEX`.
struct Launched {
    port: u16,
    token: String,
    url: String,
}

fn parse_console(line: &str) -> Option<Launched> {
    let url = line.split_once("console at ")?.1.trim().to_string();
    let (base, token) = url.split_once("#token=")?;
    // The authority ends at the first `/` after the scheme, so a base path
    // (`/portway/`) does not get mistaken for part of the port.
    let authority = base.trim_start_matches("http://").split('/').next()?;
    let port = authority.rsplit_once(':')?.1.parse().ok()?;
    Some(Launched {
        port,
        token: token.to_string(),
        url,
    })
}

/// A console process that is killed if the test panics before it ends it.
struct Reap(Child);

impl Drop for Reap {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// A daemon that is stopped if the test panics before it stops it.
struct StopDaemon(PathBuf);

impl Drop for StopDaemon {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .arg("--stop")
            .arg("--data-dir")
            .arg(&self.0)
            .output();
    }
}

/// A foreground `--web`, with stderr read until the console line appears.
fn spawn_console(dir: &Path, args: &[&str]) -> (Reap, Launched) {
    spawn_console_port(dir, args, "0")
}

fn spawn_console_port(dir: &Path, args: &[&str], web_port: &str) -> (Reap, Launched) {
    let mut child = Command::new(BIN)
        .args(args)
        .arg("--data-dir")
        .arg(dir)
        .args(["--web", "--web-port", web_port])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sent = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !sent && let Some(launched) = parse_console(&line) {
                let _ = sender.send(launched);
                sent = true;
            }
        }
    });
    let child = Reap(child);
    let launched = receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("the console line on stderr");
    (child, launched)
}

struct Answer {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Bytes,
}

impl Answer {
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("JSON: {}", String::from_utf8_lossy(&self.body)))
    }
}

struct Page {
    port: u16,
    cookie: Option<String>,
}

impl Page {
    fn new(port: u16) -> Self {
        Page { port, cookie: None }
    }

    async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Answer {
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .unwrap();
        let (mut send, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let mut builder = Request::builder().method(method).uri(path);
        if !headers.iter().any(|(name, _)| *name == "host") {
            builder = builder.header("host", format!("127.0.0.1:{}", self.port));
        }
        if let Some(cookie) = &self.cookie {
            builder = builder.header("cookie", cookie);
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap();
        let response = send.send_request(request).await.unwrap();
        let (parts, incoming) = response.into_parts();
        Answer {
            status: parts.status,
            headers: parts.headers,
            body: incoming.collect().await.unwrap().to_bytes(),
        }
    }

    async fn get(&self, path: &str) -> Answer {
        self.send("GET", path, &[], "").await
    }

    async fn post(&self, path: &str) -> Answer {
        self.send("POST", path, &[("x-portway-console", "1")], "")
            .await
    }

    /// Trade the token for the cookie, the way the page does on load.
    async fn sign_in(&mut self, token: &str) {
        let answer = self
            .send(
                "POST",
                "/api/session",
                &[
                    ("x-portway-console", "1"),
                    ("content-type", "application/json"),
                    ("origin", &format!("http://127.0.0.1:{}", self.port)),
                ],
                &format!("{{\"token\":\"{token}\"}}"),
            )
            .await;
        assert_eq!(answer.status, StatusCode::NO_CONTENT);
        let cookie = answer.headers["set-cookie"].to_str().unwrap().to_string();
        self.cookie = Some(cookie.split(';').next().unwrap().to_string());
    }

    /// The first `want` bytes of the event stream, or what came in 5s.
    async fn stream(&self, after: u64, want: usize) -> String {
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .unwrap();
        let (mut send, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let request = Request::builder()
            .uri(format!("/api/stream?after={after}"))
            .header("host", format!("127.0.0.1:{}", self.port))
            .header("cookie", self.cookie.as_deref().unwrap_or(""))
            .body(Full::new(Bytes::new()))
            .unwrap();
        let response = send.send_request(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let mut body = response.into_body();
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while text.len() < want {
            match tokio::time::timeout_at(deadline, body.frame()).await {
                Ok(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        text.push_str(&String::from_utf8_lossy(&data));
                    }
                }
                _ => break,
            }
        }
        text
    }
}

async fn eventually<F: AsyncFnMut() -> bool>(what: &str, mut check: F) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn exited(child: &mut Reap, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + within;
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            return Some(status);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn recorded_text(dir: &Path) -> String {
    let connection = rusqlite::Connection::open_with_flags(
        dir.join(DB_FILE),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut statement = connection.prepare("SELECT message FROM logs").unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_console_guards_its_door_shows_flights_and_stops() {
    let upstream = upstream(Health::Json(vec!["zstd"]), Reply::Endless).await;
    let dir = data_dir("live");
    let port = free_port();
    let config = config(&dir, &upstream.base);
    let (mut child, launched) = spawn_console(
        &dir,
        &[
            "--config",
            config.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ],
    );
    let mut page = Page::new(launched.port);

    // The door, before a session exists.
    assert_eq!(page.get("/api/health").await.json()["session"], false);
    assert_eq!(
        page.get("/api/snapshot").await.status,
        StatusCode::UNAUTHORIZED
    );
    let spoofed = page
        .send("GET", "/api/health", &[("host", "evil.example")], "")
        .await;
    assert_eq!(spoofed.status, StatusCode::FORBIDDEN);
    let no_csrf = page
        .send(
            "POST",
            "/api/session",
            &[],
            &format!("{{\"token\":\"{}\"}}", launched.token),
        )
        .await;
    assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
    let foreign = page
        .send(
            "POST",
            "/api/session",
            &[
                ("x-portway-console", "1"),
                ("origin", "http://evil.example"),
            ],
            &format!("{{\"token\":\"{}\"}}", launched.token),
        )
        .await;
    assert_eq!(foreign.status, StatusCode::FORBIDDEN);
    let wrong = page
        .send(
            "POST",
            "/api/session",
            &[("x-portway-console", "1")],
            "{\"token\":\"00\"}",
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);

    // The page itself needs no session: it holds no data.
    let index = page.get("/").await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(
        index.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(String::from_utf8_lossy(&index.body).contains("/js/app.js"));
    let etag = index.headers["etag"].to_str().unwrap().to_string();
    let again = page.send("GET", "/", &[("if-none-match", &etag)], "").await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    let script = page.get("/js/app.js").await;
    assert!(
        script.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert_eq!(page.get("/nope.js").await.status, StatusCode::NOT_FOUND);

    page.sign_in(&launched.token).await;
    assert_eq!(page.get("/api/health").await.json()["session"], true);
    let answer = page.get("/api/snapshot").await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.headers["cache-control"], "no-store");
    assert!(answer.headers.contains_key("content-security-policy"));
    let snapshot = answer.json();
    assert_eq!(snapshot["header"]["mode"], "live");
    assert_eq!(snapshot["flights"]["total"], 0);
    assert_eq!(snapshot["models"][0]["name"], "model-web");

    // Only the token-less address is recorded; the token stays on stderr.
    let file = dir.join(WEB_FILE);
    let meta = std::fs::metadata(&file).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777,
        0o600
    );
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains(&launched.url)
    );

    // A request held open upstream is in flight, streaming, until it is let go.
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (mut send, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("host", format!("127.0.0.1:{port}"))
        .body(Full::new(Bytes::from_static(
            b"{\"model\":\"model-web\",\"stream\":true}",
        )))
        .unwrap();
    let response = send.send_request(request).await.unwrap();
    let mut held = response.into_body();
    held.frame().await.unwrap().unwrap();
    let mut flight = serde_json::Value::Null;
    eventually("the flight is streaming", async || {
        let snapshot = page.get("/api/snapshot").await.json();
        flight = snapshot["flights"]["list"][0].clone();
        flight["phase"] == "stream"
    })
    .await;
    assert_eq!(flight["status"], 200);
    assert_eq!(flight["route"], "POST ../completions");
    let id = flight["id"].as_u64().unwrap();
    drop(held);
    drop(send);
    eventually("the record replaces the flight", async || {
        let snapshot = page.get("/api/snapshot").await.json();
        snapshot["flights"]["total"] == 0
            && snapshot["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["flight"] == id)
    })
    .await;

    // The stream replays the backlog after `after`, with ids.
    let text = page.stream(0, 64).await;
    assert!(text.starts_with("retry: 2000\n\n"), "{text}");
    assert!(text.contains("event: ev\nid: 1\n"), "{text}");
    let seq = page.get("/api/snapshot").await.json()["seq"]
        .as_u64()
        .unwrap();
    let older = page
        .get(&format!("/api/events?before={seq}&limit=1"))
        .await
        .json();
    assert_eq!(older["events"].as_array().unwrap().len(), 1);

    // History and usage read the same database the recorder writes.
    let report = page.get("/api/report?since=1h&text=1").await;
    assert_eq!(report.status, StatusCode::OK);
    assert!(
        report.json()["text"]
            .as_str()
            .unwrap()
            .starts_with("portway — ")
    );
    assert_eq!(
        page.get("/api/report?since=soon").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        page.get("/api/usage?range=week").await.json()["range"],
        "week"
    );

    // Stop is a POST with the header, and ends the process the orderly way.
    let refused = page.send("POST", "/api/stop", &[], "").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(page.post("/api/stop").await.status, StatusCode::ACCEPTED);
    let status = exited(&mut child, Duration::from_secs(10)).expect("the console stopped");
    assert!(status.success(), "{status}");
    assert!(!file.exists(), "portway.web outlived the console");
    assert!(!recorded_text(&dir).contains(&launched.token));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_hosts_its_console_and_another_attaches() {
    let upstream = upstream(Health::Json(vec!["zstd"]), Reply::Ok).await;
    let dir = data_dir("daemon");
    let port = free_port();
    let config = config(&dir, &upstream.base);
    let launched = Command::new(BIN)
        .args([
            "--config",
            config.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .arg("--data-dir")
        .arg(&dir)
        .args(["--daemon", "--web", "--web-port", "0"])
        .args(["--web-allow-host", "Portway-Box."])
        .output()
        .unwrap();
    let _daemon = StopDaemon(dir.clone());
    let out = String::from_utf8_lossy(&launched.stdout).to_string();
    assert!(
        launched.status.success(),
        "{out} / {}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let console = out
        .lines()
        .find_map(parse_console)
        .unwrap_or_else(|| panic!("no console line in {out}"));

    let status = Command::new(BIN)
        .arg("--status")
        .arg("--data-dir")
        .arg(&dir)
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&status.stdout).to_string();
    assert!(
        status.contains(&format!("console at {}", console.url)),
        "{status}"
    );
    // An allowed name is printed as a second link, and its door opens.
    let named = format!(
        "console at http://portway-box:{}/#token={}",
        console.port, console.token
    );
    assert!(out.contains(&named), "{out}");
    assert!(status.contains(&named), "{status}");

    let mut page = Page::new(console.port);
    let host = format!("portway-box:{}", console.port);
    let named = page
        .send("GET", "/api/health", &[("host", host.as_str())], "")
        .await;
    assert_eq!(named.status, StatusCode::OK);
    let host = format!("other-box:{}", console.port);
    let other = page
        .send("GET", "/api/health", &[("host", host.as_str())], "")
        .await;
    assert_eq!(other.status, StatusCode::FORBIDDEN);
    page.sign_in(&console.token).await;
    let snapshot = page.get("/api/snapshot").await.json();
    assert_eq!(snapshot["header"]["mode"], "daemon");
    let reloaded = page.post("/api/reload").await;
    assert_eq!(reloaded.status, StatusCode::OK, "{:?}", reloaded.json());
    assert_eq!(reloaded.json()["routes"], 1);

    // A second console on the running forwarder reads its database instead.
    let (mut watcher, attached) = spawn_console(&dir, &["--port", &port.to_string()]);
    let mut other = Page::new(attached.port);
    other.sign_in(&attached.token).await;
    let snapshot = other.get("/api/snapshot").await.json();
    assert_eq!(snapshot["header"]["mode"], "attached");
    assert!(snapshot["flights"].is_null());
    assert_eq!(other.post("/api/reload").await.status, StatusCode::ACCEPTED);
    // Its stop is the daemon's: the forwarder goes, the viewer stays.
    assert_eq!(other.post("/api/stop").await.status, StatusCode::OK);
    assert!(
        !dir.join(WEB_FILE).exists(),
        "the daemon's console outlived it"
    );
    assert_eq!(other.post("/api/stop").await.status, StatusCode::CONFLICT);
    unsafe { libc::kill(watcher.0.id() as i32, libc::SIGTERM) };
    assert!(exited(&mut watcher, Duration::from_secs(10)).is_some());

    let log = std::fs::read_to_string(dir.join(LOG_FILE)).unwrap();
    assert!(
        log.contains(&format!("console at http://127.0.0.1:{}/", console.port)),
        "{log}"
    );
    assert!(!log.contains(&console.token), "the token reached the log");
    assert!(!recorded_text(&dir).contains(&console.token));
    let _ = std::fs::remove_dir_all(&dir);
}

/// One buffered request through the forwarder, driven to completion so the
/// recorder has a row carrying the engine's usage.
async fn record_one(port: u16, body: &str) {
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (mut send, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("host", format!("127.0.0.1:{port}"))
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let response = send.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().collect().await.unwrap();
}

/// A reload carries the price table with the routes: a table added — or a rate
/// edited — in the file the daemon rereads applies to what the console has
/// already recorded, with no restart to drop the requests in flight.
#[tokio::test(flavor = "multi_thread")]
async fn a_reload_reprices_the_running_console() {
    let upstream = upstream(Health::Json(vec!["zstd"]), Reply::UsageJson).await;
    let dir = data_dir("reprice");
    let port = free_port();
    let config = dir.join("test.toml");
    let write = |price: Option<(f64, f64, f64)>| {
        let table = match price {
            Some((input, output, cache_read)) => format!(
                "\n[prices.\"model-web\"]\ninput = {input}\noutput = {output}\ncache_read = {cache_read}\n"
            ),
            None => String::new(),
        };
        std::fs::write(
            &config,
            format!("[models]\n\"model-web\" = {:?}\n{table}", upstream.base),
        )
        .unwrap();
    };
    let reload = || {
        let out = Command::new(BIN)
            .arg("--reload")
            .arg("--data-dir")
            .arg(&dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    write(None);
    let launched = Command::new(BIN)
        .args([
            "--config",
            config.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .arg("--data-dir")
        .arg(&dir)
        .args(["--daemon", "--web", "--web-port", "0"])
        .output()
        .unwrap();
    let _daemon = StopDaemon(dir.clone());
    let out = String::from_utf8_lossy(&launched.stdout).to_string();
    assert!(
        launched.status.success(),
        "{out} / {}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let console = out
        .lines()
        .find_map(parse_console)
        .unwrap_or_else(|| panic!("no console line in {out}"));
    let mut page = Page::new(console.port);
    page.sign_in(&console.token).await;

    // A day with tokens and no rates: recorded, and honestly unpriced.
    assert_eq!(
        page.get("/api/snapshot").await.json()["header"]["prices"],
        false
    );
    record_one(port, "{\"model\":\"model-web\"}").await;
    eventually("the recorded answer is unpriced", async || {
        let usage = page.get("/api/usage").await.json();
        usage["rows"][0]["model"] == "model-web" && usage["rows"][0]["charge"].is_null()
    })
    .await;

    // Adding the rate to the file prices what is already in the database.
    write(Some((1.0, 2.0, 0.5)));
    reload();
    let mut before = 0.0;
    eventually("the table added by the reload prices the day", async || {
        before = page.get("/api/usage").await.json()["rows"][0]["charge"]["total"]
            .as_f64()
            .unwrap_or(0.0);
        before > 0.0
    })
    .await;
    assert_eq!(
        page.get("/api/snapshot").await.json()["header"]["prices"],
        true
    );

    // And editing it reprices the same records, from the file it rereads.
    write(Some((2.0, 4.0, 1.0)));
    reload();
    let mut after = 0.0;
    eventually("the edited rate reaches the console", async || {
        after = page.get("/api/usage").await.json()["rows"][0]["charge"]["total"]
            .as_f64()
            .unwrap_or(0.0);
        after > 1.5 * before
    })
    .await;
    assert!(
        (after - 2.0 * before).abs() < 1e-12,
        "{before} should have doubled, not become {after}"
    );
    let log = recorded_text(&dir);
    assert_eq!(
        log.matches("configuration reloaded (1 route(s))").count(),
        2,
        "{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--web-base-path`: served under a prefix a proxy strips, the console must
/// keep the browser inside that prefix for its assets and API calls, while a
/// request that is not under the prefix is refused rather than served.
#[tokio::test]
async fn a_base_path_keeps_the_console_inside_its_prefix() {
    let upstream = upstream(Health::Json(vec!["zstd"]), Reply::Ok).await;
    let dir = data_dir("base-path");
    let mut child = Command::new(BIN)
        .arg("--config")
        .arg(config(&dir, &upstream.base))
        .arg("--data-dir")
        .arg(&dir)
        .args(["--web", "--web-port", "0", "--web-base-path", "portway/"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sent = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !sent && let Some(launched) = parse_console(&line) {
                let _ = sender.send(launched);
                sent = true;
            }
        }
    });
    let mut child = Reap(child);
    let launched = receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("the console line on stderr");
    // `portway/` was normalized to `/portway`, and the printed link names it.
    assert!(
        launched.url.contains("/portway/#token="),
        "{}",
        launched.url
    );

    let mut page = Page::new(launched.port);
    // The page is served under the prefix, with its assets pointing into it.
    let html = page.get("/portway/").await;
    assert_eq!(html.status, StatusCode::OK, "{:?}", html.status);
    let body = String::from_utf8_lossy(&html.body).to_string();
    assert!(body.contains(r#"href="/portway/css/app.css""#), "{body}");
    assert!(body.contains(r#"src="/portway/js/app.js""#), "{body}");
    assert!(!body.contains(r#"href="/css/"#), "{body}");

    // An asset under the prefix answers; the same path without it does not.
    assert_eq!(
        page.get("/portway/css/app.css").await.status,
        StatusCode::OK
    );
    assert_eq!(page.get("/css/app.css").await.status, StatusCode::NOT_FOUND);

    // `/portway` and `/portway/` are one page, not a redirect or a 404.
    assert_eq!(page.get("/portway").await.status, StatusCode::OK);

    // The JS carries the prefixed API paths.
    let js = page.get("/portway/js/api.js").await;
    let js = String::from_utf8_lossy(&js.body).to_string();
    assert!(js.contains(r#""/portway/api/health""#), "{js}");
    assert!(!js.contains(r#""/api/health""#), "{js}");

    // The API itself answers under the prefix, and only under it.
    assert_eq!(page.get("/portway/api/health").await.status, StatusCode::OK);
    assert_eq!(page.get("/api/health").await.status, StatusCode::NOT_FOUND);

    // A longer name that merely shares the prefix is not the console.
    assert_eq!(page.get("/portwayx/").await.status, StatusCode::NOT_FOUND);

    // Signing in still works through the prefixed endpoint. `sign_in` posts to
    // the root, which this console does not serve, so the prefixed call is
    // spelled out here and the cookie carried forward by hand.
    let answer = page
        .send(
            "POST",
            "/portway/api/session",
            &[
                ("x-portway-console", "1"),
                ("content-type", "application/json"),
                ("origin", &format!("http://127.0.0.1:{}", page.port)),
            ],
            &format!("{{\"token\":\"{}\"}}", launched.token),
        )
        .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    let cookie = answer.headers["set-cookie"].to_str().unwrap().to_string();
    page.cookie = Some(cookie.split(';').next().unwrap().to_string());
    let snapshot = page.get("/portway/api/snapshot").await;
    assert_eq!(snapshot.status, StatusCode::OK, "{:?}", snapshot.body);
    assert_eq!(snapshot.json()["header"]["mode"], "live");

    // The prefixed paths the browser was handed are exactly what sign-in used.
    assert!(js.contains(r#""/portway/api/session""#), "{js}");
    assert!(js.contains(r#""/portway/api/snapshot""#), "{js}");

    unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
    assert!(exited(&mut child, Duration::from_secs(10)).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "tui")]
#[path = "common/remote.rs"]
mod remote_tests;
