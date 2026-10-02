//! A local terminal viewing a remote web console. Never starts a forwarder.
pub mod transport;
pub mod wire;

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use http_body_util::BodyExt;
use tokio::sync::{mpsc, watch};

use crate::{cli::Args, spend, store, tui};
use transport::{Client, Error};
use wire::{Events, Flights, Snapshot, Tick, UsageReply, WireEvent};

pub enum Update {
    Snapshot(Box<Snapshot>),
    Tick(Tick),
    Event(WireEvent),
    Flights(Option<Flights>),
    Usage(spend::Range, Result<UsageReply, String>),
    Connection(bool, String),
}

pub fn run(url: &str, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new(url)?;
    let dir = store::data_dir(args.data_dir.as_deref())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(attach(client, dir, args))
}

async fn attach(
    mut client: Client,
    dir: PathBuf,
    args: &Args,
) -> Result<(), Box<dyn std::error::Error>> {
    client.authenticate(&dir).await?;
    let snapshot = snapshot(&client).await.map_err(|error| {
        if matches!(error, Error::Expired) {
            let _ = transport::save_session(&dir, &client.url, None);
        }
        error
    })?;
    let initial = (snapshot.seq, snapshot.generation);
    let (updates, receiver) = mpsc::channel(256);
    updates.send(Update::Snapshot(Box::new(snapshot))).await?;
    let (usage, requested) = watch::channel(None);
    let (unused, events) = std::sync::mpsc::channel();
    // The network channel carries remote events; keep the local one connected
    // so the TUI can keep its normal key-driven redraw loop.
    let _unused = unused;
    let header = tui::view::Header {
        listen: client.url.clone(),
        coding: String::new(),
        watching: None,
    };
    let settings = tui::Settings::load(&dir, args.event_columns.as_deref(), args.theme);
    let (dashboard, quit) = tui::start(
        tui::Feed::Remote {
            updates: Mutex::new(receiver),
            usage,
        },
        events,
        header,
        settings,
        None,
    )?;
    let mut worker = tokio::spawn(follow(client.clone(), updates.clone(), Some(initial)));
    let mut usage_worker = tokio::spawn(usage_loop(client.clone(), requested, updates));
    let outcome = tokio::select! {
        _ = quit => Ok(()),
        _ = interrupted() => Ok(()),
        result = &mut worker => result.unwrap_or_else(|_| Err(Error::Other("remote connection task failed".into()))),
        result = &mut usage_worker => result.unwrap_or_else(|_| Err(Error::Other("remote usage task failed".into()))),
    };
    worker.abort();
    usage_worker.abort();
    // Always restore the terminal before returning any error to main.
    dashboard.shutdown();
    if matches!(outcome, Err(Error::Expired)) {
        transport::save_session(&dir, &client.url, None)?;
    }
    outcome.map_err(Into::into)
}

async fn interrupted() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! { _ = term.recv() => {}, _ = interrupt.recv() => {} }
}

async fn snapshot(client: &Client) -> Result<Snapshot, Error> {
    let mut snapshot: Snapshot = client.get("/api/snapshot").await?;
    // Snapshot carries the newest 1000. Page backward to the same bounded
    // backlog the local TUI holds; the initial stream replays anything newer.
    while snapshot.events.len() < tui::state::EVENT_CAPACITY {
        let Some(first) = snapshot.events.first().map(|event| event.seq) else {
            break;
        };
        if first <= snapshot.oldest {
            break;
        }
        let limit = (tui::state::EVENT_CAPACITY - snapshot.events.len()).min(1000);
        let page: Events = client
            .get(&format!("/api/events?before={first}&limit={limit}"))
            .await?;
        let mut older: Vec<_> = page
            .events
            .into_iter()
            .filter(|event| event.seq < first)
            .collect();
        if older.is_empty() {
            break;
        }
        older.append(&mut snapshot.events);
        snapshot.events = older;
        snapshot.oldest = page.oldest;
    }
    snapshot.events.sort_by_key(|event| event.seq);
    snapshot.events.dedup_by_key(|event| event.seq);
    Ok(snapshot)
}

async fn send(updates: &mpsc::Sender<Update>, update: Update) -> Result<(), Error> {
    updates
        .send(update)
        .await
        .map_err(|_| Error::Other("viewer closed".into()))
}

async fn follow(
    client: Client,
    updates: mpsc::Sender<Update>,
    mut initial: Option<(u64, u64)>,
) -> Result<(), Error> {
    let mut delay = 2;
    loop {
        let started = std::time::Instant::now();
        match connected(&client, &updates, initial.take()).await {
            Err(Error::Expired) => return Err(Error::Expired),
            Err(error) => {
                send(
                    &updates,
                    Update::Connection(false, format!("reconnecting · {error}")),
                )
                .await?
            }
            Ok(()) => {
                delay = 2;
                continue;
            } // reset/reload: resnapshot immediately
        }
        if started.elapsed() >= Duration::from_secs(10) {
            delay = 2;
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(30);
    }
}

async fn connected(
    client: &Client,
    updates: &mpsc::Sender<Update>,
    initial: Option<(u64, u64)>,
) -> Result<(), Error> {
    let snapshot = if initial.is_none() {
        Some(snapshot(client).await?)
    } else {
        None
    };
    let (seq, generation) = initial.unwrap_or_else(|| {
        let snapshot = snapshot.as_ref().expect("fresh snapshot");
        (snapshot.seq, snapshot.generation)
    });
    let (mut body, _lease) = client.stream(seq).await?;
    if let Some(snapshot) = snapshot {
        send(updates, Update::Snapshot(Box::new(snapshot))).await?;
    }
    send(updates, Update::Connection(true, "live".into())).await?;
    let mut parser = Sse::default();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(45), body.frame())
            .await
            .map_err(|_| Error::Other("console stream timed out".into()))?
            .ok_or_else(|| Error::Other("console stream closed".into()))?
            .map_err(|_| Error::Other("console stream interrupted".into()))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        for (kind, data) in parser.push(&data)? {
            let update = match kind.as_str() {
                "ev" => Update::Event(serde_json::from_str(&data)?),
                "tick" => {
                    let tick: Tick = serde_json::from_str(&data)?;
                    if tick.generation != generation {
                        return Ok(());
                    }
                    Update::Tick(tick)
                }
                "flights" => Update::Flights(Some(serde_json::from_str(&data)?)),
                "reset" => return Ok(()),
                "control" => {
                    let control: serde_json::Value = serde_json::from_str(&data)?;
                    if control["event"] == "reloaded" {
                        return Ok(());
                    }
                    if control["event"] == "stopping" {
                        return Err(Error::Other("remote daemon is stopping".into()));
                    }
                    continue;
                }
                _ => continue,
            };
            send(updates, update).await?;
        }
    }
}

async fn usage_loop(
    client: Client,
    mut requested: watch::Receiver<Option<spend::Range>>,
    updates: mpsc::Sender<Update>,
) -> Result<(), Error> {
    loop {
        let range = *requested.borrow_and_update();
        if let Some(range) = range {
            let result = client
                .get::<UsageReply>(&format!("/api/usage?range={}", range.key()))
                .await;
            if matches!(result, Err(Error::Expired)) {
                return Err(Error::Expired);
            }
            send(
                &updates,
                Update::Usage(range, result.map_err(|error| error.to_string())),
            )
            .await?;
        }
        tokio::select! {
            result = requested.changed() => if result.is_err() { return Ok(()); },
            _ = tokio::time::sleep(tui::state::USAGE_REFRESH), if range.is_some() => {},
        }
    }
}

/// Incremental UTF-8 SSE parser. Limits apply to each frame, not the lifetime
/// of a connection; chunk boundaries may fall inside a codepoint or CRLF.
#[derive(Default)]
struct Sse {
    pending: Vec<u8>,
    kind: String,
    data: String,
    size: usize,
}
impl Sse {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<(String, String)>, Error> {
        const LIMIT: usize = 4 * 1024 * 1024;
        let mut frames = Vec::new();
        for byte in bytes {
            self.size += 1;
            if self.size > LIMIT {
                return Err(Error::Other("console stream frame too large".into()));
            }
            if *byte != b'\n' {
                self.pending.push(*byte);
                continue;
            }
            let raw = std::mem::take(&mut self.pending);
            let line = std::str::from_utf8(&raw)
                .map_err(|_| Error::Other("invalid console stream text".into()))?
                .trim_end_matches('\r');
            if line.is_empty() {
                if !self.data.is_empty() {
                    frames.push((
                        std::mem::take(&mut self.kind),
                        std::mem::take(&mut self.data),
                    ));
                }
                self.kind.clear();
                self.size = 0;
            } else if let Some(kind) = line.strip_prefix("event:") {
                self.kind = kind.trim_start_matches(' ').to_owned();
            } else if let Some(data) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            }
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stream_chunks_are_not_events_and_heartbeats_are_not_data() {
        let text = ": ping\r\n\r\nevent: ev\r\nid: 1\r\ndata: {\"message\":\"한글\"}\r\n\r\nevent: reset\ndata: {\ndata: \"reason\":\"gap\"}\n\n";
        let mut parser = Sse::default();
        let mut frames = Vec::new();
        for byte in text.as_bytes() {
            frames.extend(parser.push(&[*byte]).unwrap());
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], ("ev".into(), "{\"message\":\"한글\"}".into()));
        assert_eq!(frames[1], ("reset".into(), "{\n\"reason\":\"gap\"}".into()));
        assert!(parser.push(&vec![b'x'; 4 * 1024 * 1024 + 1]).is_err());
    }
}
