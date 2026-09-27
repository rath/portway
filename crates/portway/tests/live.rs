//! The feature-independent CLI socket against real delayed/streaming TCP, plus
//! an attached terminal that can leave without cancelling the serving process.
use std::future::Future;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use portway::flights::Phase;
use portway::live::{self, Snapshot, Target};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const BIN: &str = env!("CARGO_BIN_EXE_portway");

async fn within<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("operation timed out")
}

/// Read the terminal until `text` has been drawn on it, failing if the viewer
/// exits first.
#[cfg(feature = "tui")]
async fn drawn(master: &mut std::fs::File, viewer: &mut Process, screen: &mut Vec<u8>, text: &str) {
    within(async {
        loop {
            let mut bytes = [0; 16384];
            match std::io::Read::read(master, &mut bytes) {
                Ok(read) => screen.extend_from_slice(&bytes[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("terminal: {error}"),
            }
            if String::from_utf8_lossy(screen).contains(text) {
                break;
            }
            assert!(viewer.0.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Server {
    process: Process,
    dir: PathBuf,
    port: u16,
    target: Target,
}
impl Server {
    async fn start(name: &str, upstream: &TcpListener, block_socket: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("pw-live-cli-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Pin configuration so the user's own config cannot affect this test.
        std::fs::write(dir.join("config.toml"), "").unwrap();
        if block_socket {
            std::fs::write(dir.join(live::SOCKET_FILE), "keep").unwrap();
        }
        let free = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let child = Command::new(BIN)
            .args(["--config"])
            .arg(dir.join("config.toml"))
            .args(["--data-dir"])
            .arg(&dir)
            .args([
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--coding",
                "off",
                "--upstream",
            ])
            .arg(format!("http://{}", upstream.local_addr().unwrap()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.join("stderr")).unwrap())
            .spawn()
            .unwrap();
        let mut server = Self {
            process: Process(child),
            dir,
            port,
            target: Target::resolve("127.0.0.1", port).await.unwrap(),
        };
        within(async {
            loop {
                assert!(
                    server.process.0.try_wait().unwrap().is_none(),
                    "server exited"
                );
                if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        server
    }

    async fn snapshot(&self, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        within(async {
            loop {
                if let Ok(snapshot) = live::fetch(&self.dir, &self.target).await
                    && predicate(&snapshot)
                {
                    return snapshot;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    }

    async fn request(&self, upstream: &TcpListener) -> (TcpStream, TcpStream) {
        let mut client = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        client.write_all(b"GET /v1/models?secret=hidden HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: hidden\r\n\r\n").await.unwrap();
        let (mut origin, _) = within(upstream.accept()).await.unwrap();
        within(async {
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(origin.read_u8().await.unwrap());
            }
        })
        .await;
        (client, origin)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.process.0.kill();
        let _ = self.process.0.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn cli_reports_prefill_stream_progress_completion_and_cancellation() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = Server::start("stream", &upstream, false).await;
    let (mut client, mut origin) = server.request(&upstream).await;
    let first = server.snapshot(|s| s.total == 1).await;
    assert_eq!(first.listen.port(), server.port);
    assert_eq!(first.flights[0].phase, Phase::Prefill);
    assert_eq!(first.flights[0].path, "/v1/models");
    assert!(!serde_json::to_string(&first).unwrap().contains("hidden"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    origin.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nhell\r\n").await.unwrap();
    let streamed = server
        .snapshot(|s| s.flights.first().is_some_and(|f| f.received >= 4))
        .await;
    assert_eq!(streamed.flights[0].phase, Phase::Stream);
    assert!(streamed.flights[0].ttfb.unwrap() >= 0.04);
    origin.write_all(b"4\r\nmore\r\n").await.unwrap();
    server
        .snapshot(|s| s.flights.first().is_some_and(|f| f.received >= 8))
        .await;
    origin.write_all(b"0\r\n\r\n").await.unwrap();
    drop(origin);
    let mut answer = Vec::new();
    within(client.read_to_end(&mut answer)).await.unwrap();
    server
        .snapshot(|s| s.total == 0 && s.flights.is_empty())
        .await;

    let (client, mut origin) = server.request(&upstream).await;
    origin.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\ndata\r\n").await.unwrap();
    server
        .snapshot(|s| s.flights.first().is_some_and(|f| f.received >= 4))
        .await;
    drop(client);
    server.snapshot(|s| s.total == 0).await;

    // A prefill cancellation is removed as well, without waiting for headers.
    let (client, _origin) = server.request(&upstream).await;
    server.snapshot(|s| s.total == 1).await;
    drop(client);
    server.snapshot(|s| s.total == 0).await;
}

#[tokio::test]
async fn socket_creation_failure_warns_but_forwarding_continues() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = Server::start("blocked", &upstream, true).await;
    #[cfg(any(feature = "tui", feature = "web"))]
    let (watch, events) = {
        let (sender, events) = std::sync::mpsc::channel();
        let watch =
            portway::watch::spawn(&server.dir.join(portway::store::DB_FILE), sender).unwrap();
        (watch, events)
    };
    let (mut client, mut origin) = server.request(&upstream).await;
    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .await
        .unwrap();
    drop(origin);
    let mut bytes = Vec::new();
    within(client.read_to_end(&mut bytes)).await.unwrap();
    assert!(bytes.starts_with(b"HTTP/1.1 200"), "{bytes:?}");
    assert!(bytes.windows(2).any(|part| part == b"ok"), "{bytes:?}");
    assert_eq!(
        std::fs::read_to_string(server.dir.join(live::SOCKET_FILE)).unwrap(),
        "keep"
    );
    assert!(
        std::fs::read_to_string(server.dir.join("stderr"))
            .unwrap()
            .contains("live snapshots unavailable")
    );
    #[cfg(any(feature = "tui", feature = "web"))]
    {
        // The same DB reader used by old-server attach still receives history
        // while no live socket can be reached.
        let record = within(async {
            loop {
                for event in events.try_iter() {
                    if let portway::telemetry::Event::Request(record) = event {
                        return record;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert_eq!(record.status, 200);
        assert_eq!(record.received, 2);
        assert!(record.flight.is_none());
        watch.shutdown();
    }
}

#[cfg(feature = "tui")]
#[tokio::test]
async fn attached_q_leaves_server_and_request_running() {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = Server::start("quit", &upstream, false).await;
    let (mut client, mut origin) = server.request(&upstream).await;
    server.snapshot(|s| s.total == 1).await;
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: 30,
        ws_col: 140,
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
    let master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    assert_ne!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
        -1
    );
    let mut command = Command::new(BIN);
    command
        .args(["--config"])
        .arg(server.dir.join("config.toml"))
        .args(["--data-dir"])
        .arg(&server.dir)
        .args([
            "--tui",
            "--host",
            "127.0.0.1",
            "--port",
            &server.port.to_string(),
        ])
        .env("TERM", "xterm-256color")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut viewer = Process(command.spawn().unwrap());
    // Close the master before reaping the child on failure. Darwin can wait
    // for terminal output to drain even while the child is exiting.
    let mut master = master;
    let mut screen = Vec::new();
    // The in-flight list is a dialog: once the dashboard is up, `f` opens it
    // and `f` closes it again, and then `q` is the viewer's quit.
    drawn(&mut master, &mut viewer, &mut screen, "events").await;
    std::io::Write::write_all(&mut master, b"f").unwrap();
    drawn(&mut master, &mut viewer, &mut screen, "prefill").await;
    std::io::Write::write_all(&mut master, b"f").unwrap();
    std::io::Write::write_all(&mut master, b"q").unwrap();
    let status = within(async {
        loop {
            let mut remaining = [0; 16384];
            let _ = std::io::Read::read(&mut master, &mut remaining);
            if let Some(status) = viewer.0.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(status.success());
    assert_eq!(
        live::fetch(&server.dir, &server.target)
            .await
            .unwrap()
            .total,
        1
    );
    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .await
        .unwrap();
    drop(origin);
    let mut bytes = Vec::new();
    within(client.read_to_end(&mut bytes)).await.unwrap();
    assert!(bytes.starts_with(b"HTTP/1.1 200"), "{bytes:?}");
    assert!(bytes.windows(2).any(|part| part == b"ok"), "{bytes:?}");
    server.snapshot(|s| s.total == 0).await;
}
