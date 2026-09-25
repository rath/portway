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
use common::{Health, Reply, upstream};
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

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
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
    let port = base
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .rsplit_once(':')?
        .1
        .parse()
        .ok()?;
    Some(Launched {
        port,
        token: token.to_string(),
        url,
    })
}

/// A foreground `--web`, with stderr read until the console line appears.
fn spawn_console(dir: &Path, args: &[&str]) -> (Child, Launched) {
    let mut child = Command::new(BIN)
        .args(args)
        .arg("--data-dir")
        .arg(dir)
        .args(["--web", "--web-port", "0"])
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

fn exited(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
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
        .output()
        .unwrap();
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

    let mut page = Page::new(console.port);
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
    unsafe { libc::kill(watcher.id() as i32, libc::SIGTERM) };
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
