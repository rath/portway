//! The actual local TUI, pointed at a separate process's console and data dir.
use super::*;
use portway::remote::{Update, transport::Client, wire};
use portway::tui::state::State;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;

struct Viewer {
    process: Reap,
    master: File,
    _slave: File,
    screen: Vec<u8>,
}
impl Viewer {
    fn start(url: &str, dir: &Path) -> Self {
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 40,
            ws_col: 150,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            },
            0
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            -1
        );
        let mut command = Command::new(BIN);
        command
            .args(["--tui", "--attach", url, "--data-dir"])
            .arg(dir)
            .env("TERM", "xterm-256color")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            process: Reap(command.spawn().unwrap()),
            master,
            _slave: slave,
            screen: Vec::new(),
        }
    }
    fn drain(&mut self) {
        let mut bytes = [0; 32768];
        loop {
            match self.master.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => self.screen.extend_from_slice(&bytes[..n]),
                Err(error)
                    if matches!(error.kind(), std::io::ErrorKind::WouldBlock)
                        || error.raw_os_error() == Some(libc::EIO) =>
                {
                    break;
                }
                Err(error) => panic!("terminal read: {error}"),
            }
        }
    }
    async fn drawn(&mut self, text: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            self.drain();
            if String::from_utf8_lossy(&self.screen).contains(text) {
                return;
            }
            assert!(
                self.process.0.try_wait().unwrap().is_none(),
                "viewer exited: {}",
                String::from_utf8_lossy(&self.screen)
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "missing {text}: {}",
                String::from_utf8_lossy(&self.screen)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    fn keys(&mut self, keys: &str) {
        self.master.write_all(keys.as_bytes()).unwrap();
    }
    async fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            self.drain();
            if let Some(status) = self.process.0.try_wait().unwrap() {
                return status;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "viewer did not exit"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    fn restored(&self) {
        let mut attrs = std::mem::MaybeUninit::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(self.master.as_raw_fd(), attrs.as_mut_ptr()) },
            0
        );
        let attrs = unsafe { attrs.assume_init() };
        assert_ne!(attrs.c_lflag & libc::ECHO, 0);
        assert_ne!(attrs.c_lflag & libc::ICANON, 0);
    }
}
// A blocked Darwin PTY write must be released before waiting for a failed child.
impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.process.0.kill();
        self.drain();
    }
}

#[tokio::test]
async fn remote_terminal_authenticates_reuses_sessions_and_leaves_the_server_running() {
    let upstream = upstream(Health::Json(vec!["zstd"]), Reply::UsageJson).await;
    let dir = data_dir("remote-server");
    let viewer_dir = data_dir("remote-viewer");
    // This malformed config must never be loaded by a remote viewer.
    std::fs::write(viewer_dir.join("portway.toml"), "not valid toml [[[").unwrap();
    let config = config(&dir, &upstream.base);
    let port = free_port();
    let web_port = free_port().to_string();
    let args = [
        "--config",
        config.to_str().unwrap(),
        "--port",
        &port.to_string(),
        "--web-base-path",
        "/console",
    ];
    let (mut server, launched) = spawn_console_port(&dir, &args, &web_port);
    let url = launched.url.split('#').next().unwrap().to_string();
    record_one(port, r#"{"model":"model-web"}"#).await;

    let mut viewer = Viewer::start(&url, &viewer_dir);
    viewer.drawn("Console token").await;
    viewer.keys(&format!("{}\r", launched.token));
    viewer.drawn("events").await;
    viewer.drawn("model-web").await;
    assert!(!String::from_utf8_lossy(&viewer.screen).contains(&launched.token));
    viewer.keys("f");
    viewer.drawn("nothing in flight").await;
    viewer.keys("f");
    viewer.keys("u");
    viewer.drawn("usage —").await;
    viewer.keys("p");
    viewer.drawn("cost by source").await;
    viewer.keys("u");
    viewer.keys("q");
    assert!(viewer.wait().await.success());
    viewer.restored();
    assert!(server.0.try_wait().unwrap().is_none());
    assert!(!viewer_dir.join(DB_FILE).exists());
    assert!(!viewer_dir.join(portway::live::SOCKET_FILE).exists());
    let cache = viewer_dir.join("remote-sessions.json");
    assert_eq!(
        std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        !std::fs::read_to_string(&cache)
            .unwrap()
            .contains(&launched.token)
    );

    let mut viewer = Viewer::start(&url, &viewer_dir);
    viewer.drawn("events").await;
    assert!(!String::from_utf8_lossy(&viewer.screen).contains("Console token"));
    // A crash/redeploy keeps the viewer alive and its cached cookie valid.
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    viewer.drawn("reconnecting").await;
    viewer.screen.clear();
    let (mut server, restarted) = spawn_console_port(&dir, &args, &web_port);
    assert_eq!(launched.token, restarted.token);
    record_at(port, r#"{"model":"model-web"}"#, "/after-restart").await;
    viewer.drawn("after-restart").await;
    assert!(!String::from_utf8_lossy(&viewer.screen).contains("Console token"));
    viewer.keys("q");
    assert!(viewer.wait().await.success());
    viewer.restored();
    let mut viewer = Viewer::start(&url, &viewer_dir);
    viewer.drawn("after-restart").await;
    assert!(!String::from_utf8_lossy(&viewer.screen).contains("Console token"));

    // Explicitly resetting access still revokes an open viewer and its cache.
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    std::fs::remove_file(dir.join("web-auth.json")).unwrap();
    let (_server, reset) = spawn_console_port(&dir, &args, &web_port);
    assert_ne!(reset.token, restarted.token);
    assert!(!viewer.wait().await.success());
    assert!(String::from_utf8_lossy(&viewer.screen).contains("remote session is no longer valid"));
    viewer.restored();
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cache).unwrap()).unwrap();
    assert_eq!(saved.as_object().unwrap().len(), 0);
    let mut viewer = Viewer::start(&url, &viewer_dir);
    viewer.drawn("Console token").await;
    viewer.keys(&format!("{}\r", reset.token));
    viewer.drawn("events").await;
    viewer.keys("q");
    assert!(viewer.wait().await.success());
    viewer.restored();
}

#[tokio::test]
async fn remote_data_uses_server_totals_and_costs_without_recounting_events() {
    let upstream = upstream(Health::Json(vec!["zstd"]), Reply::UsageJson).await;
    let dir = data_dir("remote-data");
    let config = config(&dir, &upstream.base);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&config)
        .unwrap();
    writeln!(
        file,
        "\n[prices.\"model-web\"]\ninput = 3\noutput = 15\ncache_read = 0.3"
    )
    .unwrap();
    let port = free_port();
    let (_server, launched) = spawn_console(
        &dir,
        &[
            "--config",
            config.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--web-base-path",
            "/console",
        ],
    );
    let mut client = Client::new(launched.url.split('#').next().unwrap()).unwrap();
    assert!(client.login("invalid").await.is_err());
    client.login(&launched.token).await.unwrap();
    record_one(port, r#"{"model":"model-web"}"#).await;
    eventually("remote count", async || {
        let snapshot: wire::Snapshot = client.get("/api/snapshot").await.unwrap();
        snapshot.metrics.totals.requests == 1
    })
    .await;
    let snapshot: wire::Snapshot = client.get("/api/snapshot").await.unwrap();
    let latest: serde_json::Value = client.get("/api/snapshot").await.unwrap();
    let mut state = State::new();
    state.recorded = true;
    state.remote = Some(Default::default());
    state.apply_remote(Update::Snapshot(Box::new(snapshot)));
    assert_eq!(state.totals.requests, 1);
    assert_eq!(state.seen, 1);
    assert!(state.remote.as_ref().unwrap().latency.ttfb.p50.is_some());
    let mut request = latest["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "request")
        .unwrap()
        .clone();
    let seq = state.remote.as_ref().unwrap().seq;
    request["seq"] = (seq + 1).into();
    state.apply_remote(Update::Event(
        serde_json::from_value(request.clone()).unwrap(),
    ));
    let count = state.len();
    state.apply_remote(Update::Event(serde_json::from_value(request).unwrap()));
    assert_eq!(state.len(), count);
    assert_eq!(state.totals.requests, 1);
    assert_eq!(state.seen, 1);
    state.open_usage();
    let usage: wire::UsageReply = client.get("/api/usage?range=today").await.unwrap();
    assert!(usage.table.cost().is_some_and(|cost| cost > 0.0));
    state.apply_remote(Update::Usage(portway::spend::Range::Today, Ok(usage)));
    assert!(state.usage.as_ref().unwrap().cost().unwrap() > 0.0);
    state.apply_remote(Update::Connection(false, "reconnecting".into()));
    assert_eq!(state.totals.requests, 1);
    assert!(!state.flights_available);
    assert!(state.flights.is_empty());
}

#[tokio::test]
async fn cancelling_token_entry_restores_the_terminal_without_starting_a_proxy() {
    let upstream = upstream(Health::Json(vec![]), Reply::UsageJson).await;
    let dir = data_dir("remote-cancel-server");
    let config = config(&dir, &upstream.base);
    let (_server, launched) = spawn_console(
        &dir,
        &[
            "--config",
            config.to_str().unwrap(),
            "--port",
            &free_port().to_string(),
        ],
    );
    let viewer_dir = data_dir("remote-cancel-viewer");
    let mut viewer = Viewer::start(launched.url.split('#').next().unwrap(), &viewer_dir);
    viewer.drawn("Console token").await;
    viewer.keys("abcd\u{3}");
    assert!(!viewer.wait().await.success());
    viewer.restored();
    assert!(!String::from_utf8_lossy(&viewer.screen).contains("abcd"));
    assert!(!viewer_dir.join(DB_FILE).exists());
}

#[tokio::test]
async fn remote_flights_are_live_and_quitting_does_not_abort_them() {
    let upstream = upstream(Health::Json(vec!["zstd"]), Reply::Endless).await;
    let dir = data_dir("remote-flights-server");
    let viewer_dir = data_dir("remote-flights-viewer");
    let config = config(&dir, &upstream.base);
    let port = free_port();
    let (mut server, launched) = spawn_console(
        &dir,
        &[
            "--config",
            config.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ],
    );
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (mut send, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let response = send
        .send_request(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("host", format!("127.0.0.1:{port}"))
                .body(Full::new(Bytes::from_static(
                    b"{\"model\":\"model-web\",\"stream\":true}",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let mut held = response.into_body();
    held.frame().await.unwrap().unwrap();
    let mut client = Client::new(launched.url.split('#').next().unwrap()).unwrap();
    client.login(&launched.token).await.unwrap();
    eventually("streaming request", async || {
        let snapshot: wire::Snapshot = client.get("/api/snapshot").await.unwrap();
        snapshot
            .flights
            .is_some_and(|f| f.total == 1 && f.list[0].0.phase == portway::flights::Phase::Stream)
    })
    .await;
    // A new subscriber gets flights even when this idle stream never changes.
    let snapshot: wire::Snapshot = client.get("/api/snapshot").await.unwrap();
    let (mut stream, _lease) = client.stream(snapshot.seq).await.unwrap();
    let seed = tokio::time::timeout(Duration::from_secs(2), async {
        let mut text = String::new();
        loop {
            let frame = stream.frame().await.unwrap().unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(&String::from_utf8_lossy(&data));
            }
            if text.contains("event: flights") {
                break text;
            }
        }
    })
    .await
    .unwrap();
    assert!(seed.contains("\"total\":1"));
    let mut viewer = Viewer::start(launched.url.split('#').next().unwrap(), &viewer_dir);
    viewer.drawn("Console token").await;
    viewer.keys(&format!("{}\r", launched.token));
    viewer.drawn("events").await;
    viewer.keys("f");
    viewer.drawn("in flight · 1").await;
    viewer.drawn("stream").await;
    viewer.keys("fq");
    assert!(viewer.wait().await.success());
    assert!(server.0.try_wait().unwrap().is_none());
    let snapshot: wire::Snapshot = client.get("/api/snapshot").await.unwrap();
    assert_eq!(snapshot.flights.unwrap().total, 1);
    drop(held);
    drop(send);
    eventually("completed flight removed", async || {
        let snapshot: wire::Snapshot = client.get("/api/snapshot").await.unwrap();
        snapshot.flights.unwrap().total == 0
    })
    .await;
}

/// A local reverse proxy with the same Host rewrite as a deployed console.
/// Cutting its connections leaves the daemon and its session untouched.
struct Proxy {
    url: String,
    online: std::sync::Arc<std::sync::atomic::AtomicBool>,
    cut: std::sync::Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl Proxy {
    async fn start(backend: u16) -> Self {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/console/", listener.local_addr().unwrap());
        let online = Arc::new(AtomicBool::new(true));
        let cut = Arc::new(tokio::sync::Notify::new());
        let alive = Arc::clone(&online);
        let close = Arc::clone(&cut);
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let online = Arc::clone(&alive);
                        let cut = Arc::clone(&close);
                        connections.spawn(async move {
                            let service = hyper::service::service_fn(move |mut request: Request<hyper::body::Incoming>| {
                                let online = Arc::clone(&online);
                                async move {
                                    let response = if !online.load(Ordering::Relaxed) {
                                        http::Response::builder().status(503).body(Full::new(Bytes::new()).map_err(|never| match never {}).boxed()).unwrap()
                                    } else {
                                        request.headers_mut().insert("host", format!("127.0.0.1:{backend}").parse().unwrap());
                                        let tcp = tokio::net::TcpStream::connect(("127.0.0.1",backend)).await.unwrap();
                                        let (mut send, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await.unwrap();
                                        tokio::spawn(connection);
                                        send.send_request(request).await.unwrap().map(|body| body.boxed())
                                    };
                                    Ok::<_, std::convert::Infallible>(response)
                                }
                            });
                            tokio::select! {
                                _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service) => {},
                                _ = cut.notified() => {},
                            }
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {},
                }
            }
        });
        Self {
            url,
            online,
            cut,
            task,
        }
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn remote_reconnects_through_a_proxy_and_recovers_events_from_the_gap() {
    use std::sync::atomic::Ordering;
    let upstream = upstream(Health::Json(vec![]), Reply::UsageJson).await;
    let dir = data_dir("remote-reconnect-server");
    let viewer_dir = data_dir("remote-reconnect-viewer");
    let config = config(&dir, &upstream.base);
    let port = free_port();
    let (_server, launched) = spawn_console(
        &dir,
        &[
            "--config",
            config.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--web-base-path",
            "/console",
        ],
    );
    let proxy = Proxy::start(launched.port).await;
    let mut viewer = Viewer::start(&proxy.url, &viewer_dir);
    viewer.drawn("Console token").await;
    viewer.keys(&format!("{}\r", launched.token));
    viewer.drawn("events").await;
    proxy.online.store(false, Ordering::Relaxed);
    proxy.cut.notify_waiters();
    viewer.drawn("reconnecting").await;
    // This event is emitted while the viewer has no connection at all.
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (mut send, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let answer = send
        .send_request(
            Request::builder()
                .method("POST")
                .uri("/after-reconnect")
                .header("host", format!("127.0.0.1:{port}"))
                .body(Full::new(Bytes::from_static(b"{\"model\":\"model-web\"}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(answer.status(), 200);
    answer.into_body().collect().await.unwrap();
    viewer.screen.clear();
    proxy.online.store(true, Ordering::Relaxed);
    viewer.drawn("after-reconnect").await;
    assert!(!String::from_utf8_lossy(&viewer.screen).contains("Console token"));
    viewer.keys("q");
    assert!(viewer.wait().await.success());
    viewer.restored();
}

#[tokio::test]
async fn remote_backfills_events_older_than_the_initial_snapshot() {
    let upstream = upstream(Health::Json(vec![]), Reply::UsageJson).await;
    let dir = data_dir("remote-backfill-server");
    let viewer_dir = data_dir("remote-backfill-viewer");
    let config = config(&dir, &upstream.base);
    let port = free_port();
    let (_server, launched) = spawn_console(
        &dir,
        &[
            "--config",
            config.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ],
    );
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (mut send, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    send.send_request(
        Request::builder()
            .method("POST")
            .uri("/old-first")
            .header("host", format!("127.0.0.1:{port}"))
            .body(Full::new(Bytes::from_static(b"{\"model\":\"model-web\"}")))
            .unwrap(),
    )
    .await
    .unwrap()
    .into_body()
    .collect()
    .await
    .unwrap();
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..1001 {
        if requests.len() == 16 {
            requests.join_next().await.unwrap().unwrap();
        }
        requests.spawn(async move {
            record_one(port, r#"{"model":"model-web"}"#).await;
        });
    }
    while let Some(done) = requests.join_next().await {
        done.unwrap();
    }
    let mut client = Client::new(launched.url.split('#').next().unwrap()).unwrap();
    client.login(&launched.token).await.unwrap();
    eventually("all backfill records published", async || {
        let snapshot: wire::Snapshot = client.get("/api/snapshot").await.unwrap();
        snapshot.metrics.counts.seen == 1002
    })
    .await;
    let snapshot: serde_json::Value = client.get("/api/snapshot").await.unwrap();
    assert!(!snapshot["events"].to_string().contains("old-first"));
    let mut viewer = Viewer::start(launched.url.split('#').next().unwrap(), &viewer_dir);
    viewer.drawn("Console token").await;
    viewer.keys(&format!("{}\r", launched.token));
    viewer.drawn("events").await;
    viewer.keys("g");
    viewer.drawn("old-first").await;
    viewer.keys("q");
    assert!(viewer.wait().await.success());
}
