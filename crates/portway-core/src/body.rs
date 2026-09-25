//! The request body, the IO wrapper that times it, and the streaming response
//! decoder.

use std::io;
use std::io::Write as _;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use http::HeaderMap;
use http_body::{Body, Frame, SizeHint};
use hyper::body::Incoming;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::ack;
use crate::clock::PhaseClock;
use crate::telemetry::Telemetry;

/// The whole request body as one frame, with the upload timestamped.
///
/// `size_hint` is exact so hyper frames the request with `Content-Length`
/// rather than `Transfer-Encoding: chunked`; the handler also sets the header
/// explicitly, which hyper honors over the body's own hint.
pub struct TimedBody {
    data: Option<Bytes>,
    meter: Option<Arc<crate::origin::UploadMeter>>,
    clock: Arc<PhaseClock>,
}

impl TimedBody {
    pub fn new(data: Bytes, clock: Arc<PhaseClock>) -> Self {
        let data = if data.is_empty() { None } else { Some(data) };
        TimedBody {
            data,
            clock,
            meter: None,
        }
    }

    pub(crate) fn metered(mut self, meter: Option<Arc<crate::origin::UploadMeter>>) -> Self {
        self.meter = meter;
        self
    }

    pub fn empty(clock: Arc<PhaseClock>) -> Self {
        TimedBody {
            data: None,
            clock,
            meter: None,
        }
    }
}

impl Body for TimedBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match self.data.take() {
            Some(data) => {
                if let Some(meter) = &self.meter {
                    meter.add(data.len());
                }
                self.clock.mark_upload_started();
                // The only frame there will ever be: from here the next write
                // that completes on this connection ends the upload.
                self.clock.mark_body_handed();
                Poll::Ready(Some(Ok(Frame::data(data))))
            }
            None => Poll::Ready(None),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.data.as_ref().map_or(0, |d| d.len() as u64))
    }
}

/// Which request is currently using a pooled connection.
///
/// HTTP/1.1 is strictly serial, so a connection has at most one clock at a
/// time. `armed` keeps the common path (every write after the body is out)
/// off the mutex.
#[derive(Default)]
pub struct ClockSlot {
    armed: AtomicBool,
    clock: Mutex<Option<Arc<PhaseClock>>>,
}

impl ClockSlot {
    pub fn set(&self, clock: Arc<PhaseClock>) {
        *self.clock.lock().unwrap() = Some(clock);
        self.armed.store(true, Ordering::Release);
    }

    pub fn clear(&self) {
        self.armed.store(false, Ordering::Release);
        *self.clock.lock().unwrap() = None;
    }

    /// Called after a write completes: if the body's last frame is already out,
    /// that write finished the upload.
    fn on_write_complete(&self) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        let held = self.clock.lock().unwrap();
        let Some(clock) = held.as_ref() else { return };
        if !clock.body_handed() {
            return;
        }
        if clock.mark_upload_finished() {
            let clock = Arc::clone(clock);
            drop(held);
            self.armed.store(false, Ordering::Release);
            ack::spawn_watch(clock);
        }
    }
}

/// Delegating stream wrapper that stamps the end of the request body write,
/// and counts the bytes crossing it for the live throughput counters.
///
/// hyper writes the body from the connection driver task, so the body itself
/// cannot observe when its bytes actually reached the socket; this can.
pub struct TimedIo<S> {
    inner: S,
    slot: Arc<ClockSlot>,
    telemetry: Arc<Telemetry>,
}

impl<S> TimedIo<S> {
    pub fn new(inner: S, slot: Arc<ClockSlot>, telemetry: Arc<Telemetry>) -> Self {
        TimedIo {
            inner,
            slot,
            telemetry,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for TimedIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let done = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &done {
            self.telemetry.add_socket_down(buf.filled().len() - before);
        }
        done
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for TimedIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let done = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = &done {
            self.telemetry.add_socket_up(*written);
            self.slot.on_write_complete();
        }
        done
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let done = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(written)) = &done {
            self.telemetry.add_socket_up(*written);
            self.slot.on_write_complete();
        }
        done
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let done = Pin::new(&mut self.inner).poll_flush(cx);
        if let Poll::Ready(Ok(())) = &done {
            self.slot.on_write_complete();
        }
        done
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Reusable output window. Sized so an SSE chunk almost always decodes in one
/// pass, and initialized once so no per-chunk zeroing happens.
const SCRATCH: usize = 32 * 1024;

enum Kind {
    Identity,
    Flate(flate2::Decompress),
    Zstd(Box<zstd::stream::raw::Decoder<'static>>),
}

/// Push-style response decoder: every upstream chunk goes in and whatever the
/// codec can already produce comes straight out, so SSE cadence is preserved.
pub struct Decoder {
    kind: Kind,
    scratch: Vec<u8>,
}

impl Decoder {
    /// `None` for a coding we did not ask for and cannot decode; the caller
    /// then relays the bytes untouched with the header intact.
    pub fn for_encoding(encoding: &str) -> Option<Self> {
        let kind = match encoding.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => Kind::Identity,
            "gzip" | "x-gzip" => Kind::Flate(flate2::Decompress::new_gzip(15)),
            // Real-world `deflate` is overwhelmingly zlib-wrapped.
            "deflate" => Kind::Flate(flate2::Decompress::new(true)),
            "zstd" => Kind::Zstd(Box::new(zstd::stream::raw::Decoder::new().ok()?)),
            _ => return None,
        };
        let scratch = match kind {
            Kind::Identity => Vec::new(),
            _ => vec![0u8; SCRATCH],
        };
        Some(Decoder { kind, scratch })
    }

    pub fn identity() -> Self {
        Decoder {
            kind: Kind::Identity,
            scratch: Vec::new(),
        }
    }

    pub fn is_identity(&self) -> bool {
        matches!(self.kind, Kind::Identity)
    }

    pub fn push(&mut self, chunk: Bytes) -> io::Result<Bytes> {
        match &mut self.kind {
            Kind::Identity => Ok(chunk),
            Kind::Flate(state) => inflate(state, &chunk, &mut self.scratch),
            Kind::Zstd(state) => unzstd(state, &chunk, &mut self.scratch),
        }
    }
}

fn inflate(state: &mut flate2::Decompress, input: &[u8], scratch: &mut [u8]) -> io::Result<Bytes> {
    let mut out = BytesMut::new();
    let mut rest = input;
    loop {
        let read_before = state.total_in();
        let wrote_before = state.total_out();
        let status = state
            .decompress(rest, scratch, flate2::FlushDecompress::None)
            .map_err(io::Error::other)?;
        let read = (state.total_in() - read_before) as usize;
        let wrote = (state.total_out() - wrote_before) as usize;
        out.extend_from_slice(&scratch[..wrote]);
        rest = &rest[read..];
        if status == flate2::Status::StreamEnd || (read == 0 && wrote == 0) {
            break;
        }
        // Output window filled: go around for the rest of this input.
        if rest.is_empty() && wrote < scratch.len() {
            break;
        }
    }
    Ok(out.freeze())
}

fn unzstd(
    state: &mut zstd::stream::raw::Decoder<'static>,
    input: &[u8],
    scratch: &mut [u8],
) -> io::Result<Bytes> {
    use zstd::stream::raw::Operation;
    let mut out = BytesMut::new();
    let mut rest = input;
    loop {
        let status = state.run_on_buffers(rest, scratch)?;
        out.extend_from_slice(&scratch[..status.bytes_written]);
        rest = &rest[status.bytes_read..];
        if status.bytes_read == 0 && status.bytes_written == 0 {
            break;
        }
        if rest.is_empty() && status.bytes_written < scratch.len() {
            break;
        }
    }
    Ok(out.freeze())
}

enum EncKind {
    Zstd(Box<zstd::stream::write::Encoder<'static, Vec<u8>>>),
    Gzip(Box<flate2::write::GzEncoder<Vec<u8>>>),
    /// `finish` took the codec; nothing more can be written.
    Done,
}

/// Push-style response encoder: the mirror of [`Decoder`], for an agent that
/// offered a coding we can produce. Each decoded chunk is written and flushed
/// on the spot, so the compressed stream keeps the SSE cadence instead of
/// waiting for a codec block to fill.
pub struct Encoder {
    kind: EncKind,
}

impl Encoder {
    /// The best coding in an `Accept-Encoding` header, or `None` when the agent
    /// offered nothing we make: `identity`, `deflate`, `br`, or `q=0`.
    pub fn for_accept(accept: Option<&str>) -> Option<Self> {
        let (mut zstd, mut gzip) = (false, false);
        for part in accept?.split(',') {
            let mut fields = part.split(';');
            let name = fields.next().unwrap_or("").trim().to_ascii_lowercase();
            let refused = fields.any(|field| {
                field
                    .trim()
                    .strip_prefix("q=")
                    .and_then(|q| q.trim().parse::<f32>().ok())
                    .is_some_and(|q| q <= 0.0)
            });
            if refused {
                continue;
            }
            match name.as_str() {
                "zstd" => zstd = true,
                "gzip" | "x-gzip" => gzip = true,
                _ => {}
            }
        }
        let kind = if zstd {
            EncKind::Zstd(Box::new(
                zstd::stream::write::Encoder::new(Vec::new(), 3).ok()?,
            ))
        } else if gzip {
            EncKind::Gzip(Box::new(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::fast(),
            )))
        } else {
            return None;
        };
        Some(Encoder { kind })
    }

    /// The coding to name in the response header. `for_accept` decided it.
    pub fn name(&self) -> &'static str {
        match self.kind {
            EncKind::Zstd(_) => "zstd",
            EncKind::Gzip(_) => "gzip",
            EncKind::Done => "identity",
        }
    }

    /// Encode one decoded chunk, flushing so the agent can act on it now.
    pub fn push(&mut self, chunk: &[u8]) -> io::Result<Bytes> {
        match &mut self.kind {
            EncKind::Zstd(encoder) => {
                encoder.write_all(chunk)?;
                encoder.flush()?;
                Ok(take(encoder.get_mut()))
            }
            EncKind::Gzip(encoder) => {
                encoder.write_all(chunk)?;
                encoder.flush()?;
                Ok(take(encoder.get_mut()))
            }
            EncKind::Done => Ok(Bytes::new()),
        }
    }

    /// The codec's epilogue. Sending it is what makes the stream complete:
    /// without it the agent decodes a truncated body.
    pub fn finish(&mut self) -> io::Result<Bytes> {
        match std::mem::replace(&mut self.kind, EncKind::Done) {
            EncKind::Zstd(encoder) => Ok(Bytes::from(encoder.finish()?)),
            EncKind::Gzip(encoder) => Ok(Bytes::from(encoder.finish()?)),
            EncKind::Done => Ok(Bytes::new()),
        }
    }
}

/// Take what the codec produced, keeping the buffer's capacity for the next
/// chunk. Frames are tens of bytes, so the copy is cheaper than a realloc.
fn take(buf: &mut Vec<u8>) -> Bytes {
    if buf.is_empty() {
        return Bytes::new();
    }
    let out = Bytes::copy_from_slice(buf);
    buf.clear();
    out
}

/// Read a whole response body, decoding whatever coding it arrived in.
pub async fn collect(headers: &HeaderMap, body: Incoming) -> Result<Bytes, hyper::Error> {
    use http_body_util::BodyExt;
    let raw = body.collect().await?.to_bytes();
    let encoding = headers
        .get(http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("identity");
    match Decoder::for_encoding(encoding) {
        Some(mut decoder) => Ok(decoder.push(raw.clone()).unwrap_or(raw)),
        None => Ok(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAMES: [&[u8]; 3] = [
        b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n",
        b"data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n",
        b"data: [DONE]\n\n",
    ];

    #[test]
    fn an_encoded_stream_round_trips() {
        for coding in ["zstd", "gzip"] {
            let mut encoder = Encoder::for_accept(Some(coding)).expect("coding is offered");
            assert_eq!(encoder.name(), coding);
            let mut wire = Vec::new();
            for frame in FRAMES {
                wire.extend_from_slice(&encoder.push(frame).expect("encode"));
            }
            wire.extend_from_slice(&encoder.finish().expect("finish"));
            let mut decoder = Decoder::for_encoding(coding).expect("known coding");
            let decoded = decoder.push(Bytes::from(wire)).expect("decode");
            assert_eq!(decoded.to_vec(), FRAMES.concat(), "{coding} lost a frame");
        }
    }

    #[test]
    fn every_frame_is_flushed_so_the_stream_stays_incremental() {
        for coding in ["zstd", "gzip"] {
            let mut encoder = Encoder::for_accept(Some(coding)).expect("coding is offered");
            for frame in 0..8 {
                let out = encoder.push(FRAMES[0]).expect("encode");
                assert!(!out.is_empty(), "{coding} frame {frame} emitted nothing");
            }
        }
    }

    #[test]
    fn the_offer_decides_the_coding() {
        assert!(Encoder::for_accept(None).is_none());
        assert!(Encoder::for_accept(Some("")).is_none());
        assert!(Encoder::for_accept(Some("identity")).is_none());
        assert!(Encoder::for_accept(Some("br, deflate")).is_none());
        assert!(Encoder::for_accept(Some("*")).is_none());
        assert!(Encoder::for_accept(Some("gzip;q=0")).is_none());
        assert!(Encoder::for_accept(Some("gzip;q=0.0, zstd;q=0")).is_none());
        assert_eq!(
            Encoder::for_accept(Some("gzip")).expect("gzip").name(),
            "gzip"
        );
        assert_eq!(
            Encoder::for_accept(Some("x-gzip"))
                .expect("x-gzip is gzip")
                .name(),
            "gzip"
        );
        assert_eq!(
            Encoder::for_accept(Some("gzip, deflate, zstd"))
                .expect("zstd wins")
                .name(),
            "zstd"
        );
        assert_eq!(
            Encoder::for_accept(Some("zstd;q=0, gzip;q=0.5"))
                .expect("gzip is left")
                .name(),
            "gzip"
        );
    }
}

/// A bounded, byte-preserving request read. Content-Encoding is not interpreted.
#[derive(Debug)]
pub enum BodyReadError {
    TooLarge,
    Read(String),
}
impl BodyReadError {
    pub fn status(&self) -> http::StatusCode {
        match self {
            Self::TooLarge => http::StatusCode::PAYLOAD_TOO_LARGE,
            Self::Read(_) => http::StatusCode::BAD_REQUEST,
        }
    }
}
impl std::fmt::Display for BodyReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "request body exceeds configured limit"),
            Self::Read(_) => write!(f, "could not read request body"),
        }
    }
}
impl std::error::Error for BodyReadError {}
pub async fn collect_raw<B>(mut body: B, limit: usize) -> Result<Bytes, BodyReadError>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    use http_body_util::BodyExt;
    if body.size_hint().lower() > limit as u64 {
        return Err(BodyReadError::TooLarge);
    }
    let mut out = bytes::BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| BodyReadError::Read(e.to_string()))?;
        if let Ok(data) = frame.into_data() {
            if data.len() > limit.saturating_sub(out.len()) {
                return Err(BodyReadError::TooLarge);
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(out.freeze())
}
