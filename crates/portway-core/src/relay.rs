//! Streaming the upstream response back to the agent, and the one-line log
//! that describes the request once it is over.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Response, StatusCode};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::body::Incoming;

use crate::body::{Decoder, Encoder};
use crate::clock::PhaseClock;
use crate::forwarder::{Coding, InFlight, Stats};
use crate::pool::Lease;
use crate::telemetry::{Event, RequestRecord, Telemetry};
use crate::time;
use crate::usage::{self, Usage};

/// One SSE gap may span a whole long prefill.
pub const READ_TIMEOUT: Duration = Duration::from_secs(600);

/// How much longer, and how much more, the upstream is read once the agent
/// has gone. An agent that stops reading at the answer's last event — Codex
/// closes on `response.completed` — leaves the EOF unread, which would make
/// a finished answer a cut one: no usage, and a connection that cannot be
/// pooled. Reading on lets that answer end on its own.
///
/// The two limits tell that answer from one the agent cut short. A finished
/// answer is quiet until its end, and the end can trail its last event by
/// more than the event took to arrive: behind a receiver on another
/// continent it reached the sender up to about a second after the receiver
/// had it, and a quarter of a second cut one finished Codex turn in seven.
/// An answer still generating sends more of itself instead, and past a
/// closing event's worth of it is dropped as before, which is what makes the
/// engine stop. Only an engine that has gone quiet mid-answer, thinking,
/// runs out the clock.
const GRACE: Duration = Duration::from_secs(2);
/// Decoded bytes: a trailing `data: [DONE]` or usage chunk, not a generation.
const GRACE_BYTES: u64 = 1 << 10;

/// Everything the one-line request log needs, rendered when the relay ends —
/// including when the agent disconnects mid-stream.
pub struct RequestLog {
    telemetry: Arc<Telemetry>,
    upstream: String,
    model: String,
    tier: Option<String>,
    method: Method,
    path: String,
    status: u16,
    clock: Arc<PhaseClock>,
    body_len: u64,
    wire_len: u64,
    coding: Coding,
    ttfb: f64,
    upstream_encoding: String,
    /// The coding the agent gets, when it offered one we can make.
    agent_coding: Option<&'static str>,
    received: u64,
    received_wire: u64,
    agent_bytes: u64,
    first_byte: Option<Instant>,
    /// When the agent was last handed a chunk; `None` before the first one
    /// and once the relay is over for the agent.
    last_chunk: Option<Instant>,
    /// The longest wait for the next chunk so far (see `RequestRecord`).
    max_gap: Option<Duration>,
    /// False until the upstream body ends on its own: an abort, an upstream
    /// error or a read timeout leaves it false.
    complete: bool,
    /// What the upstream said the answer cost. Read out of the body as it is
    /// relayed, and only recorded once that body ended on its own: an engine
    /// reports the counts in its last chunk, so a cut-short answer has none.
    usage: Option<Usage>,
    stats: Arc<Stats>,
    /// Kept alive so the request counts as in flight for the whole relay.
    _in_flight: InFlight,
}

impl RequestLog {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        upstream: String,
        model: String,
        tier: Option<String>,
        method: Method,
        path: String,
        status: u16,
        clock: Arc<PhaseClock>,
        body_len: u64,
        wire_len: u64,
        coding: Coding,
        ttfb: f64,
        upstream_encoding: String,
        agent_coding: Option<&'static str>,
        stats: Arc<Stats>,
        in_flight: InFlight,
        telemetry: Arc<Telemetry>,
    ) -> Self {
        RequestLog {
            telemetry,
            upstream,
            model,
            tier,
            method,
            path,
            status,
            clock,
            body_len,
            wire_len,
            coding,
            ttfb,
            upstream_encoding,
            agent_coding,
            received: 0,
            received_wire: 0,
            agent_bytes: 0,
            first_byte: None,
            last_chunk: None,
            max_gap: None,
            complete: false,
            usage: None,
            stats,
            _in_flight: in_flight,
        }
    }

    /// The same numbers the log line renders, for an in-process observer.
    fn record(&self) -> RequestRecord {
        RequestRecord {
            stamp: time::stamp(),
            upstream: self.upstream.clone(),
            model: self.model.clone(),
            tier: self.tier.clone(),
            method: self.method.clone(),
            path: self.path.clone(),
            status: self.status,
            dns: self.clock.dns(),
            tcp: self.clock.tcp(),
            tls: self.clock.tls(),
            body_len: self.body_len,
            wire_len: self.wire_len,
            coding: self.coding,
            upload: self.clock.upload_wall(),
            ttfb: self.ttfb,
            received: self.received,
            received_wire: self.received_wire,
            received_agent: self.agent_bytes,
            upstream_encoding: self.upstream_encoding.clone(),
            agent_encoding: self.agent_coding.map(str::to_owned),
            download: self.first_byte.map(|at| at.elapsed().as_secs_f64()),
            // A gap still open here belongs to a log that ended outside the
            // relay; it lasted until now.
            max_gap: self
                .max_gap
                .into_iter()
                .chain(self.last_chunk.map(|last| last.elapsed()))
                .max()
                .map(|gap| gap.as_secs_f64()),
            complete: self.complete,
            usage: self.usage,
            flight: Some(self._in_flight.flight().id()),
        }
    }
}

impl RequestLog {
    /// Publish the running totals to the flight registry. What the agent got
    /// is the decoded count until the hop encodes.
    fn report_progress(&self) {
        let agent = if self.agent_coding.is_some() {
            self.agent_bytes
        } else {
            self.received
        };
        self._in_flight
            .flight()
            .progress(self.received, self.received_wire, agent);
    }

    /// A chunk handed to the agent at `now`: the wait since the previous one
    /// is a gap, and the first one opens the count at zero.
    fn chunk(&mut self, now: Instant) {
        let gap = self
            .last_chunk
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        self.widen(gap);
        self.last_chunk = Some(now);
    }

    /// The relay is over for the agent at `now`, so the wait since its last
    /// chunk is its last gap. Whatever is read after that, the agent never
    /// waited for.
    fn close_gap(&mut self, now: Instant) {
        if let Some(last) = self.last_chunk.take() {
            self.widen(now.saturating_duration_since(last));
        }
    }

    fn widen(&mut self, gap: Duration) {
        self.max_gap = Some(self.max_gap.map_or(gap, |max| max.max(gap)));
    }
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        // Out of the registry first: a reader that sees the record must not
        // also still see the request in flight.
        self._in_flight.end();
        self.telemetry.emit(Event::Request(Arc::new(self.record())));
    }
}

/// Streams the upstream response to the agent, decoding as it goes.
///
/// Dropping this — which is what an agent disconnect does — hands the
/// upstream body to a short grace read (`GRACE`); past that it is dropped,
/// and hyper then closes that HTTP/1.1 connection. That disconnect is what
/// makes the engine abort the generation.
pub struct RelayBody {
    upstream: Option<Incoming>,
    decoder: Decoder,
    /// Set when the agent offered a coding: every decoded chunk is re-encoded
    /// on its way out, so the hop costs the agent less than the answer.
    encoder: Option<Encoder>,
    /// Reads the engine's usage object out of the decoded stream as it passes.
    usage: usage::Scanner,
    lease: Option<Lease>,
    log: Option<RequestLog>,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl RelayBody {
    pub fn new(
        upstream: Incoming,
        decoder: Decoder,
        encoder: Option<Encoder>,
        lease: Lease,
        log: RequestLog,
    ) -> Self {
        RelayBody {
            upstream: Some(upstream),
            decoder,
            encoder,
            usage: usage::Scanner::default(),
            lease: Some(lease),
            log: Some(log),
            deadline: Box::pin(tokio::time::sleep(READ_TIMEOUT)),
        }
    }

    fn finish(&mut self) {
        if let Some(log) = self.log.as_mut() {
            log.close_gap(Instant::now());
        }
        // Only a body read to completion leaves the connection reusable.
        if self.log.as_ref().is_some_and(|log| log.complete)
            && let Some(lease) = self.lease.as_mut()
        {
            lease.release();
        }
        self.upstream = None;
    }

    /// One frame on its way to the agent, counted only when the hop encodes:
    /// otherwise the payload is the decoded bytes the log already counted.
    fn count_agent(&mut self, len: usize) {
        let Some(log) = self.log.as_mut() else { return };
        if log.agent_coding.is_some() {
            log.agent_bytes += len as u64;
            log.stats
                .agent_bytes
                .fetch_add(len as u64, Ordering::Relaxed);
        }
        log.report_progress();
    }
}

impl Drop for RelayBody {
    fn drop(&mut self) {
        // `finish` has already taken the body of an answer that ended, failed
        // or timed out; what is left here is an answer the agent walked out on.
        let Some(upstream) = self.upstream.take() else {
            return;
        };
        if let Some(log) = self.log.as_mut() {
            log.close_gap(Instant::now());
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(drain(
            upstream,
            std::mem::replace(&mut self.decoder, Decoder::identity()),
            std::mem::take(&mut self.usage),
            self.lease.take(),
            self.log.take(),
        ));
    }
}

/// Read the rest of an answer the agent stopped reading, for at most `GRACE`
/// and `GRACE_BYTES` more of it. An answer that ends within them is recorded
/// as complete, with the usage its last chunk carried, and its connection is
/// pooled; one that does not is dropped here, cut, as the agent's disconnect
/// would have dropped it at once.
async fn drain(
    mut upstream: Incoming,
    mut decoder: Decoder,
    mut usage: usage::Scanner,
    mut lease: Option<Lease>,
    mut log: Option<RequestLog>,
) {
    let rest = async {
        let mut more = 0u64;
        while let Some(frame) = upstream.frame().await {
            let Ok(frame) = frame else { return false };
            let Ok(data) = frame.into_data() else {
                continue;
            };
            if let Some(log) = log.as_mut() {
                log.received_wire += data.len() as u64;
                log.stats
                    .down_wire_bytes
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
            }
            let Ok(decoded) = decoder.push(data) else {
                return false;
            };
            usage.push(&decoded);
            if let Some(log) = log.as_mut() {
                log.received += decoded.len() as u64;
                log.stats
                    .down_bytes
                    .fetch_add(decoded.len() as u64, Ordering::Relaxed);
            }
            // More than a closing event: the engine is still generating.
            more += decoded.len() as u64;
            if more > GRACE_BYTES {
                return false;
            }
        }
        true
    };
    let ended = tokio::time::timeout(GRACE, rest).await.unwrap_or(false);
    if ended {
        if let Some(log) = log.as_mut() {
            log.complete = true;
            log.usage = usage.usage();
        }
        if let Some(lease) = lease.as_mut() {
            lease.release();
        }
    }
    // The connection goes back, or away, before the record says the request
    // is over; the record is emitted by the log's drop.
    drop(lease);
    drop(upstream);
    drop(log);
}

impl Body for RelayBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        loop {
            let Some(upstream) = self.upstream.as_mut() else {
                return Poll::Ready(None);
            };
            match Pin::new(upstream).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    self.deadline
                        .as_mut()
                        .reset(tokio::time::Instant::now() + READ_TIMEOUT);
                    let Ok(data) = frame.into_data() else {
                        continue; // trailers: nothing to relay on an h1 leg
                    };
                    if let Some(log) = self.log.as_mut() {
                        log.received_wire += data.len() as u64;
                        log.stats
                            .down_wire_bytes
                            .fetch_add(data.len() as u64, Ordering::Relaxed);
                    }
                    let decoded = match self.decoder.push(data) {
                        Ok(decoded) => decoded,
                        Err(err) => {
                            self.finish();
                            return Poll::Ready(Some(Err(err)));
                        }
                    };
                    if decoded.is_empty() {
                        continue; // the codec needs more input before it emits
                    }
                    // The counts and the usage object describe the answer, so
                    // both are read before the hop re-encodes the bytes.
                    self.usage.push(&decoded);
                    if let Some(log) = self.log.as_mut() {
                        let now = Instant::now();
                        if log.first_byte.is_none() {
                            log.first_byte = Some(now);
                        }
                        log.chunk(now);
                        log.received += decoded.len() as u64;
                        log.stats
                            .down_bytes
                            .fetch_add(decoded.len() as u64, Ordering::Relaxed);
                    }
                    let payload = match self.encoder.as_mut() {
                        Some(encoder) => match encoder.push(&decoded) {
                            Ok(payload) => payload,
                            Err(err) => {
                                self.finish();
                                return Poll::Ready(Some(Err(err)));
                            }
                        },
                        None => decoded,
                    };
                    self.count_agent(payload.len());
                    return Poll::Ready(Some(Ok(Frame::data(payload))));
                }
                Poll::Ready(Some(Err(err))) => {
                    self.finish();
                    return Poll::Ready(Some(Err(std::io::Error::other(err))));
                }
                Poll::Ready(None) => {
                    // The engine reports the counts in its last chunk, so the
                    // body ending is what makes them this answer's.
                    let usage = self.usage.usage();
                    if let Some(log) = self.log.as_mut() {
                        log.complete = true;
                        log.usage = usage;
                    }
                    // The codec's epilogue is the agent's last frame: without it
                    // the stream it decodes would be truncated. `finish` takes
                    // the encoder, so this happens exactly once.
                    let tail = self.encoder.as_mut().map(Encoder::finish);
                    self.finish();
                    match tail {
                        Some(Ok(tail)) if !tail.is_empty() => {
                            self.count_agent(tail.len());
                            return Poll::Ready(Some(Ok(Frame::data(tail))));
                        }
                        Some(Err(err)) => return Poll::Ready(Some(Err(err))),
                        _ => {}
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => {
                    if self.deadline.as_mut().poll(cx).is_ready() {
                        self.finish();
                        return Poll::Ready(Some(Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "upstream read timed out",
                        ))));
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}

/// Either a buffered JSON answer from the forwarder itself or a live relay.
///
/// The large variant is the hot one, so boxing it would trade a free move for
/// an allocation on every proxied request.
#[allow(clippy::large_enum_variant)]
pub enum OutBody {
    Fixed(Option<Bytes>),
    Relay(RelayBody),
}

impl OutBody {
    pub fn fixed(bytes: Bytes) -> Self {
        OutBody::Fixed(if bytes.is_empty() { None } else { Some(bytes) })
    }
}

impl Body for OutBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match self.get_mut() {
            OutBody::Fixed(slot) => Poll::Ready(slot.take().map(|b| Ok(Frame::data(b)))),
            OutBody::Relay(relay) => Pin::new(relay).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, OutBody::Fixed(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            OutBody::Fixed(None) => SizeHint::with_exact(0),
            OutBody::Fixed(Some(bytes)) => SizeHint::with_exact(bytes.len() as u64),
            OutBody::Relay(_) => SizeHint::default(),
        }
    }
}

pub fn json_response(status: StatusCode, value: serde_json::Value) -> Response<OutBody> {
    let bytes = Bytes::from(serde_json::to_vec(&value).expect("json value serializes"));
    Response::builder()
        .status(status)
        .header(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        )
        .body(OutBody::fixed(bytes))
        .expect("json response is well formed")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// What the sink saw per record: flights still registered, and the id.
    type Seen = Arc<Mutex<Vec<(usize, Option<u64>)>>>;

    /// A reader that sees the record must not still see the request in
    /// flight: the registry is emptied before the sink hears about it, and
    /// the record names the flight it ended.
    #[test]
    fn the_flight_leaves_before_its_record_is_emitted() {
        let seen: Seen = Arc::default();
        let telemetry = Arc::new_cyclic(|weak: &std::sync::Weak<Telemetry>| {
            let weak = weak.clone();
            let seen = Arc::clone(&seen);
            Telemetry::new(move |event| {
                if let (Event::Request(record), Some(telemetry)) = (event, weak.upgrade()) {
                    let flying = telemetry.flights().len();
                    seen.lock().unwrap().push((flying, record.flight));
                }
            })
        });
        let stats = Arc::new(Stats::default());
        let in_flight = InFlight::new(&stats, &telemetry, "alpha", "", &Method::POST, "/v1/x", 3);
        let id = in_flight.flight().id();
        let log = RequestLog::new(
            "alpha".into(),
            String::new(),
            None,
            Method::POST,
            "/v1/x".into(),
            200,
            Arc::new(PhaseClock::new()),
            3,
            3,
            Coding::None,
            0.1,
            "identity".into(),
            None,
            Arc::clone(&stats),
            in_flight,
            Arc::clone(&telemetry),
        );
        assert_eq!(telemetry.flights().len(), 1);
        assert_eq!(stats.in_flight.load(Ordering::Relaxed), 1);
        drop(log);
        assert_eq!(*seen.lock().unwrap(), vec![(0, Some(id))]);
        assert_eq!(stats.in_flight.load(Ordering::Relaxed), 0);
    }
}
