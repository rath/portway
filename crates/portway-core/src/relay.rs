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

/// Everything the one-line request log needs, rendered when the relay ends —
/// including when the agent disconnects mid-stream.
pub struct RequestLog {
    telemetry: Arc<Telemetry>,
    model: String,
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
        model: String,
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
            model,
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
            model: self.model.clone(),
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
            complete: self.complete,
            usage: self.usage,
        }
    }
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        self.telemetry.emit(Event::Request(Arc::new(self.record())));
    }
}

/// Streams the upstream response to the agent, decoding as it goes.
///
/// Dropping this — which is what an agent disconnect does — drops the upstream
/// body, and hyper then closes that HTTP/1.1 connection. That disconnect is
/// what makes the engine abort the generation.
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
        // Only a body read to completion leaves the connection reusable.
        if let Some(lease) = self.lease.as_mut() {
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
    }
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
                        if log.first_byte.is_none() {
                            log.first_byte = Some(Instant::now());
                        }
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
