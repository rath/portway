//! `--web`: the dashboard in a browser.
//!
//! Everything the terminal dashboard shows, computed by the same code, plus
//! the recorded history and the controls a daemon otherwise needs a second
//! shell for.
//! The server is one hyper listener of its own beside the forwarder's; the
//! page is a handful of static files compiled into the binary.

pub mod address;
mod aggregate;
mod api;
pub mod assets;
mod auth;
mod body;
mod file;
mod http;
pub mod launch;
mod ring;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};

pub use aggregate::Feed;
pub use file::{WEB_FILE, console_urls};

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
    /// `--web-allow-host`: names the host check accepts besides `localhost`
    /// and IP literals.
    pub allow: Vec<String>,
    /// `--web-base-path`: the URL prefix the console is published under
    /// (`/` for the root, the default). Already normalized by the CLI parser.
    pub base: String,
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
    /// Every address the console answers on, with the run's token: printed
    /// once, to whoever started this, and written to the 0600 `portway.web`.
    /// Never logged. The first is the one to open on this machine.
    pub urls: Vec<String>,
    /// The same addresses without the token: safe for the log.
    pub public_urls: Vec<String>,
    /// The run's token, for a daemon to hand its launcher once.
    pub token: String,
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
            allow,
            base,
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
        let names = address::allowed_names(&host, &allow);
        // The printed link must name the prefix, or it would point at the
        // proxy's root, where nothing answers.
        let prefix = if base == "/" {
            String::new()
        } else {
            base.clone()
        };
        let public_urls: Vec<String> = address::authorities(&host, &listener, local, &names)
            .iter()
            .map(|authority| format!("http://{authority}{prefix}/"))
            .collect();
        let auth = auth::Auth::new(local.port(), names).map_err(|err| format!("entropy: {err}"))?;
        let urls: Vec<String> = public_urls
            .iter()
            .map(|url| format!("{url}#token={}", auth.token()))
            .collect();
        let launch_url = format!("{}#token={}", public_urls[0], auth.launch_code());
        let token = auth.token().to_owned();
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
            "version": crate::VERSION,
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
            base,
        });
        let server = tokio::spawn(http::serve(listener, app, closed));
        crate::store::ensure_dir(&dir)?;
        let file = file::WebFile::claim(&dir, &urls)?;
        Ok(Console {
            urls,
            public_urls,
            token,
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
