#[cfg(feature = "tui")]
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::SyncSender;

use clap::Parser;
#[cfg(any(feature = "tui", feature = "web"))]
use portway::cli::CodingArg;
use portway::cli::{Args, Mode};
use portway::config::Config;
use portway::control::{self, PricesCell, RouterCell};
use portway::router::STATS_PATH;
use portway::telemetry::{Event, Sinks};
#[cfg(feature = "tui")]
use portway::tui;
#[cfg(feature = "tui")]
use portway::tui::view::Header;
#[cfg(any(feature = "tui", feature = "web"))]
use portway::watch;
#[cfg(feature = "web")]
use portway::web;
use portway::{daemon, live, logfmt, report, server, store, telemetry};
#[cfg(feature = "tui")]
type Settings = tui::Settings;
#[cfg(not(feature = "tui"))]
type Settings = ();

/// jemalloc in place of glibc's malloc; the manifest says why.
#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Give freed pages back from jemalloc's own thread, so a server that goes
/// idle returns what its last burst freed instead of waiting for the next
/// allocation to do it. After the fork: jemalloc stops this thread in a child.
fn purge_in_background() {
    #[cfg(target_os = "linux")]
    {
        let mut on = true;
        // A refusal leaves purging to allocation time, which still happens.
        // SAFETY: a known boolean option, written from a live bool of the
        // size given, with no old value asked for.
        unsafe {
            tikv_jemalloc_sys::mallctl(
                c"background_thread".as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                (&raw mut on).cast(),
                size_of::<bool>(),
            );
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = Args::parse();

    // One-shot commands: no runtime, no listener, no recorder thread.
    if args.stop || args.reload || args.status {
        let dir = store::data_dir(args.data_dir.as_deref())?;
        daemon_message(if args.stop {
            daemon::stop(&dir)
        } else if args.reload {
            daemon::reload(&dir)
        } else {
            daemon::status(&dir).map(|status| with_console(status, &dir))
        });
    }
    if args.report {
        let dir = store::data_dir(args.data_dir.as_deref())?;
        match report::render(&dir.join(store::DB_FILE), args.since, args.model.as_deref()) {
            Ok(text) => {
                print!("{text}");
                return Ok(());
            }
            Err(message) => {
                eprintln!("portway: {message}");
                std::process::exit(1);
            }
        }
    }

    #[cfg(feature = "tui")]
    if args.tui && !std::io::stdout().is_terminal() {
        // Explicitly asked for, so no silent fallback to the log.
        eprintln!("portway: --tui needs a terminal on stdout");
        std::process::exit(2);
    }
    #[cfg(feature = "tui")]
    if let Some(url) = &args.attach {
        if args.mode != Mode::Forward {
            return Err("--attach cannot be combined with receive".into());
        }
        return portway::remote::run(url, &args);
    }
    logfmt::init_color();
    // The daemon moves to `/`, and SIGHUP reloads with these same arguments:
    // pin both paths now, so a reload rereads the file this start read
    // instead of whatever a relative path names from `/`.
    args.data_dir = args.data_dir.map(std::path::absolute).transpose()?;
    args.config = Config::path(&args).map(std::path::absolute).transpose()?;
    let config = Config::load(&args)?;

    let dir = store::data_dir(args.data_dir.as_deref())?;
    // Read only when a dashboard is going to use it: a forwarder that logs is
    // not a forwarder that has settings.
    #[cfg(feature = "tui")]
    let mut settings = if args.tui {
        tui::Settings::load(&dir, args.event_columns.as_deref(), args.theme)
    } else {
        tui::Settings::default()
    };
    #[cfg(feature = "tui")]
    {
        settings.prices = config.prices.clone();
    }
    #[cfg(not(feature = "tui"))]
    let settings = ();
    // Asked for a dashboard on a port that is already serving: watch the
    // forwarder that holds it instead of starting a second one. Nothing below
    // this line runs in that mode — no listener, no recorder, no pid file.
    #[cfg(feature = "tui")]
    if args.tui && watch::forwarder_on(&config.host, config.port) {
        return watching(&config, &dir, settings);
    }
    // The same for the browser: a console that reads the running forwarder's
    // database, and signals its daemon for reload and stop.
    #[cfg(feature = "web")]
    if args.web && !args.daemon && watch::forwarder_on(&config.host, config.port) {
        return web_watching(&args, &config, &dir);
    }
    // `--daemon` forks here, before the runtime and before the recorder thread
    // exists: the child returns with the pid file held, the parent waits for
    // readiness and exits, and neither inherits a thread it did not create.
    let daemon = args.daemon.then(|| daemon::start(&dir)).transpose()?;
    purge_in_background();
    let store = store::spawn(&dir, args.retention_days)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let outcome = runtime.block_on(run(
        &args,
        &config,
        &dir,
        store.sender(),
        daemon.as_ref(),
        settings,
    ));
    // The recorder is flushed before the process goes, and the pid file goes
    // with it — the lock is released either way, but a file that only ever
    // reads as stale helps nobody. The lock itself goes last: `daemon` was
    // declared before the runtime, so it is dropped after the runtime has
    // joined its threads, and that release is what `--stop` waits for.
    store.shutdown();
    if let Some(daemon) = &daemon {
        daemon.remove_pid_file();
    }
    outcome
}

async fn run(
    args: &Args,
    config: &Config,
    _dir: &std::path::Path,
    store: SyncSender<Event>,
    daemon: Option<&daemon::Daemon>,
    _settings: Settings,
) -> Result<(), Box<dyn std::error::Error>> {
    let router = config.router(args.mode)?;
    let router_state = control::cell(&router);
    let prices_state = control::prices(&config.prices);
    let decoder = (args.mode == Mode::Receive)
        .then(|| portway_core::Receiver::new(config.receiver.clone()))
        .transpose()?;

    // Bound before the probes: a taken port has to fail before the dashboard
    // takes the screen, and requests that arrive during negotiation then wait
    // in the backlog instead of being refused.
    let listener = match tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await {
        Ok(listener) => listener,
        Err(err) => {
            let message = format!("bind {}:{}: {err}", config.host, config.port);
            // A daemon reports through the readiness pipe: the launching
            // terminal prints the reason instead of "see the log".
            match daemon {
                Some(daemon) => daemon.fail(&message),
                None => return Err(message.into()),
            }
        }
    };
    let banner = format!(
        "http://{}:{} -> {} route(s) (stats: {STATS_PATH})",
        config.host,
        config.port,
        router.routes().len()
    );
    // Available in every CLI build, without starting the web console. A local
    // observer failure must never stop forwarding.
    let _live =
        match live::Server::start(_dir, listener.local_addr()?, Arc::clone(router.telemetry())) {
            Ok(server) => Some(server),
            Err(error) => {
                logfmt::warn(&format!("live snapshots unavailable: {error}"));
                None
            }
        };

    if !args.tui {
        #[cfg(feature = "web")]
        let (web_sender, web_events) = match args.web {
            true => {
                let (sender, receiver) = std::sync::mpsc::channel();
                (Some(sender), Some(receiver))
            }
            false => (None, None),
        };
        #[cfg(not(feature = "web"))]
        let web_sender = None;
        telemetry::install(Sinks {
            store: Some(store),
            web: web_sender,
            ..Sinks::default()
        });
        logfmt::info(&banner);
        // Stop from the console: the same way out as SIGTERM, recorder flush
        // and pid file included.
        let stop = Arc::new(tokio::sync::Notify::new());
        #[cfg(feature = "web")]
        let console = match web_events {
            Some(events) => {
                let options = web::Options {
                    host: args.web_host.clone(),
                    port: args.web_port,
                    allow: args.web_allow_host.clone(),
                    base: web::address::base_or_root(args.web_base_path.as_deref()),
                    feed: web::Feed::Live(Arc::clone(&router_state)),
                    events,
                    header: web::Header {
                        listen: format!("http://{}:{}/v1", config.host, config.port),
                        coding: coding_label(&config.compression),
                        mode: if daemon.is_some() {
                            web::Mode::Daemon
                        } else {
                            web::Mode::Live
                        },
                        window: None,
                    },
                    control: web::Control::Live {
                        args: Box::new(args.clone()),
                        cell: Arc::clone(&router_state),
                        prices: Arc::clone(&prices_state),
                        stop: Arc::clone(&stop),
                    },
                    db: Some(_dir.join(store::DB_FILE)),
                    prices: Arc::clone(&prices_state),
                    dir: _dir.to_path_buf(),
                };
                match web::Console::start(options).await {
                    Ok(console) => Some(console),
                    Err(message) => match daemon {
                        Some(daemon) => daemon.fail(&message),
                        None => return Err(message.into()),
                    },
                }
            }
            None => None,
        };
        #[cfg(feature = "web")]
        if let Some(console) = &console {
            logfmt::info(&format!("console at {}", console.public_urls.join(", ")));
        }
        #[cfg(feature = "web")]
        let ready = console
            .as_ref()
            .map(|console| (console.token.as_str(), console.public_urls.as_slice()));
        #[cfg(not(feature = "web"))]
        let ready = None;
        match daemon {
            Some(daemon) => {
                daemon.ready_with(ready);
                let log = daemon.log_path().to_path_buf();
                tokio::spawn(reload_on_hangup(
                    args.clone(),
                    Arc::clone(&router_state),
                    Arc::clone(&prices_state),
                    log,
                ));
            }
            // The token goes to the terminal that started this, never to
            // the log: whoever reads the log has not been handed the page.
            #[cfg(feature = "web")]
            None => {
                if let Some(console) = &console {
                    for url in &console.urls {
                        eprintln!("portway: console at {url}");
                    }
                    if !args.no_open {
                        web::launch::open(&console.launch_url);
                    }
                }
            }
            #[cfg(not(feature = "web"))]
            None => {}
        }
        // A foreground console has no log to reopen: a hangup is the terminal
        // going away, and ends the process the orderly way.
        let hangup = ready.is_some() && daemon.is_none();
        tokio::select! {
            () = negotiated_serving(&router, listener, router_state, decoder) => {}
            () = terminate() => {
                logfmt::info("stopping");
            }
            () = stop.notified() => {
                logfmt::info("stopping");
            }
            () = hung_up(), if hangup => {
                logfmt::info("stopping");
            }
        }
        #[cfg(feature = "web")]
        if let Some(console) = console {
            console.shutdown().await;
        }
        return Ok(());
    }

    #[cfg(feature = "tui")]
    {
        // The dashboard styles its own spans, so the records must arrive plain.
        logfmt::set_color(false);
        let (sender, receiver) = std::sync::mpsc::channel();
        telemetry::install(Sinks {
            tui: Some(sender),
            store: Some(store),
            ..Sinks::default()
        });
        let header = Header {
            listen: format!("http://{}:{}/v1", config.host, config.port),
            coding: coding_label(&config.compression),
            watching: None,
        };
        let (dashboard, quit) = tui::start(
            tui::Feed::Live(Arc::clone(&router)),
            receiver,
            header,
            _settings,
            Some(_dir.join(store::DB_FILE)),
        )?;

        logfmt::info(&banner);
        tokio::select! {
            () = negotiated_serving(&router, listener, router_state, decoder) => {}
            _ = quit => {}
            () = terminate_tui() => {}
        }
        // Joins the thread, which is what puts the terminal back.
        dashboard.shutdown();
    }
    Ok(())
}

/// `--tui` on a port that already has a forwarder: draw what that one is
/// recording. This process owns nothing — no listener to serve, no recorder to
/// write, no pid file to hold — which is what lets the two run side by side.
#[cfg(feature = "tui")]
fn watching(
    config: &Config,
    dir: &std::path::Path,
    settings: tui::Settings,
) -> Result<(), Box<dyn std::error::Error>> {
    // A dashboard styles its own spans, so the records must arrive plain.
    logfmt::set_color(false);
    let (sender, receiver) = std::sync::mpsc::channel();
    telemetry::install(Sinks {
        tui: Some(sender.clone()),
        ..Sinks::default()
    });
    let watch = watch::spawn(&dir.join(store::DB_FILE), sender)?;
    // After the backfill, which is already queued: the newest line in the pane
    // is the one that says what is being read.
    logfmt::info(&format!(
        "watching the forwarder on http://{}:{}: reading {}, last {}",
        config.host,
        config.port,
        dir.join(store::DB_FILE).display(),
        logfmt::span(watch::WINDOW),
    ));
    let header = Header {
        listen: format!("http://{}:{}/v1", config.host, config.port),
        // Nothing was negotiated here; the header shows the window instead.
        coding: String::new(),
        watching: Some(watch::WINDOW),
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let live = live::Client::start(dir.to_path_buf(), config.host.clone(), config.port);
        let (dashboard, quit) = tui::start(
            tui::Feed::Recorded {
                window: watch.window(),
                live: live.snapshots(),
            },
            receiver,
            header,
            settings,
            Some(dir.join(store::DB_FILE)),
        )?;
        tokio::select! {
            _ = quit => {}
            () = terminate_tui() => {}
        }
        // Joins the thread, which is what puts the terminal back.
        dashboard.shutdown();
        Ok::<(), std::io::Error>(())
    })?;
    watch.shutdown();
    Ok(())
}

/// `--status`, plus where the console is while one is running on `dir`.
#[cfg(feature = "web")]
fn with_console(status: String, dir: &std::path::Path) -> String {
    web::console_urls(dir).iter().fold(status, |status, url| {
        format!("{status}\nportway: console at {url}")
    })
}

#[cfg(not(feature = "web"))]
fn with_console(status: String, _dir: &std::path::Path) -> String {
    status
}

/// `--web` on a port that already has a forwarder: serve what that one is
/// recording. Like the watching dashboard this owns no listener of the
/// forwarder's, no recorder and no pid file; reload and stop go to the
/// daemon through its pid file.
#[cfg(feature = "web")]
fn web_watching(
    args: &Args,
    config: &Config,
    dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    telemetry::install(Sinks {
        web: Some(sender.clone()),
        ..Sinks::default()
    });
    let db = dir.join(store::DB_FILE);
    let watch = watch::spawn(&db, sender)?;
    logfmt::info(&format!(
        "watching the forwarder on http://{}:{}: reading {}, last {}",
        config.host,
        config.port,
        db.display(),
        logfmt::span(watch::WINDOW),
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let console = web::Console::start(web::Options {
            host: args.web_host.clone(),
            port: args.web_port,
            allow: args.web_allow_host.clone(),
            base: web::address::base_or_root(args.web_base_path.as_deref()),
            feed: web::Feed::Recorded(watch.window()),
            events: receiver,
            header: web::Header {
                listen: format!("http://{}:{}/v1", config.host, config.port),
                coding: String::new(),
                mode: web::Mode::Attached,
                window: Some(watch::WINDOW),
            },
            control: web::Control::Attached {
                dir: dir.to_path_buf(),
            },
            db: Some(db.clone()),
            // Attached to another process's forwarder: this console never
            // reloads one itself, so the file it read at start is the one it
            // keeps. The daemon it signals republishes its own.
            prices: control::prices(&config.prices),
            dir: dir.to_path_buf(),
        })
        .await?;
        logfmt::info(&format!("console at {}", console.public_urls.join(", ")));
        for url in &console.urls {
            eprintln!("portway: console at {url}");
        }
        if !args.no_open {
            web::launch::open(&console.launch_url);
        }
        tokio::select! {
            () = terminate() => {}
            () = hung_up() => {}
        }
        console.shutdown().await;
        Ok::<(), String>(())
    })?;
    watch.shutdown();
    Ok(())
}

/// One line and an exit code: 0 when the answer is yes, 1 when it is no.
fn daemon_message(outcome: Result<String, String>) -> ! {
    match outcome {
        Ok(message) => {
            println!("portway: {message}");
            std::process::exit(0);
        }
        Err(message) => {
            eprintln!("portway: {message}");
            std::process::exit(1);
        }
    }
}

/// SIGHUP in daemon mode: reopen the log, rebuild the router and the price
/// table from the current TOML and CLI overrides, then negotiate the new
/// upstreams before publishing either.
async fn reload_on_hangup(args: Args, router: RouterCell, prices: PricesCell, log: PathBuf) {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(mut stream) = signal(SignalKind::hangup()) else {
        return;
    };
    while stream.recv().await.is_some() {
        match daemon::reopen_log(&log) {
            Ok(()) => logfmt::info("SIGHUP: log reopened; reloading configuration"),
            Err(err) => logfmt::error(&format!("SIGHUP: could not reopen the log: {err}")),
        }
        match control::reload(&args, &router, &prices).await {
            Ok(routes) => {
                logfmt::info(&format!(
                    "SIGHUP: configuration reloaded ({routes} route(s))"
                ));
            }
            Err(err) => {
                logfmt::error(&format!("SIGHUP: keeping existing configuration: {err}"));
            }
        }
    }
}

/// What the header line says the compressor was asked to do. The coding each
/// upstream actually negotiated is in the model table.
#[cfg(any(feature = "tui", feature = "web"))]
fn coding_label(args: &portway_core::ForwarderConfig) -> String {
    let want = match args.coding {
        CodingArg::Auto => "auto",
        CodingArg::Zstd => "zstd",
        CodingArg::Gzip => "gzip",
        CodingArg::Off => return "off".to_string(),
    };
    format!(
        "{want} L{} >={}",
        args.level,
        logfmt::human(args.min_bytes as u64)
    )
}

/// SIGTERM and SIGINT stop the forwarder in every mode, which is what lets the
/// recorder flush before the process goes.
async fn terminate() {
    use tokio::signal::unix::{SignalKind, signal};

    async fn wait(kind: SignalKind) {
        match signal(kind) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            // No handler: this arm simply never fires.
            Err(_) => std::future::pending::<()>().await,
        }
    }

    tokio::select! {
        () = wait(SignalKind::terminate()) => {}
        () = wait(SignalKind::interrupt()) => {}
    }
}

/// The dashboard also reads SIGHUP as "the terminal went away" — that is what
/// it meant before, and a dashboard has no log file to reopen.
#[cfg(feature = "tui")]
async fn terminate_tui() {
    tokio::select! {
        () = terminate() => {}
        () = hung_up() => {}
    }
}

/// SIGHUP, for the processes that have no log to reopen on one.
async fn hung_up() {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::hangup()) {
        Ok(mut stream) => {
            stream.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

/// Negotiate every upstream, then serve: one future, raced against the ways
/// out. A stop or a signal that arrives while an upstream is slow to answer
/// its probe abandons the probe and ends the process the orderly way. Run
/// before the race, the probe was waited out first, and a signal sent in the
/// meantime, with no handler installed yet, killed the process outright:
/// no recorder flush, no pid file removed, no console told.
async fn negotiated_serving(
    router: &portway::router::Router,
    listener: tokio::net::TcpListener,
    cell: RouterCell,
    receiver: Option<Arc<portway_core::Receiver>>,
) {
    router.negotiate_all().await;
    serving(listener, cell, receiver).await;
}

async fn serving(
    listener: tokio::net::TcpListener,
    router: RouterCell,
    receiver: Option<Arc<portway_core::Receiver>>,
) {
    let telemetry = Arc::clone(control::current(&router).await.telemetry());
    server::serve_with(listener, telemetry, move |request| {
        let router = Arc::clone(&router);
        let receiver = receiver.clone();
        async move {
            let Some(receiver) = receiver else {
                return control::current(&router).await.handle(request).await;
            };
            if request.method() == http::Method::GET
                && request.uri().path() == portway::router::STATS_PATH
            {
                let upstreams = {
                    let current = control::current(&router).await;
                    current
                        .routes()
                        .iter()
                        .map(|(n, f)| (n.clone(), f.snapshot()))
                        .collect::<serde_json::Map<String, serde_json::Value>>()
                };
                return portway::relay::json_response(
                    http::StatusCode::OK,
                    serde_json::json!({"upstreams":upstreams,"receiver":receiver.snapshot()}),
                );
            }
            receiver
                .handle(request, move |request| {
                    let router = Arc::clone(&router);
                    async move {
                        control::current(&router)
                            .await
                            .handle(request.map(http_body_util::Full::new))
                            .await
                    }
                })
                .await
        }
    })
    .await;
}
