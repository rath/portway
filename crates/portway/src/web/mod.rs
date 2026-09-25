//! `--web`: the dashboard in a browser.
//!
//! Everything the terminal dashboard shows, computed by the same code, plus
//! what a terminal cannot hold: the requests still in flight, the recorded
//! history, and the controls a daemon otherwise needs a second shell for.
//! The server is one hyper listener of its own beside the forwarder's; the
//! page is a handful of static files compiled into the binary.

mod aggregate;
mod api;
pub mod assets;
mod auth;
mod body;
mod file;
mod http;
pub mod launch;
mod ring;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};

pub use aggregate::Feed;
pub use file::{WEB_FILE, console_url};

use crate::cli::Args;
use crate::config::Prices;
use crate::control::RouterCell;
use crate::logfmt;
use crate::telemetry::Event;

/// The default `--web-port`: the forwarder's 8787 and the two ports the docs
/// already use for a second instance and a sidecar are left alone.
pub const DEFAULT_PORT: u16 = 8790;

/// What the stop and reload buttons do.
pub enum Control {
    /// This process serves the forwarder: reload rebuilds its router, stop
    /// ends it the way SIGTERM would.
    Live {
        args: Box<Args>,
        cell: RouterCell,
        stop: Arc<Notify>,
    },
    /// Watching another forwarder's database: both go to its daemon, through
    /// the pid file, the way `--reload` and `--stop` do.
    Attached { dir: PathBuf },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Live,
    Daemon,
    Attached,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Live => "live",
            Mode::Daemon => "daemon",
            Mode::Attached => "attached",
        }
    }
}

/// The header line: what the terminal dashboard prints across its top.
pub struct Header {
    pub listen: String,
    pub coding: String,
    pub mode: Mode,
    /// The window an attached console reads, in the slot a live one shows its
    /// coding in.
    pub window: Option<Duration>,
}

pub struct Options {
    pub host: String,
    pub port: u16,
    pub feed: Feed,
    pub events: Receiver<Event>,
    pub header: Header,
    pub control: Control,
    /// The database usage and history read; `None` leaves both explaining why.
    pub db: Option<PathBuf>,
    pub prices: Prices,
    /// Where `portway.web` goes.
    pub dir: PathBuf,
}

/// A running console. Dropping it without `shutdown` leaves the listener to
/// the runtime; `shutdown` is what tells the pages and removes the file.
pub struct Console {
    /// The address with the run's token: printed once, to whoever started
    /// this, and written to the 0600 `portway.web`. Never logged.
    pub url: String,
    /// The same address without the token: safe for the log.
    pub public_url: String,
    /// The address with the one-time launch code instead of the token: what
    /// a browser this run opens is handed (see `launch`).
    pub launch_url: String,
    closing: watch::Sender<bool>,
    server: tokio::task::JoinHandle<()>,
    board: aggregate::Handle,
    _file: Option<file::WebFile>,
}

impl Console {
    /// Bind, start the board thread and the listener, and claim `portway.web`.
    pub async fn start(options: Options) -> Result<Console, String> {
        let Options {
            host,
            port,
            feed,
            events,
            header,
            control,
            db,
            prices,
            dir,
        } = options;
        let listener = TcpListener::bind((host.as_str(), port))
            .await
            .map_err(|err| format!("--web {host}:{port}: {err}"))?;
        let local = listener
            .local_addr()
            .map_err(|err| format!("--web {host}:{port}: {err}"))?;
        let auth = auth::Auth::new(local.port()).map_err(|err| format!("entropy: {err}"))?;
        let public_url = format!("http://{}/", url_authority(&host, local.ip(), local.port()));
        let url = format!("{public_url}#token={}", auth.token());
        let launch_url = format!("{public_url}#token={}", auth.launch_code());
        if !local.ip().is_loopback() {
            logfmt::warn(&format!(
                "console on {local} is reachable from the network: token-protected, not encrypted"
            ));
        }

        let board = aggregate::spawn(feed, events).map_err(|err| format!("web board: {err}"))?;
        let started_unix = logfmt::epoch();
        let header = json!({
            "listen": header.listen,
            "coding": header.coding,
            "mode": header.mode.name(),
            "window_s": header.window.map(|window| window.as_secs()),
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "started_unix": started_unix,
            "uptime_s": 0.0,
            "control": {"reload": true, "stop": true},
            "db": db.is_some(),
            "prices": !prices.is_empty(),
        });
        let (closing, closed) = watch::channel(false);
        let app = Arc::new(http::App {
            auth,
            shared: Arc::clone(&board.shared),
            header,
            control,
            db,
            prices,
            streams: AtomicUsize::new(0),
            closing: closed.clone(),
        });
        let server = tokio::spawn(http::serve(listener, app, closed));
        crate::store::ensure_dir(&dir)?;
        let file = file::WebFile::claim(&dir, &url)?;
        Ok(Console {
            url,
            public_url,
            launch_url,
            closing,
            server,
            board,
            _file: file,
        })
    }

    /// Tell the pages, close every stream and connection, stop the board and
    /// give `portway.web` back.
    pub async fn shutdown(self) {
        self.board.shared.announce(json!({"event": "stopping"}));
        let _ = self.closing.send(true);
        let _ = self.server.await;
        let board = self.board;
        let _ = tokio::task::spawn_blocking(move || board.shutdown()).await;
    }
}

/// The host part of the console's address. The listener only answers to
/// `localhost` and IP literals, so a name that is neither is replaced by the
/// address it bound, and a wildcard by loopback.
fn url_authority(host: &str, bound: IpAddr, port: u16) -> String {
    let ip = match bound {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    if host.eq_ignore_ascii_case("localhost") {
        return format!("localhost:{port}");
    }
    match ip {
        IpAddr::V4(ip) => format!("{ip}:{port}"),
        IpAddr::V6(ip) => format!("[{ip}]:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_is_one_the_host_check_accepts() {
        let v4 = |text: &str| text.parse::<IpAddr>().unwrap();
        assert_eq!(url_authority("0.0.0.0", v4("0.0.0.0"), 1), "127.0.0.1:1");
        assert_eq!(url_authority("::", v4("::"), 1), "[::1]:1");
        assert_eq!(
            url_authority("localhost", v4("127.0.0.1"), 1),
            "localhost:1"
        );
        assert_eq!(url_authority("box.lan", v4("10.0.0.2"), 1), "10.0.0.2:1");
    }
}
