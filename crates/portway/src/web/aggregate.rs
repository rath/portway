//! The console's one reader of the telemetry channel.
//!
//! A thread of its own, like the terminal dashboard's render loop: it numbers
//! and serializes every event once, keeps the backlog pages resume from,
//! folds events and the 250ms counter sample into the same `Board` the
//! terminal draws, and broadcasts what changed to every open stream. Pages
//! never touch the board directly; they get frames, or a snapshot built under
//! the one lock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::board::Board;
use crate::control::RouterCell;
use crate::flights::{FlightView, Phase};
use crate::logfmt;
use crate::router::Router;
use crate::telemetry::Event;
use crate::watch;
use crate::web::api;
use crate::web::ring::{self, Gap, Ring};

/// The counter sample, and the fastest the in-flight list is re-sent.
const TICK: Duration = Duration::from_millis(250);
/// How often the HUD numbers go out.
const FRAME: Duration = Duration::from_secs(1);
/// Frames a slow page may fall behind by before it is told to start over.
const BACKLOG: usize = 4096;
/// Events a snapshot carries; older ones are a `/api/events` away.
pub const SNAPSHOT_EVENTS: usize = 1000;
/// Traffic buckets a tick carries: the newest is still filling, so the page
/// overwrites the seconds it already has.
const TRAFFIC_TAIL: usize = 5;

/// Where the numbers come from: this process's own router, or another
/// forwarder's database window.
pub enum Feed {
    Live(RouterCell),
    Recorded(Arc<Mutex<watch::Window>>),
}

/// One server-sent event, serialized once for every page.
#[derive(Clone, Debug)]
pub enum Frame {
    Event { seq: u64, json: Arc<str> },
    Tick(Arc<str>),
    Flights(Arc<str>),
    Control(Arc<str>),
}

/// Serialized events with their sequence numbers, oldest first.
pub type Backlog = Vec<(u64, Arc<str>)>;

struct Inner {
    board: Board,
    ring: Ring,
    generation: u64,
    /// The router the last sample read; a different one is a reload.
    router: Option<Arc<Router>>,
}

pub struct Shared {
    inner: Mutex<Inner>,
    frames: broadcast::Sender<Frame>,
    stop: AtomicBool,
    recorded: bool,
}

impl Shared {
    /// Subscribe and read the backlog under one lock: every event after
    /// `after` is then either in the backlog or on the receiver (or, for one
    /// pushed in between, both — the stream skips sequence numbers it sent).
    pub fn resume(&self, after: u64) -> Result<(broadcast::Receiver<Frame>, Backlog), Gap> {
        let inner = self.inner.lock().expect("web board");
        let receiver = self.frames.subscribe();
        inner.ring.after(after).map(|backlog| (receiver, backlog))
    }

    /// Up to `limit` events before `seq`, and the oldest one still held.
    pub fn before(&self, seq: u64, limit: usize) -> (Vec<Arc<str>>, u64) {
        let inner = self.inner.lock().expect("web board");
        (inner.ring.before(seq, limit), inner.ring.oldest())
    }

    pub fn announce(&self, control: Value) {
        let _ = self
            .frames
            .send(Frame::Control(Arc::from(control.to_string())));
    }

    /// Seed every subscriber, including one connecting between two flight
    /// changes. The periodic publisher only emits when progress changes.
    pub fn flights(&self) -> Option<String> {
        let inner = self.inner.lock().expect("web board");
        inner
            .router
            .as_ref()
            .filter(|_| !self.recorded)
            .map(|router| {
                api::flights(logfmt::epoch(), &router.telemetry().flights().views()).to_string()
            })
    }

    /// Everything a page needs to draw its first frame. `header` is fixed at
    /// start and completed here with what moves.
    pub fn snapshot(&self, mut header: Value) -> String {
        let (body, events) = {
            let inner = self.inner.lock().expect("web board");
            let board = &inner.board;
            header["uptime_s"] = json!(uptime(board));
            let flights = inner
                .router
                .as_ref()
                .filter(|_| !self.recorded)
                .map(|router| api::flights(logfmt::epoch(), &router.telemetry().flights().views()));
            let body = json!({
                "header": header,
                "seq": inner.ring.newest(),
                "oldest": inner.ring.oldest(),
                "generation": inner.generation,
                "totals": api::totals(board),
                "counts": api::counts(board),
                "latency": api::latency(board),
                "models": api::models(board),
                "bars": api::bars(board, crate::board::BARS),
                "bars_total": board.bars_pushed,
                "traffic": api::traffic(board, crate::board::TRAFFIC_SECONDS),
                "coverage": board.coverage.map(|coverage| coverage.as_secs_f64()),
                "flights": flights,
            });
            (body, inner.ring.tail(SNAPSHOT_EVENTS))
        };
        // The events are already JSON; splice them in rather than parse them
        // back just to write them out again.
        let body = body.to_string();
        let mut out = String::with_capacity(
            body.len() + events.iter().map(|e| e.len() + 1).sum::<usize>() + 16,
        );
        out.push_str("{\"events\":[");
        for (index, event) in events.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(event);
        }
        out.push_str("],");
        out.push_str(&body[1..]);
        out
    }
}

/// The live process knows its own uptime; a window only how far back it reaches.
fn uptime(board: &Board) -> f64 {
    match board.coverage {
        Some(coverage) => coverage.as_secs_f64(),
        None => board.started.elapsed().as_secs_f64(),
    }
}

pub struct Handle {
    pub shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Handle {
    pub fn shutdown(mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn spawn(feed: Feed, events: Receiver<Event>) -> std::io::Result<Handle> {
    let recorded = matches!(feed, Feed::Recorded(_));
    let mut board = Board::new();
    board.recorded = recorded;
    let (frames, _) = broadcast::channel(BACKLOG);
    let shared = Arc::new(Shared {
        inner: Mutex::new(Inner {
            board,
            ring: Ring::new(ring::CAPACITY),
            generation: 0,
            router: None,
        }),
        frames,
        stop: AtomicBool::new(false),
        recorded,
    });
    // Whatever is already queued — an attached console starts with an hour of
    // it — and a first sample are in place before the first page asks.
    for event in events.try_iter() {
        push(&shared, event);
    }
    sample(&shared, &feed);
    let thread = std::thread::Builder::new()
        .name("web-board".to_string())
        .spawn({
            let shared = Arc::clone(&shared);
            move || run(&shared, &feed, &events)
        })?;
    Ok(Handle {
        shared,
        thread: Some(thread),
    })
}

fn run(shared: &Shared, feed: &Feed, events: &Receiver<Event>) {
    let mut last_sample = Instant::now();
    let mut last_frame = Instant::now();
    let mut bars_sent = shared.inner.lock().expect("web board").board.bars_pushed;
    let mut flights_sent: Option<Vec<Signature>> = None;
    while !shared.stop.load(Ordering::Acquire) {
        match events.recv_timeout(TICK.saturating_sub(last_sample.elapsed())) {
            Ok(event) => {
                push(shared, event);
                for event in events.try_iter() {
                    push(shared, event);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // Every sender is gone: nothing more will arrive, but the counters
            // still move.
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(TICK),
        }
        if last_sample.elapsed() < TICK {
            continue;
        }
        last_sample = Instant::now();
        sample(shared, feed);
        send_flights(shared, &mut flights_sent);
        if last_frame.elapsed() >= FRAME {
            last_frame = Instant::now();
            send_tick(shared, &mut bars_sent);
        }
    }
}

/// Number, serialize and fold one event, then hand it to the open streams.
fn push(shared: &Shared, event: Event) {
    let (seq, json) = {
        let mut inner = shared.inner.lock().expect("web board");
        let seq = inner.ring.newest() + 1;
        let ts = match &event {
            // A replayed row says when it happened only as `HH:MM:SS`.
            Event::Request(record) if shared.recorded => stamp_time(&record.stamp),
            Event::Log { stamp, .. } if shared.recorded => stamp_time(stamp),
            _ => logfmt::epoch(),
        };
        let json: Arc<str> = Arc::from(api::event(seq, ts, &event).to_string());
        inner.board.observe(&event);
        inner.ring.push(seq, Arc::clone(&json));
        (seq, json)
    };
    let _ = shared.frames.send(Frame::Event { seq, json });
}

/// The most recent moment that reads `HH:MM:SS` on the local clock: today's,
/// or yesterday's when today's has not happened yet. A stamp inside a
/// repeated DST hour is taken as its later occurrence.
fn stamp_time(stamp: &str) -> f64 {
    let now = logfmt::epoch();
    let mut parts = stamp.split(':').map(|part| part.parse::<u32>().ok());
    let (Some(Some(hours)), Some(Some(minutes)), Some(Some(seconds))) =
        (parts.next(), parts.next(), parts.next())
    else {
        return now;
    };
    let offset = f64::from(hours * 3600 + minutes * 60 + seconds);
    let today = logfmt::midnight(now) + offset;
    if today <= now + 1.0 {
        today
    } else {
        logfmt::midnight(logfmt::midnight(now) - 1.0) + offset
    }
}

/// The 250ms counter sample. A router that is not the one sampled last is a
/// reload: its per-model counters start over, and the pages are told so.
fn sample(shared: &Shared, feed: &Feed) {
    match feed {
        Feed::Live(cell) => {
            // Never wait on a reload in progress: the old router is still a
            // fine sample until the new one is published.
            let router = cell.try_read().map(|router| Arc::clone(&router)).ok();
            let mut inner = shared.inner.lock().expect("web board");
            let Some(router) = router.or_else(|| inner.router.clone()) else {
                return;
            };
            let reloaded = inner
                .router
                .as_ref()
                .is_some_and(|last| !Arc::ptr_eq(last, &router));
            inner.board.tick(&router);
            inner.router = Some(Arc::clone(&router));
            if reloaded {
                inner.generation += 1;
                let generation = inner.generation;
                drop(inner);
                shared.announce(json!({
                    "event": "reloaded",
                    "generation": generation,
                    "routes": router.routes().len(),
                }));
            }
        }
        Feed::Recorded(window) => {
            let window = window.lock().expect("watch window").clone();
            shared
                .inner
                .lock()
                .expect("web board")
                .board
                .tick_recorded(&window);
        }
    }
}

/// What makes an in-flight list worth re-sending: ages move on their own and
/// the page advances them from `at_unix`.
type Signature = (u64, Phase, Option<u16>, u64, u32, u64);

fn signature(view: &FlightView) -> Signature {
    (
        view.id,
        view.phase,
        view.status,
        view.received,
        view.retries,
        view.wire_len,
    )
}

fn send_flights(shared: &Shared, sent: &mut Option<Vec<Signature>>) {
    if shared.recorded {
        return;
    }
    let Some(router) = shared.inner.lock().expect("web board").router.clone() else {
        return;
    };
    let views = router.telemetry().flights().views();
    let now: Vec<Signature> = views.iter().map(signature).collect();
    if sent.as_ref() == Some(&now) {
        return;
    }
    let frame = api::flights(logfmt::epoch(), &views).to_string();
    *sent = Some(now);
    let _ = shared.frames.send(Frame::Flights(Arc::from(frame)));
}

fn send_tick(shared: &Shared, bars_sent: &mut u64) {
    let frame = {
        let inner = shared.inner.lock().expect("web board");
        let board = &inner.board;
        let fresh = (board.bars_pushed - *bars_sent).min(board.bars.len() as u64);
        *bars_sent = board.bars_pushed;
        json!({
            "uptime_s": uptime(board),
            "coverage": board.coverage.map(|coverage| coverage.as_secs_f64()),
            "totals": api::totals(board),
            "counts": api::counts(board),
            "latency": api::latency(board),
            "models": api::models(board),
            "bars_push": api::bars(board, fresh as usize),
            "bars_total": board.bars_pushed,
            "traffic_tail": api::traffic(board, TRAFFIC_TAIL),
            "generation": inner.generation,
        })
    };
    let _ = shared
        .frames
        .send(Frame::Tick(Arc::from(frame.to_string())));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_is_the_latest_moment_it_names() {
        let now = logfmt::epoch();
        let at = stamp_time(&logfmt::clock(now - 5.0));
        assert!((now - 5.0 - at).abs() < 1.5, "{at} vs {now}");
        // A minute from now has not happened today: it was yesterday's.
        let later = stamp_time(&logfmt::clock(now + 60.0));
        assert!(later < now && later > now - 86_400.0 - 3_700.0, "{later}");
        assert!((stamp_time("nonsense") - now).abs() < 5.0);
    }
}
