//! Same-user, local snapshots for an attached TUI. One connection returns one
//! JSON document followed by EOF; no request bytes or HTTP endpoint are needed.
//! The lock file is permanent: unlinking it would let two owners lock different
//! inodes. Only its holder may remove a stale socket or the socket on shutdown.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use http::Method;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::{JoinHandle, JoinSet};

use crate::flights::{FlightView, Flights, Phase};
use crate::forwarder::Coding;

pub const SOCKET_FILE: &str = "portway.live.sock";
const LOCK_FILE: &str = "portway.live.lock";
const VERSION: u32 = 1;
pub const MAX_FLIGHTS: usize = 200;
const MAX_RESPONSE: usize = 1024 * 1024;
const MAX_CLIENTS: usize = 8;
const TIMEOUT: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(250);
const RETRY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub instance: String,
    pub listen: SocketAddr,
    pub total: u64,
    pub models: BTreeMap<String, u64>,
    #[serde(with = "flight_list")]
    pub flights: Vec<FlightView>,
}

// Keep this local protocol's representation out of the core's HTTP API.
#[derive(Serialize, Deserialize)]
#[serde(remote = "FlightView")]
struct FlightData {
    id: u64,
    model: String,
    #[serde(with = "method")]
    method: Method,
    path: String,
    started_unix: f64,
    age: f64,
    idle: f64,
    #[serde(with = "PhaseData")]
    phase: Phase,
    body_len: u64,
    wire_len: u64,
    #[serde(with = "CodingData")]
    coding: Coding,
    status: Option<u16>,
    ttfb: Option<f64>,
    received: u64,
    received_wire: u64,
    received_agent: u64,
    retries: u32,
    upload: Option<f64>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Phase", rename_all = "snake_case")]
enum PhaseData {
    Upload,
    Prefill,
    Stream,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Coding", rename_all = "snake_case")]
enum CodingData {
    None,
    Zstd,
    Gzip,
    Dcz,
}

mod method {
    use super::*;

    pub fn serialize<S: serde::Serializer>(
        value: &Method,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(value.as_str())
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Method, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

mod flight_list {
    use super::*;

    pub fn serialize<S: serde::Serializer>(
        value: &[FlightView],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Borrowed<'a>(#[serde(with = "FlightData")] &'a FlightView);
        serializer.collect_seq(value.iter().map(Borrowed))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<FlightView>, D::Error> {
        #[derive(Deserialize)]
        struct Owned(#[serde(with = "FlightData")] FlightView);
        Ok(Vec::<Owned>::deserialize(deserializer)?
            .into_iter()
            .map(|flight| flight.0)
            .collect())
    }
}

struct Ownership {
    _lock: File,
    socket: PathBuf,
    inode: Option<(u64, u64)>,
}

impl Ownership {
    fn bind(dir: &Path) -> io::Result<(Self, UnixListener)> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(dir.join(LOCK_FILE))?;
        let metadata = lock.metadata()?;
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other("live lock is not owned by this user"));
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        let socket = dir.join(SOCKET_FILE);
        match fs::symlink_metadata(&socket) {
            Ok(metadata)
                if metadata.file_type().is_socket()
                    && metadata.uid() == unsafe { libc::geteuid() } =>
            {
                fs::remove_file(&socket)?;
            }
            Ok(_) => return Err(io::Error::other("live socket path is not an owned socket")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut owner = Self {
            _lock: lock,
            socket,
            inode: None,
        };
        let listener = UnixListener::bind(&owner.socket)?;
        let metadata = fs::symlink_metadata(&owner.socket)?;
        owner.inode = Some((metadata.dev(), metadata.ino()));
        fs::set_permissions(&owner.socket, fs::Permissions::from_mode(0o600))?;
        Ok((owner, listener))
    }
}

impl Drop for Ownership {
    fn drop(&mut self) {
        // Also avoid removing a path that was replaced while we were running.
        if let Ok(metadata) = fs::symlink_metadata(&self.socket)
            && self.inode == Some((metadata.dev(), metadata.ino()))
        {
            let _ = fs::remove_file(&self.socket);
        }
    }
}

fn same_user(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() == unsafe { libc::geteuid() } {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "live peer has a different UID",
        ))
    }
}

/// Aborting/dropping the handle cancels the listener and all its clients. The
/// socket is unlinked before releasing ownership, including on startup errors.
pub struct Server {
    task: JoinHandle<()>,
    _owner: Ownership,
}

impl Server {
    pub fn start(
        dir: &Path,
        listen: SocketAddr,
        telemetry: Arc<crate::telemetry::Telemetry>,
    ) -> io::Result<Self> {
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let instance: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let (owner, listener) = Ownership::bind(dir)?;
        let task = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = match accepted {
                            Ok(accepted) => accepted,
                            Err(error) => {
                                crate::logfmt::warn(&format!("live socket accept: {error}"));
                                tokio::time::sleep(RETRY).await;
                                continue;
                            }
                        };
                        if clients.len() >= MAX_CLIENTS || same_user(&stream).is_err() {
                            continue;
                        }
                        let telemetry = Arc::clone(&telemetry);
                        let instance = instance.clone();
                        clients.spawn(async move {
                            let _ = tokio::time::timeout(TIMEOUT, send(stream, listen, instance, telemetry.flights())).await;
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        Ok(Self {
            task,
            _owner: owner,
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// Stop serializing at the cap rather than allocating an unbounded JSON buffer
// first. Extremely large model/path metadata makes the live feed unavailable.
struct Limited(Vec<u8>);
impl Write for Limited {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_RESPONSE - self.0.len() {
            return Err(io::Error::other("live snapshot exceeds size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn send(
    mut stream: UnixStream,
    listen: SocketAddr,
    instance: String,
    flights: &Flights,
) -> io::Result<()> {
    let snapshot = flights.snapshot(MAX_FLIGHTS);
    let snapshot = Snapshot {
        version: VERSION,
        instance,
        listen,
        total: snapshot.total,
        models: snapshot.models,
        flights: snapshot.flights,
    };
    let mut bytes = Limited(Vec::new());
    serde_json::to_writer(&mut bytes, &snapshot)?;
    stream.write_all(&bytes.0).await?;
    stream.shutdown().await
}

/// The endpoint the attach probe selected. Resolve once, independently of the
/// renderer, and compare every response with the actual bound address.
#[derive(Clone)]
pub struct Target(Vec<SocketAddr>);

impl Target {
    pub async fn resolve(host: &str, port: u16) -> io::Result<Self> {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let addresses =
            tokio::time::timeout(TIMEOUT, tokio::net::lookup_host((host, port))).await??;
        Ok(Self(addresses.collect()))
    }

    fn matches(&self, listen: SocketAddr) -> bool {
        self.0.iter().any(|address| {
            address.port() == listen.port()
                && (address.ip() == listen.ip()
                    || (listen.ip().is_unspecified()
                        && address.ip().is_loopback()
                        && address.is_ipv4() == listen.is_ipv4()))
        })
    }
}

/// Read a fresh snapshot under one connect/read deadline; no requests overlap.
pub async fn fetch(dir: &Path, target: &Target) -> io::Result<Snapshot> {
    tokio::time::timeout(TIMEOUT, async {
        let stream = UnixStream::connect(dir.join(SOCKET_FILE)).await?;
        same_user(&stream)?;
        let mut bytes = Vec::new();
        stream
            .take((MAX_RESPONSE + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > MAX_RESPONSE {
            return Err(io::Error::other("live response exceeds size limit"));
        }
        let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
        snapshot.validate(target)?;
        Ok(snapshot)
    })
    .await?
}

impl Snapshot {
    fn validate(&self, target: &Target) -> io::Result<()> {
        let valid_instance = self.instance.len() == 32
            && self
                .instance
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        let count = self
            .models
            .values()
            .try_fold(0u64, |total, count| total.checked_add(*count));
        let mut previous = 0;
        let mut listed = BTreeMap::<&str, u64>::new();
        let valid_flights = self.flights.iter().all(|flight| {
            let ordered = flight.id > previous;
            previous = flight.id;
            *listed.entry(&flight.model).or_default() += 1;
            ordered
                && !flight.path.contains('?')
                && [flight.started_unix, flight.age, flight.idle]
                    .into_iter()
                    .chain(flight.ttfb)
                    .chain(flight.upload)
                    .all(|duration| duration.is_finite() && duration >= 0.0)
        });
        if self.version != VERSION
            || !valid_instance
            || !target.matches(self.listen)
            || count != Some(self.total)
            || self.flights.len() as u64 != self.total.min(MAX_FLIGHTS as u64)
            || !valid_flights
            || listed
                .iter()
                .any(|(model, count)| self.models.get(*model).is_none_or(|total| total < count))
        {
            return Err(io::Error::other("invalid or mismatched live snapshot"));
        }
        Ok(())
    }
}

/// A latest-value channel keeps a slow renderer from building a snapshot queue.
/// Failures publish `None` immediately. Each success replaces the entire list,
/// including when an instance changes and flight IDs start again at one.
pub struct Client {
    task: JoinHandle<()>,
    snapshots: tokio::sync::watch::Receiver<Option<Arc<Snapshot>>>,
}

impl Client {
    pub fn start(dir: PathBuf, host: String, port: u16) -> Self {
        let (sender, snapshots) = tokio::sync::watch::channel(None);
        let task = tokio::spawn(async move {
            loop {
                let target = match Target::resolve(&host, port).await {
                    Ok(target) => target,
                    Err(_) => {
                        sender.send_replace(None);
                        tokio::time::sleep(RETRY).await;
                        continue;
                    }
                };
                loop {
                    let started = tokio::time::Instant::now();
                    let snapshot = fetch(&dir, &target).await.ok().map(Arc::new);
                    let available = snapshot.is_some();
                    sender.send_replace(snapshot);
                    let interval = if available { POLL } else { RETRY };
                    tokio::time::sleep_until(started + interval).await;
                    if !available {
                        break;
                    }
                }
            }
        });
        Self { task, snapshots }
    }

    pub fn snapshots(&self) -> tokio::sync::watch::Receiver<Option<Arc<Snapshot>>> {
        self.snapshots.clone()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::Telemetry;

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("pw-live-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn address() -> SocketAddr {
        "127.0.0.1:8789".parse().unwrap()
    }
    fn target() -> Target {
        Target(vec![address()])
    }

    #[tokio::test]
    async fn ownership_permissions_stale_socket_and_restart() {
        let dir = Dir::new("owner");
        // A dead server can leave a socket behind without holding the lock.
        drop(UnixListener::bind(dir.0.join(SOCKET_FILE)).unwrap());
        let telemetry = Arc::new(Telemetry::default());
        let server = Server::start(&dir.0, address(), Arc::clone(&telemetry)).unwrap();
        let inode = fs::metadata(dir.0.join(LOCK_FILE)).unwrap().ino();
        for file in [SOCKET_FILE, LOCK_FILE] {
            assert_eq!(
                fs::metadata(dir.0.join(file)).unwrap().mode() & 0o777,
                0o600
            );
        }
        assert!(Server::start(&dir.0, address(), Arc::clone(&telemetry)).is_err());
        let first = fetch(&dir.0, &target()).await.unwrap();
        assert_eq!(first.total, 0);
        drop(server);
        assert!(!dir.0.join(SOCKET_FILE).exists());
        assert!(dir.0.join(LOCK_FILE).exists());
        let _server = Server::start(&dir.0, address(), telemetry).unwrap();
        assert_eq!(fs::metadata(dir.0.join(LOCK_FILE)).unwrap().ino(), inode);
        assert_ne!(
            first.instance,
            fetch(&dir.0, &target()).await.unwrap().instance
        );
    }

    #[tokio::test]
    async fn never_unlink_a_regular_file_or_a_symlink() {
        let dir = Dir::new("paths");
        let path = dir.0.join(SOCKET_FILE);
        fs::write(&path, "keep").unwrap();
        let telemetry = Arc::new(Telemetry::default());
        assert!(Server::start(&dir.0, address(), Arc::clone(&telemetry)).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep");
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(dir.0.join(LOCK_FILE), &path).unwrap();
        assert!(Server::start(&dir.0, address(), telemetry).is_err());
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }

    #[tokio::test]
    async fn snapshot_caps_only_the_list_and_keeps_all_counts_and_phases() {
        let dir = Dir::new("snapshot");
        let telemetry = Arc::new(Telemetry::default());
        let registry = telemetry.flights();
        let first = registry.begin("alpha", &Method::POST, "/v1/messages?secret=hidden", 100);
        for _ in 0..204 {
            registry.begin("beta", &Method::GET, "/v1/models", 0);
        }
        let _server = Server::start(&dir.0, address(), Arc::clone(&telemetry)).unwrap();
        let read = fetch(&dir.0, &target()).await.unwrap();
        assert_eq!(read.total, 205);
        assert_eq!(
            read.models,
            BTreeMap::from([("alpha".into(), 1), ("beta".into(), 204)])
        );
        assert_eq!(read.flights.len(), MAX_FLIGHTS);
        assert_eq!(read.flights.last().unwrap().id, 200);
        assert_eq!(read.flights[0].phase, Phase::Upload);
        let json = serde_json::to_string(&read).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("headers"));
        let clock = Arc::new(crate::clock::PhaseClock::new());
        first.attempt(&clock, 50, Coding::Zstd);
        clock.mark_upload_started();
        clock.mark_upload_finished();
        assert_eq!(
            fetch(&dir.0, &target()).await.unwrap().flights[0].phase,
            Phase::Prefill
        );
        first.responded(200, 0.3, 50, Coding::Zstd);
        first.progress(8192, 4096, 8192);
        let read = fetch(&dir.0, &target()).await.unwrap();
        assert_eq!(read.flights[0].phase, Phase::Stream);
        assert_eq!(read.flights[0].received, 8192);
        assert_eq!(read.flights[0].ttfb, Some(0.3));
        registry.end(first.id());
        let read = fetch(&dir.0, &target()).await.unwrap();
        assert_eq!(read.total, 204);
        assert_eq!(read.flights[0].id, 2);
        assert!(!read.models.contains_key("alpha"));
    }

    async fn reply(dir: &Path, bytes: Vec<u8>) -> JoinHandle<()> {
        let listener = UnixListener::bind(dir.join(SOCKET_FILE)).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = stream.write_all(&bytes).await;
        })
    }

    #[tokio::test]
    async fn absent_slow_oversize_and_invalid_peers_are_unavailable() {
        let dir = Dir::new("invalid");
        assert!(fetch(&dir.0, &target()).await.is_err());
        let listener = UnixListener::bind(dir.0.join(SOCKET_FILE)).unwrap();
        let stalled = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let started = tokio::time::Instant::now();
        assert_eq!(
            fetch(&dir.0, &target()).await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        stalled.abort();
        fs::remove_file(dir.0.join(SOCKET_FILE)).unwrap();
        let task = reply(&dir.0, vec![b' '; MAX_RESPONSE + 1]).await;
        assert!(fetch(&dir.0, &target()).await.is_err());
        task.await.unwrap();
        fs::remove_file(dir.0.join(SOCKET_FILE)).unwrap();

        let good = serde_json::json!({"version": VERSION, "instance": "a".repeat(32),
            "listen": address(), "total": 0, "models": {}, "flights": []});
        for (field, value) in [
            ("version", serde_json::json!(VERSION + 1)),
            ("instance", serde_json::json!("")),
            ("instance", serde_json::json!("z".repeat(32))),
            ("listen", serde_json::json!("127.0.0.1:8790")),
            ("listen", serde_json::json!("192.0.2.1:8789")),
            ("total", serde_json::json!(1)),
        ] {
            let mut invalid = good.clone();
            invalid[field] = value;
            let task = reply(&dir.0, serde_json::to_vec(&invalid).unwrap()).await;
            assert!(fetch(&dir.0, &target()).await.is_err(), "{invalid}");
            task.await.unwrap();
            fs::remove_file(dir.0.join(SOCKET_FILE)).unwrap();
        }
        assert!(target().matches("0.0.0.0:8789".parse().unwrap()));
        assert!(!target().matches("[::]:8789".parse().unwrap()));
        let ipv6 = Target::resolve("[::1]", 8789).await.unwrap();
        assert!(ipv6.matches("[::1]:8789".parse().unwrap()));
        assert!(ipv6.matches("[::]:8789".parse().unwrap()));
        assert!(!ipv6.matches("[::1]:8790".parse().unwrap()));
    }

    async fn changed(
        receiver: &mut tokio::sync::watch::Receiver<Option<Arc<Snapshot>>>,
    ) -> Option<Arc<Snapshot>> {
        tokio::time::timeout(Duration::from_secs(3), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        receiver.borrow_and_update().clone()
    }

    #[tokio::test]
    async fn client_clears_failures_reconnects_and_drops_old_instance_ids() {
        let dir = Dir::new("client");
        let telemetry = Arc::new(Telemetry::default());
        telemetry.flights().begin("old", &Method::GET, "/old", 0);
        let server = Server::start(&dir.0, address(), Arc::clone(&telemetry)).unwrap();
        let client = Client::start(dir.0.clone(), "127.0.0.1".into(), address().port());
        let mut receiver = client.snapshots();
        let first = changed(&mut receiver).await.unwrap();
        assert_eq!(first.flights[0].id, 1);
        drop(server);
        assert!(changed(&mut receiver).await.is_none());
        let next = Arc::new(Telemetry::default());
        next.flights().begin("new", &Method::GET, "/new", 0);
        let _server = Server::start(&dir.0, address(), Arc::clone(&next)).unwrap();
        let second = changed(&mut receiver).await.unwrap();
        assert_ne!(first.instance, second.instance);
        assert_eq!(second.flights[0].model, "new");
        assert_eq!(second.flights[0].id, 1);
        drop(client);
        assert_eq!(fetch(&dir.0, &target()).await.unwrap().total, 1);
    }

    #[tokio::test]
    async fn slow_readers_expire_and_oversize_server_responses_are_bounded() {
        let dir = Dir::new("limits");
        let telemetry = Arc::new(Telemetry::default());
        // This is larger than a Unix socket's send buffer, below the JSON cap.
        let flight = telemetry
            .flights()
            .begin("alpha", &Method::GET, &"/".repeat(800_000), 0);
        let _server = Server::start(&dir.0, address(), Arc::clone(&telemetry)).unwrap();
        let mut readers = Vec::new();
        for _ in 0..MAX_CLIENTS {
            readers.push(UnixStream::connect(dir.0.join(SOCKET_FILE)).await.unwrap());
        }
        tokio::time::sleep(TIMEOUT * 2).await;
        telemetry.flights().end(flight.id());
        assert_eq!(fetch(&dir.0, &target()).await.unwrap().total, 0);
        for mut stream in readers {
            let mut bytes = Vec::new();
            tokio::time::timeout(TIMEOUT, stream.read_to_end(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            assert!(bytes.len() <= MAX_RESPONSE);
        }
        telemetry
            .flights()
            .begin("huge", &Method::GET, &"/".repeat(MAX_RESPONSE + 1), 0);
        assert!(fetch(&dir.0, &target()).await.is_err());
    }
}
