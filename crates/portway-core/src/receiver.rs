//! Request decompression to place after authentication and before an HTTP service.
//! Dictionaries live in bounded memory; neither content nor credentials are logged.
use crate::{
    dict::{self, DictionaryScope, Hash},
    relay::{OutBody, json_response},
};
use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode};
use http_body::Body;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const ERROR_HEADER: &str = "x-portway-decode-error";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReceiverConfig {
    /// Standalone forwarding policy; embedding users apply it to their forwarder.
    pub origin_compression: crate::origin::OriginCompressionConfig,
    pub dictionary_bytes: usize,
    pub dictionary_ttl_seconds: u64,
    pub min_dictionary_bytes: usize,
    pub max_dictionary_bytes: usize,
    pub max_body_bytes: usize,
    pub max_window_bytes: usize,
}
impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            origin_compression: Default::default(),
            dictionary_bytes: 256 << 20,
            dictionary_ttl_seconds: 3600,
            min_dictionary_bytes: 32 << 10,
            max_dictionary_bytes: dict::MAX_BASE_BYTES,
            max_body_bytes: 256 << 20,
            max_window_bytes: dict::MAX_WINDOW_BYTES,
        }
    }
}
impl ReceiverConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_body_bytes == 0
            || self.dictionary_ttl_seconds == 0
            || self.min_dictionary_bytes == 0
            || self.min_dictionary_bytes > self.max_dictionary_bytes
            || self.max_dictionary_bytes > self.max_body_bytes
        {
            return Err(
                "receiver body, dictionary and TTL limits must be positive and ordered".into(),
            );
        }
        if !self.max_window_bytes.is_power_of_two()
            || !(1024..=dict::MAX_WINDOW_BYTES).contains(&self.max_window_bytes)
        {
            return Err(format!(
                "max_window_bytes must be a power of two between 1024 and {}",
                dict::MAX_WINDOW_BYTES
            ));
        }
        Ok(())
    }
}
struct Entry {
    scope: DictionaryScope,
    hash: Hash,
    body: Bytes,
    used: Instant,
}
#[derive(Default)]
struct Store {
    entries: VecDeque<Entry>,
    bytes: usize,
}
impl Store {
    fn expire(&mut self, ttl: Duration) {
        while self
            .entries
            .front()
            .is_some_and(|e| e.used.elapsed() >= ttl)
        {
            self.bytes -= self.entries.pop_front().expect("front exists").body.len();
        }
    }
    fn get(&mut self, scope: &DictionaryScope, hash: &Hash, ttl: Duration) -> Option<Bytes> {
        self.expire(ttl);
        let at = self
            .entries
            .iter()
            .position(|e| &e.scope == scope && &e.hash == hash)?;
        let mut entry = self.entries.remove(at)?;
        entry.used = Instant::now();
        let body = entry.body.clone();
        self.entries.push_back(entry);
        Some(body)
    }
    fn put(
        &mut self,
        scope: DictionaryScope,
        body: Bytes,
        config: &ReceiverConfig,
    ) -> Option<Hash> {
        if body.len() < config.min_dictionary_bytes
            || body.len() > config.max_dictionary_bytes
            || body.len() > config.dictionary_bytes
        {
            return None;
        }
        self.expire(Duration::from_secs(config.dictionary_ttl_seconds));
        let hash = dict::sha256(&body);
        if let Some(at) = self
            .entries
            .iter()
            .position(|e| e.scope == scope && e.hash == hash)
        {
            self.bytes -= self.entries.remove(at).expect("entry exists").body.len();
        }
        while body.len() > config.dictionary_bytes.saturating_sub(self.bytes) {
            self.bytes -= self
                .entries
                .pop_front()
                .expect("nonempty while over budget")
                .body
                .len();
        }
        self.bytes += body.len();
        self.entries.push_back(Entry {
            scope,
            hash,
            body,
            used: Instant::now(),
        });
        Some(hash)
    }
}
#[derive(Default)]
struct Counters {
    decoded: AtomicU64,
    wire: AtomicU64,
    plain: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    stored: AtomicU64,
}
pub struct Receiver {
    config: ReceiverConfig,
    store: Mutex<Store>,
    counters: Counters,
}
/// The uncompressed request and an acknowledgement to attach to its response.
/// `finish` never changes the application's status or body.
pub struct DecodedRequest {
    pub request: Request<Bytes>,
    acknowledgement: Option<Hash>,
}
impl DecodedRequest {
    pub fn into_parts(self) -> (Request<Bytes>, Acknowledgement) {
        (self.request, Acknowledgement(self.acknowledgement))
    }
}
pub struct Acknowledgement(Option<Hash>);
impl Acknowledgement {
    pub fn finish<B>(self, mut response: Response<B>) -> Response<B> {
        // Only the decoder is allowed to signal a retry. Origin headers cannot.
        response.headers_mut().remove(ERROR_HEADER);
        response.headers_mut().remove(dict::MISS_HEADER);
        response.headers_mut().remove(dict::STORED_HEADER);
        if let Some(hash) = self.0 {
            response.headers_mut().insert(
                dict::STORED_HEADER,
                HeaderValue::from_str(&dict::hex(&hash)).expect("hex hash"),
            );
        }
        response
    }
}
impl Receiver {
    pub fn new(config: ReceiverConfig) -> Result<Arc<Self>, String> {
        config.validate()?;
        Ok(Arc::new(Self {
            config,
            store: Mutex::default(),
            counters: Counters::default(),
        }))
    }
    pub fn capabilities(&self) -> Response<OutBody> {
        let mut response = json_response(
            StatusCode::OK,
            serde_json::json!({"request_encodings":["zstd","gzip"]}),
        );
        response.headers_mut().insert(
            "x-request-encodings",
            HeaderValue::from_static("zstd, gzip"),
        );
        if self.config.dictionary_bytes > 0 {
            response
                .headers_mut()
                .insert(dict::ADVERTISE_HEADER, HeaderValue::from_static("dcz"));
        }
        response
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let counters = &self.counters;
        let store = self.store.lock().expect("dictionary store");
        serde_json::json!({"decoded_requests":counters.decoded.load(Ordering::Relaxed),
            "wire_bytes":counters.wire.load(Ordering::Relaxed),"body_bytes":counters.plain.load(Ordering::Relaxed),
            "dict_hits":counters.hits.load(Ordering::Relaxed),"dict_misses":counters.misses.load(Ordering::Relaxed),
            "dict_stored":counters.stored.load(Ordering::Relaxed),"dictionary_bytes":store.bytes,"dictionary_entries":store.entries.len()})
    }
    /// Call only after your authentication layer has accepted the request.
    /// A DictionaryScope extension overrides the Authorization/Cookie partition.
    pub async fn decode<B>(
        self: &Arc<Self>,
        request: Request<B>,
    ) -> Result<DecodedRequest, DecodeError>
    where
        B: Body<Data = Bytes> + Unpin,
        B::Error: std::fmt::Display,
    {
        let (mut parts, body) = request.into_parts();
        let scope = parts
            .extensions
            .get::<DictionaryScope>()
            .cloned()
            .unwrap_or_else(|| DictionaryScope::from_headers(&parts.headers));
        if parts
            .headers
            .get_all(http::header::CONTENT_ENCODING)
            .iter()
            .count()
            > 1
        {
            return Err(reject(415, "multiple content encodings are unsupported"));
        }
        let coding = parts
            .headers
            .get(http::header::CONTENT_ENCODING)
            .map(|v| v.to_str().unwrap_or("unsupported"))
            .unwrap_or("identity")
            .trim()
            .to_ascii_lowercase();
        if !matches!(
            coding.as_str(),
            "identity" | "zstd" | "gzip" | "x-gzip" | "dcz"
        ) || (coding == "dcz" && self.config.dictionary_bytes == 0)
        {
            return Err(reject(415, "unsupported content encoding"));
        }
        let keep = parts
            .headers
            .get(dict::STORE_HEADER)
            .is_some_and(|v| v == "1");
        let raw = crate::body::collect_raw(body, self.config.max_body_bytes)
            .await
            .map_err(|err| reject(err.status().as_u16(), &err.to_string()))?;
        let wire = raw.len();
        let encoded = coding != "identity";
        let receiver = Arc::clone(self);
        let (body, acknowledgement) = if encoded {
            tokio::task::spawn_blocking(move || receiver.inflate(&coding, raw, scope, keep))
                .await
                .map_err(|_| reject(500, "decompression worker failed"))??
        } else {
            (raw, None)
        };
        if encoded {
            self.counters.decoded.fetch_add(1, Ordering::Relaxed);
            self.counters.wire.fetch_add(wire as u64, Ordering::Relaxed);
            self.counters
                .plain
                .fetch_add(body.len() as u64, Ordering::Relaxed);
            parts.headers.remove(http::header::CONTENT_ENCODING);
        }
        parts.headers.remove(dict::STORE_HEADER);
        parts.headers.remove(dict::STORED_HEADER);
        parts.headers.remove(dict::MISS_HEADER);
        parts.headers.remove(ERROR_HEADER);
        parts.headers.remove(http::header::TRANSFER_ENCODING);
        parts.headers.insert(
            http::header::CONTENT_LENGTH,
            HeaderValue::from(body.len() as u64),
        );
        Ok(DecodedRequest {
            request: Request::from_parts(parts, body),
            acknowledgement,
        })
    }
    /// Minimal middleware adapter for handlers returning Portway's streaming body.
    pub async fn handle<B, F, Fut>(
        self: &Arc<Self>,
        request: Request<B>,
        next: F,
    ) -> Response<OutBody>
    where
        B: Body<Data = Bytes> + Unpin,
        B::Error: std::fmt::Display,
        F: FnOnce(Request<Bytes>) -> Fut,
        Fut: std::future::Future<Output = Response<OutBody>>,
    {
        if request.method() == http::Method::GET
            && request.uri().path() == crate::router::CAPABILITIES_PATH
        {
            return self.capabilities();
        }
        let decoded = match self.decode(request).await {
            Ok(d) => d,
            Err(error) => return error.into_response(),
        };
        let (request, ack) = decoded.into_parts();
        ack.finish(next(request).await)
    }
    fn inflate(
        &self,
        coding: &str,
        raw: Bytes,
        scope: DictionaryScope,
        keep: bool,
    ) -> Result<(Bytes, Option<Hash>), DecodeError> {
        let body = match coding {
            "gzip" | "x-gzip" => inflate_gzip(&raw, self.config.max_body_bytes)?,
            "zstd" => inflate_zstd(&raw, None, &self.config)?,
            "dcz" => {
                if raw.len() < 40 || raw[..8] != dict::DCZ_MAGIC {
                    return Err(reject(400, "invalid dcz header"));
                }
                let hash: Hash = raw[8..40].try_into().expect("32 byte hash");
                let base = self.store.lock().expect("dictionary store").get(
                    &scope,
                    &hash,
                    Duration::from_secs(self.config.dictionary_ttl_seconds),
                );
                let Some(base) = base else {
                    self.counters.misses.fetch_add(1, Ordering::Relaxed);
                    return Err(reject(412, "dictionary not found"));
                };
                let body = inflate_zstd(&raw[40..], Some(&base), &self.config)?;
                self.counters.hits.fetch_add(1, Ordering::Relaxed);
                body
            }
            _ => return Err(reject(415, "unsupported content encoding")),
        };
        let hash = if keep && self.config.dictionary_bytes > 0 {
            self.store
                .lock()
                .expect("dictionary store")
                .put(scope, body.clone(), &self.config)
        } else {
            None
        };
        if hash.is_some() {
            self.counters.stored.fetch_add(1, Ordering::Relaxed);
        }
        Ok((body, hash))
    }
}
/// A request rejected before the application is invoked.
#[derive(Debug)]
pub struct DecodeError {
    status: StatusCode,
    message: String,
}
impl DecodeError {
    pub fn status(&self) -> StatusCode {
        self.status
    }
    pub fn into_response(self) -> Response<OutBody> {
        let mut response = json_response(
            self.status,
            serde_json::json!({"error":{"message":self.message,"type":"request_decoding_error"}}),
        );
        response
            .headers_mut()
            .insert(ERROR_HEADER, HeaderValue::from_static("1"));
        if self.status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
            response.headers_mut().insert(
                http::header::ACCEPT_ENCODING,
                HeaderValue::from_static("zstd, gzip"),
            );
        }
        if self.status == StatusCode::PRECONDITION_FAILED {
            response
                .headers_mut()
                .insert(dict::MISS_HEADER, HeaderValue::from_static("1"));
        }
        response
    }
}
impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for DecodeError {}
fn reject(status: u16, message: &str) -> DecodeError {
    DecodeError {
        status: StatusCode::from_u16(status).expect("valid status"),
        message: message.into(),
    }
}
fn inflate_gzip(raw: &[u8], limit: usize) -> Result<Bytes, DecodeError> {
    let mut decoder = flate2::Decompress::new_gzip(15);
    let mut out = Vec::new();
    let mut scratch = [0; 65536];
    loop {
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let status = decoder
            .decompress(
                &raw[before_in as usize..],
                &mut scratch,
                flate2::FlushDecompress::None,
            )
            .map_err(|_| reject(400, "invalid gzip stream"))?;
        let written = (decoder.total_out() - before_out) as usize;
        if written > limit.saturating_sub(out.len()) {
            return Err(reject(413, "decoded body exceeds configured limit"));
        }
        out.extend_from_slice(&scratch[..written]);
        if status == flate2::Status::StreamEnd {
            if decoder.total_in() != raw.len() as u64 {
                return Err(reject(400, "trailing gzip data"));
            }
            return Ok(out.into());
        }
        if decoder.total_in() == before_in && written == 0 {
            return Err(reject(400, "truncated gzip stream"));
        }
    }
}
fn inflate_zstd(
    raw: &[u8],
    base: Option<&[u8]>,
    config: &ReceiverConfig,
) -> Result<Bytes, DecodeError> {
    use zstd::zstd_safe::{DCtx, DParameter, InBuffer, OutBuffer};
    if raw.len() < 5 || raw[..4] != [0x28, 0xb5, 0x2f, 0xfd] {
        return Err(reject(400, "invalid zstd frame"));
    }
    if base.is_some() && raw[4] & 4 == 0 {
        return Err(reject(400, "dcz requires a content checksum"));
    }
    let mut decoder = DCtx::create();
    decoder
        .set_parameter(DParameter::WindowLogMax(config.max_window_bytes.ilog2()))
        .map_err(|_| reject(400, "invalid zstd window limit"))?;
    if let Some(base) = base {
        decoder
            .ref_prefix(base)
            .map_err(|_| reject(400, "invalid dictionary"))?;
    }
    let mut input = InBuffer::around(raw);
    let mut out = Vec::new();
    let mut scratch = [0; 65536];
    loop {
        let before = input.pos();
        let mut output = OutBuffer::around(&mut scratch);
        let remaining = decoder
            .decompress_stream(&mut output, &mut input)
            .map_err(|_| reject(400, "invalid zstd stream"))?;
        let written = output.pos();
        if written > config.max_body_bytes.saturating_sub(out.len()) {
            return Err(reject(413, "decoded body exceeds configured limit"));
        }
        out.extend_from_slice(&scratch[..written]);
        if remaining == 0 {
            if input.pos() != raw.len() {
                return Err(reject(400, "trailing zstd data"));
            }
            return Ok(out.into());
        }
        if input.pos() == before && written == 0 {
            return Err(reject(400, "truncated zstd stream"));
        }
    }
}

/// Capability documents are small; decoding them has the same bounded codec rules.
pub(crate) fn decode_probe(raw: &[u8], encoding: &str) -> Result<Bytes, DecodeError> {
    let config = ReceiverConfig {
        max_body_bytes: 1 << 20,
        ..ReceiverConfig::default()
    };
    match encoding.trim().to_ascii_lowercase().as_str() {
        "identity" | "" => Ok(Bytes::copy_from_slice(raw)),
        "gzip" | "x-gzip" => inflate_gzip(raw, config.max_body_bytes),
        "zstd" => inflate_zstd(raw, None, &config),
        _ => Err(reject(415, "unsupported capability response encoding")),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dictionary_expiration_and_recent_use_follow_lru_order() {
        let config = ReceiverConfig {
            min_dictionary_bytes: 1,
            max_dictionary_bytes: 8,
            dictionary_bytes: 8,
            ..ReceiverConfig::default()
        };
        let mut store = Store::default();
        let scope = DictionaryScope::default();
        let hash = store
            .put(scope.clone(), Bytes::from_static(b"one"), &config)
            .unwrap();
        store.entries[0].used = Instant::now() - Duration::from_secs(3601);
        assert!(
            store
                .get(&scope, &hash, Duration::from_secs(3600))
                .is_none()
        );
        assert_eq!(store.bytes, 0);
        let first = store
            .put(scope.clone(), Bytes::from_static(b"aaa"), &config)
            .unwrap();
        let second = store
            .put(scope.clone(), Bytes::from_static(b"bbb"), &config)
            .unwrap();
        assert!(
            store
                .get(&scope, &first, Duration::from_secs(3600))
                .is_some()
        );
        store.put(scope.clone(), Bytes::from_static(b"ccc"), &config);
        assert!(
            store
                .get(&scope, &second, Duration::from_secs(3600))
                .is_none()
        );
        assert!(
            store
                .get(&scope, &first, Duration::from_secs(3600))
                .is_some()
        );
    }
}
