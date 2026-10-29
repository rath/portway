//! The request body, the IO wrapper that times it, and the streaming response
//! decoder.

use std::io;
use std::io::Write as _;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use bytes::{Buf, Bytes, BytesMut};
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
    Flate(flate2::Decompress, bool),
    Zstd(Box<zstd::stream::raw::Decoder<'static>>),
}

/// Push-style response decoder: every upstream chunk goes in and whatever the
/// codec can already produce comes straight out, so SSE cadence is preserved.
pub struct Decoder {
    kind: Kind,
    scratch: Vec<u8>,
    ended: bool,
    input: Bytes,
    needs_drain: bool,
}

impl Decoder {
    /// `None` for a coding we did not ask for and cannot decode; the caller
    /// then relays the bytes untouched with the header intact.
    pub fn for_encoding(encoding: &str) -> Option<Self> {
        let kind = match encoding.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => Kind::Identity,
            "gzip" | "x-gzip" => Kind::Flate(flate2::Decompress::new_gzip(15), true),
            // Real-world `deflate` is overwhelmingly zlib-wrapped.
            "deflate" => Kind::Flate(flate2::Decompress::new(true), false),
            "zstd" => {
                let mut decoder = zstd::stream::raw::Decoder::new().ok()?;
                decoder
                    .set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(
                        crate::dict::MAX_WINDOW_BYTES.ilog2(),
                    ))
                    .ok()?;
                Kind::Zstd(Box::new(decoder))
            }
            _ => return None,
        };
        let scratch = match kind {
            Kind::Identity => Vec::new(),
            _ => vec![0u8; SCRATCH],
        };
        Some(Decoder {
            kind,
            scratch,
            ended: false,
            input: Bytes::new(),
            needs_drain: false,
        })
    }

    pub fn identity() -> Self {
        Decoder {
            kind: Kind::Identity,
            scratch: Vec::new(),
            ended: false,
            input: Bytes::new(),
            needs_drain: false,
        }
    }

    pub fn is_identity(&self) -> bool {
        matches!(self.kind, Kind::Identity)
    }

    /// Decode a whole input chunk for callers that explicitly want to buffer it.
    /// The live relay uses `feed`/`next_chunk` to bound expansion and honor backpressure.
    pub fn push(&mut self, chunk: Bytes) -> io::Result<Bytes> {
        if chunk.is_empty() || self.is_identity() {
            return Ok(chunk);
        }
        self.feed(chunk);
        let mut out = BytesMut::new();
        while let Some(chunk) = self.next_chunk()? {
            out.extend_from_slice(&chunk);
        }
        Ok(out.freeze())
    }

    pub(crate) fn feed(&mut self, chunk: Bytes) {
        debug_assert!(self.input.is_empty() && !self.needs_drain);
        self.input = chunk;
    }

    /// One bounded codec step. Empty output can still have consumed a header;
    /// `None` alone means the next input frame is needed.
    pub(crate) fn next_chunk(&mut self) -> io::Result<Option<Bytes>> {
        if self.input.is_empty() && !self.needs_drain {
            return Ok(None);
        }
        let (read, wrote) = match &mut self.kind {
            Kind::Identity => return Ok(Some(std::mem::take(&mut self.input))),
            Kind::Flate(state, gzip) => {
                if self.ended {
                    if !*gzip {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "data after deflate stream",
                        ));
                    }
                    *state = flate2::Decompress::new_gzip(15);
                    self.ended = false;
                }
                let before_in = state.total_in();
                let before_out = state.total_out();
                let status = state
                    .decompress(
                        &self.input,
                        &mut self.scratch,
                        flate2::FlushDecompress::None,
                    )
                    .map_err(io::Error::other)?;
                self.ended = status == flate2::Status::StreamEnd;
                (
                    (state.total_in() - before_in) as usize,
                    (state.total_out() - before_out) as usize,
                )
            }
            Kind::Zstd(state) => {
                use zstd::stream::raw::Operation;
                let status = state.run_on_buffers(&self.input, &mut self.scratch)?;
                if status.bytes_read != 0 || status.bytes_written != 0 {
                    self.ended = status.remaining == 0;
                }
                (status.bytes_read, status.bytes_written)
            }
        };
        self.input.advance(read);
        if read == 0 && wrote == 0 && !self.input.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed response made no progress",
            ));
        }
        self.needs_drain = wrote == self.scratch.len() && !self.ended;
        Ok(Some(Bytes::copy_from_slice(&self.scratch[..wrote])))
    }

    /// HTTP EOF is only complete after the codec has consumed its trailer.
    pub fn finish(&self) -> io::Result<()> {
        if self.input.is_empty() && !self.needs_drain && (self.is_identity() || self.ended) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compressed response ended before its trailer",
            ))
        }
    }
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

    fn compressed(coding: &str, data: &[u8]) -> Vec<u8> {
        if coding == "deflate" {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(data).unwrap();
            return encoder.finish().unwrap();
        }
        let mut encoder = Encoder::for_accept(Some(coding)).unwrap();
        let mut wire = encoder.push(data).unwrap().to_vec();
        wire.extend_from_slice(&encoder.finish().unwrap());
        wire
    }

    #[test]
    fn concatenated_members_survive_every_chunk_boundary() {
        for coding in ["gzip", "zstd"] {
            let mut wire = compressed(coding, b"first ");
            wire.extend(compressed(coding, b"second"));
            for split in 0..=wire.len() {
                let mut decoder = Decoder::for_encoding(coding).unwrap();
                let mut out = decoder
                    .push(Bytes::copy_from_slice(&wire[..split]))
                    .unwrap()
                    .to_vec();
                out.extend_from_slice(
                    &decoder
                        .push(Bytes::copy_from_slice(&wire[split..]))
                        .unwrap(),
                );
                decoder.finish().unwrap();
                assert_eq!(out, b"first second", "{coding}, split {split}");
            }
        }
    }

    #[test]
    fn compressed_eof_requires_a_complete_frame_and_trailer() {
        for coding in ["gzip", "deflate", "zstd"] {
            let wire = compressed(coding, &b"payload".repeat(SCRATCH));
            for end in 0..wire.len() {
                let mut decoder = Decoder::for_encoding(coding).unwrap();
                assert!(
                    decoder.push(Bytes::copy_from_slice(&wire[..end])).is_err()
                        || decoder.finish().is_err(),
                    "{coding} accepted truncation at {end}"
                );
            }
            let mut decoder = Decoder::for_encoding(coding).unwrap();
            assert_eq!(
                decoder.push(Bytes::from(wire)).unwrap(),
                b"payload".repeat(SCRATCH)
            );
            decoder.finish().unwrap();
        }
    }

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
            decoder.finish().expect("complete frame");
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

/// Maximum silence while buffering an inbound request body.
pub const REQUEST_BODY_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug)]
pub enum BodyReadError {
    TooLarge,
    Timeout,
    Read(String),
}
impl BodyReadError {
    pub fn status(&self) -> http::StatusCode {
        match self {
            Self::TooLarge => http::StatusCode::PAYLOAD_TOO_LARGE,
            Self::Timeout => http::StatusCode::REQUEST_TIMEOUT,
            Self::Read(_) => http::StatusCode::BAD_REQUEST,
        }
    }
}
impl std::fmt::Display for BodyReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "request body exceeds configured limit"),
            Self::Timeout => write!(f, "request body read timed out"),
            Self::Read(_) => write!(f, "could not read request body"),
        }
    }
}
impl std::error::Error for BodyReadError {}
/// A bounded, byte-preserving request read. Content-Encoding is not interpreted.
pub async fn collect_raw<B>(mut body: B, limit: usize) -> Result<Bytes, BodyReadError>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    use http_body_util::BodyExt;
    if body.size_hint().lower() > limit as u64 {
        return Err(BodyReadError::TooLarge);
    }
    let mut first = None;
    let mut out = Vec::new();
    let mut len = 0;
    while let Some(frame) = tokio::time::timeout(REQUEST_BODY_IDLE_TIMEOUT, body.frame())
        .await
        .map_err(|_| BodyReadError::Timeout)?
    {
        let frame = frame.map_err(|e| BodyReadError::Read(e.to_string()))?;
        if let Ok(data) = frame.into_data() {
            if data.len() > limit.saturating_sub(len) {
                return Err(BodyReadError::TooLarge);
            }
            if data.is_empty() {
                continue;
            }
            len += data.len();
            if first.is_none() && out.is_empty() {
                first = Some(data);
            } else {
                if let Some(data) = first.take() {
                    out.reserve(len);
                    out.extend_from_slice(&data);
                }
                out.extend_from_slice(&data);
            }
        }
    }
    // A receiver hands its already-decoded body to the router as one frame.
    // Keep that allocation; multiple frames are joined without retaining the
    // growth buffer's spare capacity in a dictionary.
    Ok(first.unwrap_or_else(|| Bytes::from(out.into_boxed_slice())))
}

#[cfg(test)]
mod read_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn an_already_buffered_body_keeps_its_allocation() {
        let bytes = Bytes::from(vec![b'x'; 1 << 20]);
        let pointer = bytes.as_ptr();
        let read = collect_raw(http_body_util::Full::new(bytes), 1 << 20)
            .await
            .unwrap();
        assert_eq!(read.len(), 1 << 20);
        assert_eq!(read.as_ptr(), pointer);
    }

    #[tokio::test]
    async fn multiple_frames_preserve_order_and_enforce_the_total_limit() {
        struct Frames(std::collections::VecDeque<Bytes>);
        impl Body for Frames {
            type Data = Bytes;
            type Error = std::convert::Infallible;
            fn poll_frame(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
                Poll::Ready(self.0.pop_front().map(|bytes| Ok(Frame::data(bytes))))
            }
        }
        let body = || {
            Frames(
                [
                    Bytes::from_static(b"ab"),
                    Bytes::new(),
                    Bytes::from_static(b"cde"),
                ]
                .into(),
            )
        };
        assert_eq!(collect_raw(body(), 5).await.unwrap(), "abcde");
        assert_eq!(
            collect_raw(body(), 4).await.unwrap_err().status(),
            http::StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    struct Drip {
        left: usize,
        interval: Duration,
        next: Pin<Box<tokio::time::Sleep>>,
    }
    impl Drip {
        fn new(seconds: u64, count: usize) -> Self {
            let interval = Duration::from_secs(seconds);
            Self {
                left: count,
                interval,
                next: Box::pin(tokio::time::sleep(interval)),
            }
        }
    }
    impl Body for Drip {
        type Data = Bytes;
        type Error = std::convert::Infallible;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            if self.left == 0 {
                return Poll::Ready(None);
            }
            if self.next.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.left -= 1;
            let next = tokio::time::Instant::now() + self.interval;
            self.next.as_mut().reset(next);
            Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"x")))))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_upload_times_out_but_progress_renews_the_budget() {
        let error = collect_raw(Drip::new(31, 1), 8).await.unwrap_err();
        assert_eq!(error.status(), http::StatusCode::REQUEST_TIMEOUT);
        let started = tokio::time::Instant::now();
        assert_eq!(collect_raw(Drip::new(20, 3), 8).await.unwrap(), "xxx");
        assert!(started.elapsed() >= Duration::from_secs(60));
    }
}
