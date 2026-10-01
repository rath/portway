//! Requests that have been counted but have not finished relaying yet.
//!
//! The telemetry channel only ever hears about a request once it is over, so
//! a long prefill or a stalled stream is invisible there until it ends. The
//! registry is the other half: every request the forwarder counts is entered
//! here, advanced as it moves through upload, prefill and stream, and removed
//! just before the `Event::Request` that describes it is emitted. A reader
//! holding the registry and the event stream therefore never sees a request
//! twice or not at all: the record carries the flight id it replaced.
//!
//! The request path only writes relaxed atomics; the one lock is taken to
//! enter and to leave the map, and by readers taking a snapshot.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use http::Method;

use crate::clock::PhaseClock;
use crate::forwarder::Coding;

const UNSET: u64 = u64::MAX;

/// Where a request is: sending its body, waiting for the first byte of the
/// answer, or relaying that answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Upload,
    Prefill,
    Stream,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Upload => "upload",
            Phase::Prefill => "prefill",
            Phase::Stream => "stream",
        }
    }
}

/// One request in flight. Fixed at entry except for the atomics, which the
/// request path moves forward.
pub struct Flight {
    id: u64,
    upstream: String,
    model: String,
    method: Method,
    path: String,
    started: Instant,
    started_unix: f64,
    body_len: u64,
    wire_len: AtomicU64,
    coding: AtomicU8,
    /// 0 until response headers arrive.
    status: AtomicU16,
    /// Nanoseconds; `UNSET` until response headers arrive.
    ttfb: AtomicU64,
    received: AtomicU64,
    received_wire: AtomicU64,
    received_agent: AtomicU64,
    attempts: AtomicU32,
    /// Nanoseconds since `started` of the last sign of life.
    progress: AtomicU64,
    /// The current attempt's clock: whether the body is still going out.
    clock: Mutex<Option<Arc<PhaseClock>>>,
}

impl Flight {
    pub fn id(&self) -> u64 {
        self.id
    }

    fn touch(&self) {
        self.progress
            .store(self.started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// A new attempt goes out: its body size, its coding and its clock.
    /// Every attempt after the first is a retry.
    pub fn attempt(&self, clock: &Arc<PhaseClock>, wire_len: usize, coding: Coding) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        self.wire_len.store(wire_len as u64, Ordering::Relaxed);
        self.coding.store(coding as u8, Ordering::Relaxed);
        *self.clock.lock().expect("flight clock") = Some(Arc::clone(clock));
        self.touch();
    }

    /// Response headers are in: the request is streaming from here on.
    pub fn responded(&self, status: u16, ttfb: f64, wire_len: u64, coding: Coding) {
        self.wire_len.store(wire_len, Ordering::Relaxed);
        self.coding.store(coding as u8, Ordering::Relaxed);
        self.ttfb.store((ttfb * 1e9) as u64, Ordering::Relaxed);
        self.status.store(status, Ordering::Relaxed);
        self.touch();
    }

    /// The relay's running totals, as they stand after one more frame.
    pub fn progress(&self, received: u64, received_wire: u64, received_agent: u64) {
        self.received.store(received, Ordering::Relaxed);
        self.received_wire.store(received_wire, Ordering::Relaxed);
        self.received_agent.store(received_agent, Ordering::Relaxed);
        self.touch();
    }

    pub fn view(&self) -> FlightView {
        let now = self.started.elapsed().as_secs_f64();
        let clock = self.clock.lock().expect("flight clock").clone();
        let upload = clock.as_ref().and_then(|clock| clock.upload_wall());
        let status = match self.status.load(Ordering::Relaxed) {
            0 => None,
            status => Some(status),
        };
        let phase = if status.is_some() {
            Phase::Stream
        } else if self.body_len == 0 || upload.is_some() {
            Phase::Prefill
        } else {
            Phase::Upload
        };
        let ttfb = match self.ttfb.load(Ordering::Relaxed) {
            UNSET => None,
            nanos => Some(nanos as f64 / 1e9),
        };
        FlightView {
            id: self.id,
            upstream: self.upstream.clone(),
            model: self.model.clone(),
            method: self.method.clone(),
            path: self.path.clone(),
            started_unix: self.started_unix,
            age: now,
            idle: (now - self.progress.load(Ordering::Relaxed) as f64 / 1e9).max(0.0),
            phase,
            body_len: self.body_len,
            wire_len: self.wire_len.load(Ordering::Relaxed),
            coding: Coding::from_code(self.coding.load(Ordering::Relaxed)),
            status,
            ttfb,
            received: self.received.load(Ordering::Relaxed),
            received_wire: self.received_wire.load(Ordering::Relaxed),
            received_agent: self.received_agent.load(Ordering::Relaxed),
            retries: self.attempts.load(Ordering::Relaxed).saturating_sub(1),
            upload,
        }
    }
}

/// A flight as it stood when it was read. Durations are seconds.
#[derive(Debug, Clone)]
pub struct FlightView {
    pub id: u64,
    /// The route the request is going through.
    pub upstream: String,
    /// The model it named; empty when it named none.
    pub model: String,
    pub method: Method,
    pub path: String,
    pub started_unix: f64,
    /// Since the request was counted.
    pub age: f64,
    /// Since the last attempt, response or relayed frame.
    pub idle: f64,
    pub phase: Phase,
    pub body_len: u64,
    pub wire_len: u64,
    pub coding: Coding,
    pub status: Option<u16>,
    pub ttfb: Option<f64>,
    pub received: u64,
    pub received_wire: u64,
    pub received_agent: u64,
    pub retries: u32,
    /// The current attempt's upload, once it has finished.
    pub upload: Option<f64>,
}

/// Every flight of one process, by id. Ids start at 1 and are never reused,
/// so they also order the flights by the moment they were counted.
#[derive(Default)]
pub struct Flights {
    next: AtomicU64,
    map: Mutex<BTreeMap<u64, Arc<Flight>>>,
}

/// Counts from one registry read, with a bounded list of the oldest flights.
pub struct Snapshot {
    pub total: u64,
    /// In flight per route.
    pub upstreams: BTreeMap<String, u64>,
    pub flights: Vec<FlightView>,
}

impl Flights {
    /// Enter a request that has just been counted.
    pub fn begin(
        &self,
        upstream: &str,
        model: &str,
        method: &Method,
        path: &str,
        body_len: u64,
    ) -> Arc<Flight> {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let started_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_secs_f64());
        let flight = Arc::new(Flight {
            id,
            upstream: upstream.to_owned(),
            model: model.to_owned(),
            method: method.clone(),
            path: path.split('?').next().unwrap_or(path).to_owned(),
            started: Instant::now(),
            started_unix,
            body_len,
            wire_len: AtomicU64::new(0),
            coding: AtomicU8::new(Coding::None as u8),
            status: AtomicU16::new(0),
            ttfb: AtomicU64::new(UNSET),
            received: AtomicU64::new(0),
            received_wire: AtomicU64::new(0),
            received_agent: AtomicU64::new(0),
            attempts: AtomicU32::new(0),
            progress: AtomicU64::new(0),
            clock: Mutex::new(None),
        });
        self.map
            .lock()
            .expect("flights")
            .insert(id, Arc::clone(&flight));
        flight
    }

    /// Take a request out. Idempotent: the relay ends it explicitly before
    /// emitting its record, and the ticket's drop ends it again.
    pub fn end(&self, id: u64) {
        self.map.lock().expect("flights").remove(&id);
    }

    pub fn len(&self) -> usize {
        self.map.lock().expect("flights").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every flight, oldest first.
    pub fn views(&self) -> Vec<FlightView> {
        let flights: Vec<Arc<Flight>> = self
            .map
            .lock()
            .expect("flights")
            .values()
            .cloned()
            .collect();
        flights.iter().map(|flight| flight.view()).collect()
    }

    /// Count every flight, but only clone and inspect the oldest `limit`.
    /// Per-flight clocks are read after releasing the registry lock.
    pub fn snapshot(&self, limit: usize) -> Snapshot {
        let (total, upstreams, flights) = {
            let map = self.map.lock().expect("flights");
            let mut upstreams = BTreeMap::new();
            for flight in map.values() {
                *upstreams.entry(flight.upstream.clone()).or_default() += 1;
            }
            (
                map.len() as u64,
                upstreams,
                map.values().take(limit).cloned().collect::<Vec<_>>(),
            )
        };
        Snapshot {
            total,
            upstreams,
            flights: flights.iter().map(|flight| flight.view()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flight_moves_through_its_phases_and_leaves_once() {
        let flights = Flights::default();
        let first = flights.begin("alpha", "m", &Method::POST, "/v1/chat/completions?x=1", 10);
        let empty = flights.begin("alpha", "", &Method::GET, "/v1/models", 0);
        assert_eq!((first.id(), empty.id()), (1, 2));

        let views = flights.views();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].path, "/v1/chat/completions");
        assert_eq!(views[0].phase, Phase::Upload);
        // Nothing to send: waiting on the answer from the start.
        assert_eq!(views[1].phase, Phase::Prefill);

        let clock = Arc::new(PhaseClock::new());
        first.attempt(&clock, 4, Coding::Zstd);
        first.attempt(&clock, 10, Coding::None);
        let view = first.view();
        assert_eq!(
            (view.retries, view.wire_len, view.coding),
            (1, 10, Coding::None)
        );

        first.responded(200, 0.5, 10, Coding::None);
        first.progress(100, 40, 100);
        let view = first.view();
        assert_eq!(view.phase, Phase::Stream);
        assert_eq!(view.status, Some(200));
        assert_eq!(view.ttfb, Some(0.5));
        assert_eq!((view.received, view.received_wire), (100, 40));

        flights.end(first.id());
        flights.end(first.id());
        assert_eq!(flights.len(), 1);
        assert_eq!(flights.views()[0].id, 2);
    }
}
