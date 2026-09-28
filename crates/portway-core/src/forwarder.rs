//! One upstream upstream: negotiate its request coding, compress toward it, relay
//! its response back as identity, and log the single line that describes it.

use crate::origin::{OriginCompressionMode, OriginState, UploadMeter};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use hyper::body::Incoming;

use crate::body::{Decoder, Encoder, TimedBody, collect_raw};
use crate::clock::PhaseClock;
use crate::config::{CodingPreference, DictionaryPreference, ForwarderConfig};
use crate::dict::{self, Ring};
use crate::flights::Flight;
use crate::pool::{Lease, Upstream, UpstreamError};
use crate::relay::{OutBody, RelayBody, RequestLog, json_response};
use crate::telemetry::Telemetry;

const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long one /health answer is trusted. The check is lazy — it runs when a
/// request is routed, never on a timer — so an idle forwarder probes nothing,
/// and the probe itself runs off to the side so no request waits for it. An upstream
/// that is redeployed is usually caught before this elapses: the failed
/// requests during its downtime mark the negotiation stale.
const REPROBE_AFTER: Duration = Duration::from_secs(60);
/// A coding a 415 turned off stays off this long even if /health keeps
/// advertising it, so an upstream whose advertisement disagrees with its behaviour
/// cannot make every re-probe cost another rejected upload.
const REFUSAL_BACKOFF: Duration = Duration::from_secs(600);
/// What the upstream leg asks for; the agent always gets identity.
const ACCEPT_ENCODING: &str = "gzip, deflate, zstd";

/// Hop-by-hop (RFC 9110 7.6.1) plus what each side recomputes.
/// accept-encoding: the upstream leg negotiates its own.
/// content-encoding: the agent gets identity.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

fn drop_from_request(name: &str) -> bool {
    // content-encoding is deliberately NOT dropped: a body the agent already
    // encoded keeps its own coding on the way upstream.
    // x-dict-store: asking the upstream to keep a body is this process's call.
    HOP_BY_HOP.contains(&name)
        || matches!(
            name,
            "host" | "content-length" | "accept-encoding" | dict::STORE_HEADER
        )
}

fn drop_from_response(name: &str) -> bool {
    HOP_BY_HOP.contains(&name)
        || matches!(
            name,
            "content-length" | "content-encoding" | dict::STORED_HEADER | dict::MISS_HEADER
        )
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Coding {
    #[default]
    None = 0,
    Zstd = 1,
    Gzip = 2,
    /// zstd against a previous body (`dict`). Chosen per request on top of a
    /// negotiated zstd, so it is never what `Forwarder::coding` holds.
    Dcz = 3,
}

impl Coding {
    pub fn name(self) -> Option<&'static str> {
        match self {
            Coding::None => None,
            Coding::Zstd => Some("zstd"),
            Coding::Gzip => Some("gzip"),
            Coding::Dcz => Some("dcz"),
        }
    }

    /// The inverse of `name()`, for the `requests.coding` column the recorder
    /// writes: what a request went out as, read back out of the database.
    pub fn from_stored(name: &str) -> Coding {
        match name {
            "zstd" => Coding::Zstd,
            "gzip" => Coding::Gzip,
            "dcz" => Coding::Dcz,
            _ => Coding::None,
        }
    }

    /// Any coding by its discriminant, `Dcz` included.
    pub(crate) fn from_code(raw: u8) -> Coding {
        match raw {
            3 => Coding::Dcz,
            raw => Coding::from_u8(raw),
        }
    }

    fn from_u8(raw: u8) -> Coding {
        match raw {
            1 => Coding::Zstd,
            2 => Coding::Gzip,
            _ => Coding::None,
        }
    }
}

/// Why compression is unavailable. Backoff expiry permits a later successful
/// capability probe to restore it; it does not itself change the route's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CompressionIssue {
    NotNegotiated = 1,
    ConfiguredOff = 2,
    ProbeFailed = 3,
    NoSupportedCoding = 4,
    EncodingRefused = 5,
    DictionaryRefused = 6,
    HashMismatch = 7,
}

impl CompressionIssue {
    pub fn name(self) -> &'static str {
        match self {
            Self::NotNegotiated => "not_negotiated",
            Self::ConfiguredOff => "configured_off",
            Self::ProbeFailed => "probe_failed",
            Self::NoSupportedCoding => "no_supported_coding",
            Self::EncodingRefused => "encoding_refused",
            Self::DictionaryRefused => "dictionary_refused",
            Self::HashMismatch => "hash_mismatch",
        }
    }

    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::NotNegotiated),
            2 => Some(Self::ConfiguredOff),
            3 => Some(Self::ProbeFailed),
            4 => Some(Self::NoSupportedCoding),
            5 => Some(Self::EncodingRefused),
            6 => Some(Self::DictionaryRefused),
            7 => Some(Self::HashMismatch),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub requests: AtomicU64,
    pub encoded_requests: AtomicU64,
    /// Requests that have been counted but whose relay has not ended.
    pub in_flight: AtomicU64,
    pub body_bytes: AtomicU64,
    pub wire_bytes: AtomicU64,
    /// Response bytes as the agent got them, and as they came off the wire.
    pub down_bytes: AtomicU64,
    pub down_wire_bytes: AtomicU64,
    /// Response bytes that left for the agent encoded, and the relays that
    /// encoded them. Zero while no agent offers a coding we can make.
    pub agent_bytes: AtomicU64,
    pub responses_encoded: AtomicU64,
    pub retried_identity: AtomicU64,
    /// Requests the upstream opened with a previous body, and the ones it could
    /// not (dictionary gone: resent as plain zstd).
    pub dict_hits: AtomicU64,
    pub dict_misses: AtomicU64,
    pub dict_hash_mismatches: AtomicU64,
    pub probe_failures: AtomicU64,
    pub client_aborts: AtomicU64,
    pub upstream_errors: AtomicU64,
}

/// Holds `in_flight` up for one request, and its entry in the telemetry's
/// flight registry. Created when the request is counted, moved into the
/// `RequestLog` on success so it lives as long as the relay, and dropped on
/// the spot by every early return.
pub struct InFlight {
    stats: Arc<Stats>,
    telemetry: Arc<Telemetry>,
    flight: Arc<Flight>,
}

impl InFlight {
    pub(crate) fn new(
        stats: &Arc<Stats>,
        telemetry: &Arc<Telemetry>,
        model: &str,
        method: &Method,
        path: &str,
        body_len: usize,
    ) -> Self {
        stats.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlight {
            stats: Arc::clone(stats),
            telemetry: Arc::clone(telemetry),
            flight: telemetry
                .flights()
                .begin(model, method, path, body_len as u64),
        }
    }

    pub fn flight(&self) -> &Flight {
        &self.flight
    }

    /// Leave the registry now rather than at the drop: the relay does this
    /// before it emits the record, so no reader sees the request twice.
    pub fn end(&self) {
        self.telemetry.flights().end(self.flight.id());
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.end();
        self.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One model's counters, read together. The JSON at `/__portway/stats` and
/// the dashboard's model table render from this, so they cannot drift.
#[derive(Debug, Clone, Default)]
pub struct StatsView {
    pub requests: u64,
    pub encoded_requests: u64,
    pub in_flight: u64,
    pub body_bytes: u64,
    pub wire_bytes: u64,
    pub down_bytes: u64,
    pub down_wire_bytes: u64,
    pub agent_bytes: u64,
    pub responses_encoded: u64,
    pub retried_identity: u64,
    pub dict_hits: u64,
    pub dict_misses: u64,
    pub dict_hash_mismatches: u64,
    pub probe_failures: u64,
    pub client_aborts: u64,
    pub upstream_errors: u64,
    pub coding: Coding,
    /// Whether previous-body dictionaries are in use toward this upstream.
    pub dict: bool,
    pub identity_reason: Option<CompressionIssue>,
    /// Seconds remaining in the refusal backoff, rounded up. Zero does not
    /// imply recovery: the next successful capability probe decides that.
    pub identity_backoff_secs: u64,
    pub dict_backoff_reason: Option<CompressionIssue>,
    pub dict_backoff_secs: u64,
    /// None before a probe, or when observing historical request records.
    pub last_probe_ok: Option<bool>,
    /// Warm connections parked in this model's pool.
    pub idle_conns: usize,
}

impl StatsView {
    /// Request bytes the compression kept off the wire.
    pub fn saved_bytes(&self) -> i64 {
        self.body_bytes as i64 - self.wire_bytes as i64
    }
}

pub struct Forwarder {
    origin_auto: bool,
    origin: RwLock<Arc<OriginState>>,
    max_body_bytes: usize,
    telemetry: Arc<Telemetry>,
    probe_path: Option<String>,
    pub model: String,
    upstream: Arc<Upstream>,
    coding: AtomicU8,
    want: CodingPreference,
    want_dict: DictionaryPreference,
    /// The upstream advertised `dcz` and nothing has turned it off since.
    dict: AtomicBool,
    ring: Ring,
    level: i32,
    min_bytes: usize,
    stats: Arc<Stats>,
    /// Negotiation freshness. All the instants are millisecond offsets from
    /// `born`, so they fit in atomics and the request path stays lock-free.
    born: Instant,
    probed_at: AtomicU64,
    /// Something happened that suggests the upstream changed underneath us.
    stale: AtomicBool,
    /// One re-probe at a time, however many requests find it due.
    probing: AtomicBool,
    /// When a 415-refused coding, and dictionaries, may be trusted again.
    encoding_refused_until: AtomicU64,
    dict_refused_until: AtomicU64,
    identity_reason: AtomicU8,
    dict_backoff_reason: AtomicU8,
    /// 0: not probed, 1: succeeded, 2: failed.
    last_probe: AtomicU8,
}

/// What the upstream's /health says it takes.
#[derive(Default)]
struct Advertised {
    encodings: Vec<String>,
    dcz: bool,
}

/// One request body, ready for the wire.
struct Encoded {
    wire: Bytes,
    coding: Coding,
    /// The previous body `wire` was compressed against.
    base: Option<dict::Hash>,
    /// This body's own hash, when the upstream is asked to keep it.
    hash: Option<dict::Hash>,
}

impl Forwarder {
    pub fn from_url(name: &str, url: &str, config: &ForwarderConfig) -> Result<Arc<Self>, String> {
        config.validate()?;
        let upstream = Arc::new(Upstream::with_telemetry(
            url,
            None,
            Arc::clone(&config.telemetry),
        )?);
        Ok(Arc::new(Self::new(name, upstream, config)))
    }
    pub(crate) fn new(model: &str, upstream: Arc<Upstream>, args: &ForwarderConfig) -> Self {
        Forwarder {
            origin_auto: args.origin_compression.mode == OriginCompressionMode::Auto,
            origin: RwLock::new(Arc::default()),
            max_body_bytes: args.max_body_bytes,
            telemetry: Arc::clone(&args.telemetry),
            probe_path: args.probe_path.clone(),
            model: model.to_string(),
            upstream,
            coding: AtomicU8::new(Coding::None as u8),
            want: args.coding,
            want_dict: args.dict,
            dict: AtomicBool::new(false),
            ring: Ring::default(),
            level: args.level,
            min_bytes: args.min_bytes,
            stats: Arc::default(),
            born: Instant::now(),
            probed_at: AtomicU64::new(0),
            stale: AtomicBool::new(false),
            probing: AtomicBool::new(false),
            encoding_refused_until: AtomicU64::new(0),
            dict_refused_until: AtomicU64::new(0),
            identity_reason: AtomicU8::new(if args.coding == CodingPreference::Off {
                CompressionIssue::ConfiguredOff as u8
            } else {
                CompressionIssue::NotNegotiated as u8
            }),
            dict_backoff_reason: AtomicU8::new(0),
            last_probe: AtomicU8::new(0),
        }
    }

    /// Carry learned origin state across a reload only when its policy and destination match.
    pub(crate) fn inherit_origin_state(&self, previous: &Self) {
        if self.origin_auto
            && previous.origin_auto
            && self.upstream.base == previous.upstream.base
            && self.min_bytes == previous.min_bytes
            && self.level == previous.level
            && self.max_body_bytes == previous.max_body_bytes
        {
            let state = previous.origin.read().expect("origin state").clone();
            *self.origin.write().expect("origin state") = state;
        }
    }

    /// Milliseconds since this forwarder was built.
    fn now(&self) -> u64 {
        self.born.elapsed().as_millis() as u64
    }

    fn coding(&self) -> Coding {
        Coding::from_u8(self.coding.load(Ordering::Relaxed))
    }

    /// Pick the request coding from the upstream's /health, never by guessing.
    pub async fn negotiate(&self) {
        if self.origin_auto || self.want == CodingPreference::Off {
            return;
        }
        let probed = self.probe_health().await;
        self.note_probe(probed.is_ok());
        let advertised = match probed {
            Ok(advertised) => advertised,
            Err(err) => {
                self.telemetry
                    .warn(&format!("could not read /health.request_encodings: {err}"));
                Advertised::default()
            }
        };
        self.probed_at.store(self.now(), Ordering::Relaxed);
        self.apply(&advertised, true);
    }

    fn note_probe(&self, ok: bool) {
        self.last_probe
            .store(if ok { 1 } else { 2 }, Ordering::Relaxed);
        if !ok {
            self.stats.probe_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether the negotiation should be re-read before the next request.
    fn reprobe_due(&self) -> bool {
        !self.origin_auto
            && self.want != CodingPreference::Off
            && (self.stale.load(Ordering::Relaxed)
                || self
                    .now()
                    .saturating_sub(self.probed_at.load(Ordering::Relaxed))
                    >= REPROBE_AFTER.as_millis() as u64)
    }

    /// Re-read /health in the background when it is due. Nothing waits for it:
    /// the request that triggered it goes out under the old answer, which is
    /// safe in both directions — a capability the upstream dropped is caught by the
    /// 415/412 paths, and one it gained is simply used from the next request on.
    pub fn reprobe_if_due(self: &Arc<Self>) {
        if !self.reprobe_due() || self.probing.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            // Cleared first, so an error that lands mid-probe is not swallowed.
            this.stale.store(false, Ordering::Relaxed);
            let probed = this.probe_health().await;
            this.note_probe(probed.is_ok());
            // Recorded either way: an unreachable upstream must not be re-probed on
            // every single request.
            this.probed_at.store(this.now(), Ordering::Relaxed);
            if let Ok(advertised) = probed {
                this.apply(&advertised, false);
            }
            this.probing.store(false, Ordering::Release);
        });
    }

    /// Something suggests the upstream changed underneath us — re-read /health
    /// before the next request rather than waiting for the interval.
    fn mark_stale(&self) {
        self.stale.store(true, Ordering::Relaxed);
    }

    /// Decide the coding and dictionary state from one /health answer, and say
    /// whether anything moved. Announced on the first answer and on every
    /// change after it, so a steady upstream is silent.
    fn apply(&self, advertised: &Advertised, first: bool) -> bool {
        let accepted = &advertised.encodings;
        let order: &[&str] = match self.want {
            CodingPreference::Auto => &["zstd", "gzip"],
            CodingPreference::Zstd => &["zstd"],
            CodingPreference::Gzip => &["gzip"],
            CodingPreference::Off => &[],
        };
        let now = self.now();
        let mut coding = match order
            .iter()
            .find(|candidate| accepted.iter().any(|a| a == *candidate))
        {
            Some(&"zstd") => Coding::Zstd,
            Some(&"gzip") => Coding::Gzip,
            _ => Coding::None,
        };
        if now < self.encoding_refused_until.load(Ordering::Relaxed) {
            coding = Coding::None;
        }
        // The miss path resends as plain zstd, so dictionaries ride on zstd only.
        let dict = advertised.dcz
            && self.want_dict == DictionaryPreference::Auto
            && coding == Coding::Zstd
            && now >= self.dict_refused_until.load(Ordering::Relaxed);

        let before = (self.coding(), self.dict.load(Ordering::Relaxed));
        let reason = if coding != Coding::None {
            0
        } else if self.want == CodingPreference::Off {
            CompressionIssue::ConfiguredOff as u8
        } else if now < self.encoding_refused_until.load(Ordering::Relaxed) {
            CompressionIssue::EncodingRefused as u8
        } else if self.last_probe.load(Ordering::Relaxed) == 2 {
            CompressionIssue::ProbeFailed as u8
        } else {
            CompressionIssue::NoSupportedCoding as u8
        };
        self.identity_reason.store(reason, Ordering::Relaxed);
        if dict {
            self.dict_backoff_reason.store(0, Ordering::Relaxed);
        }
        self.coding.store(coding as u8, Ordering::Relaxed);
        self.dict.store(dict, Ordering::Relaxed);
        if !dict {
            // Bases the upstream will never be asked for again.
            self.ring.clear();
        }
        let changed = before != (coding, dict);
        if !first && !changed {
            return false;
        }
        if coding == Coding::None {
            let seen = if accepted.is_empty() {
                "no request encodings".to_string()
            } else {
                format!("[{}]", accepted.join(", "))
            };
            let want = match self.want {
                CodingPreference::Auto => "auto",
                CodingPreference::Zstd => "zstd",
                CodingPreference::Gzip => "gzip",
                CodingPreference::Off => "off",
            };
            self.telemetry.warn(&format!(
                "{}: upstream accepts {seen}, wanted {want}: request bodies go \
                 UNCOMPRESSED (connection reuse and response compression still apply)",
                self.model
            ));
        } else {
            self.telemetry.info(&format!(
                "{}: request bodies >= {} bytes -> {}{}",
                self.model,
                self.min_bytes,
                coding.name().unwrap_or("identity"),
                if dict {
                    " + previous-body dictionary (dcz)"
                } else {
                    ""
                }
            ));
        }
        changed
    }

    /// JSON capability endpoint serves JSON with a `request_encodings` field; the SGLang
    /// builds serve plain text and advertise via `X-Request-Encodings`.
    async fn probe_health(&self) -> Result<Advertised, UpstreamError> {
        let capabilities = self.probe_at(crate::router::CAPABILITIES_PATH).await;
        if let Ok(ref advertised) = capabilities
            && (!advertised.encodings.is_empty() || advertised.dcz)
        {
            return capabilities;
        }
        self.probe_at(self.probe_path.as_deref().unwrap_or("/health"))
            .await
    }
    async fn probe_at(&self, path: &str) -> Result<Advertised, UpstreamError> {
        let clock = Arc::new(PhaseClock::new());
        let request = Request::builder()
            .method(Method::GET)
            .uri(path)
            .header(http::header::HOST, &self.upstream.authority)
            .header(http::header::ACCEPT_ENCODING, "gzip, zstd")
            .body(TimedBody::empty(Arc::clone(&clock)))
            .expect("static /health request");
        let send = self.upstream.send(request, clock);
        let (response, mut lease) = tokio::time::timeout(NEGOTIATE_TIMEOUT, send)
            .await
            .map_err(|_| UpstreamError::Timeout("health"))??;
        let (parts, body) = response.into_parts();
        if !parts.status.is_success() {
            return Err(UpstreamError::Body(
                "capability endpoint returned an error".into(),
            ));
        }
        let raw = tokio::time::timeout(NEGOTIATE_TIMEOUT, collect_raw(body, 1 << 20))
            .await
            .map_err(|_| UpstreamError::Timeout("health"))?
            .map_err(|e| UpstreamError::Body(e.to_string()))?;
        let encoding = parts
            .headers
            .get(http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("identity")
            .to_owned();
        let collected =
            tokio::task::spawn_blocking(move || crate::receiver::decode_probe(&raw, &encoding))
                .await
                .map_err(|_| UpstreamError::Body("probe decoder failed".into()))?
                .map_err(|e| UpstreamError::Body(e.to_string()))?;
        lease.release();

        let dcz = parts
            .headers
            .get(dict::ADVERTISE_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|csv| csv.split(',').any(|t| t.trim() == dict::ADVERTISED_AS));
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_slice(&collected)
            && let Some(list) = map.get("request_encodings")
        {
            let encodings = match list {
                serde_json::Value::Array(items) => items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                _ => Vec::new(),
            };
            return Ok(Advertised { encodings, dcz });
        }
        // Plaintext /health (SGLang): codings come in a response header.
        let encodings = parts
            .headers
            .get("x-request-encodings")
            .and_then(|v| v.to_str().ok())
            .map(|csv| {
                csv.split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Advertised { encodings, dcz })
    }

    fn encode(&self, body: &Bytes, coding: Coding) -> std::io::Result<Bytes> {
        match coding {
            Coding::Zstd => compress_zstd(body, self.level).map(Bytes::from),
            Coding::Gzip => {
                use std::io::Write;
                let level = flate2::Compression::new(self.level.min(9) as u32);
                let mut encoder = flate2::write::GzEncoder::new(Vec::new(), level);
                encoder.write_all(body)?;
                encoder.finish().map(Bytes::from)
            }
            // Needs a base: `encode_turn` builds these.
            Coding::None | Coding::Dcz => Ok(body.clone()),
        }
    }

    /// `coding` and `dict` are what was negotiated when the request arrived; the
    /// turn goes out as `dcz` instead when the upstream holds a body to compress
    /// it against.
    fn encode_turn(
        &self,
        body: &Bytes,
        coding: Coding,
        dict: bool,
        scope: &dict::DictionaryScope,
    ) -> std::io::Result<Encoded> {
        if coding == Coding::Zstd && dict {
            let hash = Some(dict::sha256(body));
            if let Some(base) = self.ring.pick_scoped(body, scope) {
                return Ok(Encoded {
                    wire: dict::compress(body, &base, self.level)?.into(),
                    coding: Coding::Dcz,
                    base: Some(base.hash),
                    hash,
                });
            }
            return Ok(Encoded {
                wire: self.encode(body, coding)?,
                coding,
                base: None,
                hash,
            });
        }
        Ok(Encoded {
            wire: self.encode(body, coding)?,
            coding,
            base: None,
            hash: None,
        })
    }

    /// Stop using dictionaries toward this upstream. A later /health may turn them
    /// back on, but not before the refusal backoff is over.
    fn dict_off(&self, reason: CompressionIssue) {
        self.dict_backoff_reason
            .store(reason as u8, Ordering::Relaxed);
        self.dict.store(false, Ordering::Relaxed);
        self.ring.clear();
        self.dict_refused_until.store(
            self.now() + REFUSAL_BACKOFF.as_millis() as u64,
            Ordering::Relaxed,
        );
        self.mark_stale();
    }

    fn upstream_headers(&self, client: &HeaderMap) -> HeaderMap {
        let mut headers = HeaderMap::with_capacity(client.len() + 3);
        for (name, value) in client {
            if !drop_from_request(name.as_str()) && !connection_header(client, name.as_str()) {
                headers.append(name.clone(), value.clone());
            }
        }
        // Everything else passes through untouched, including User-Agent — if
        // an upstream rejects a library default agent, that is the
        // client's UA to fix, not ours to rewrite.
        headers.insert(
            http::header::HOST,
            HeaderValue::from_str(&self.upstream.authority).expect("authority is a valid header"),
        );
        if no_transform(client) || client.contains_key(http::header::RANGE) {
            if let Some(value) = client.get(http::header::ACCEPT_ENCODING) {
                headers.insert(http::header::ACCEPT_ENCODING, value.clone());
            }
        } else {
            headers.insert(
                http::header::ACCEPT_ENCODING,
                HeaderValue::from_static(ACCEPT_ENCODING),
            );
        }
        headers
    }

    async fn send(
        &self,
        method: &Method,
        path_and_query: &str,
        headers: &HeaderMap,
        wire: Bytes,
        meter: Option<Arc<UploadMeter>>,
        attempt: (&InFlight, Coding),
    ) -> Result<(Response<Incoming>, Lease, Arc<PhaseClock>), UpstreamError> {
        let clock = Arc::new(PhaseClock::new());
        attempt.0.flight().attempt(&clock, wire.len(), attempt.1);
        let mut builder = Request::builder()
            .method(method)
            .uri(format!("{}{path_and_query}", self.upstream.base_path));
        {
            let slot = builder.headers_mut().expect("builder has headers");
            *slot = headers.clone();
            if !wire.is_empty() {
                // An explicit Content-Length keeps the wire byte-identical to a
                // buffered body; hyper honors the header over the body's hint.
                slot.insert(
                    http::header::CONTENT_LENGTH,
                    HeaderValue::from(wire.len() as u64),
                );
            }
        }
        if let Some(meter) = &meter {
            meter.attempt(headers);
        }
        let request = builder
            .body(TimedBody::new(wire, Arc::clone(&clock)).metered(meter))
            .map_err(|e| UpstreamError::Resolve(e.to_string()))?;
        let (response, lease) = self.upstream.send(request, Arc::clone(&clock)).await?;
        Ok((response, lease, clock))
    }

    /// Forward an already-buffered request without binding a listener.
    pub async fn forward(self: &Arc<Self>, request: Request<Bytes>) -> Response<OutBody> {
        let (parts, body) = request.into_parts();
        if parts.method == Method::CONNECT || parts.headers.contains_key(http::header::UPGRADE) {
            return json_response(
                StatusCode::NOT_IMPLEMENTED,
                serde_json::json!({"error":"tunnels and protocol upgrades are not supported"}),
            );
        }
        let path = parts.uri.path().to_owned();
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_owned();
        let scope = parts
            .extensions
            .get::<dict::DictionaryScope>()
            .cloned()
            .unwrap_or_else(|| dict::DictionaryScope::from_headers(&parts.headers));
        self.handle_scoped(
            parts.method,
            path_and_query,
            path,
            parts.headers,
            body,
            scope,
        )
        .await
    }

    pub async fn handle(
        self: &Arc<Self>,
        method: Method,
        path_and_query: String,
        path: String,
        client_headers: HeaderMap,
        body: Bytes,
    ) -> Response<OutBody> {
        let scope = dict::DictionaryScope::from_headers(&client_headers);
        self.handle_scoped(method, path_and_query, path, client_headers, body, scope)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_scoped(
        self: &Arc<Self>,
        method: Method,
        path_and_query: String,
        path: String,
        client_headers: HeaderMap,
        body: Bytes,
        scope: dict::DictionaryScope,
    ) -> Response<OutBody> {
        if body.len() > self.max_body_bytes {
            return json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                serde_json::json!({"error":"request body exceeds configured limit"}),
            );
        }
        let t0 = Instant::now();
        self.reprobe_if_due();
        let mut headers = self.upstream_headers(&client_headers);
        let pre_encoded = client_headers.contains_key(http::header::CONTENT_ENCODING);

        let origin = self
            .origin_auto
            .then(|| self.origin.read().expect("origin state").clone());
        let meter = origin.as_ref().map(|state| state.meter());
        let eligible = !body.is_empty()
            && body.len() >= self.min_bytes
            && !pre_encoded
            && !no_transform(&client_headers)
            && (!self.origin_auto
                || (!matches!(method, Method::GET | Method::HEAD)
                    && ![
                        "content-md5",
                        "digest",
                        "content-digest",
                        "signature",
                        "signature-input",
                    ]
                    .iter()
                    .any(|name| client_headers.contains_key(*name))));
        let origin_attempt = origin
            .as_ref()
            .map(|state| state.begin(&method, &path_and_query, &client_headers, &scope, eligible));
        let selected = origin_attempt
            .as_ref()
            .map_or_else(|| self.coding(), |attempt| attempt.coding);
        // Read with the coding, before anything awaits: a re-probe applied while
        // this turn is encoding must not change what it asks the upstream to keep.
        let dict = self.dict.load(Ordering::Relaxed);

        // Any content-encoding the agent set means it framed the body itself;
        // the bytes and the header both pass through untouched.
        let mut coding = Coding::None;
        let mut wire = body.clone();
        let mut base = None;
        let mut hash = None;
        if selected != Coding::None && eligible {
            let negotiated = selected;
            let source = body.clone();
            let this = Arc::clone(self);
            let encoding_scope = scope.clone();
            let job = tokio::task::spawn_blocking(move || {
                if this.origin_auto {
                    this.encode(&source, negotiated).map(|wire| Encoded {
                        wire,
                        coding: negotiated,
                        base: None,
                        hash: None,
                    })
                } else {
                    this.encode_turn(&source, negotiated, dict, &encoding_scope)
                }
            });
            if let Ok(Ok(encoded)) = job.await
                && encoded.wire.len() < body.len()
            {
                (wire, coding, base, hash) =
                    (encoded.wire, encoded.coding, encoded.base, encoded.hash);
                headers.insert(
                    http::header::CONTENT_ENCODING,
                    HeaderValue::from_static(coding.name().expect("coding is set")),
                );
                if hash.is_some() {
                    headers.insert(dict::STORE_HEADER, HeaderValue::from_static("1"));
                }
            }
        }
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let in_flight = InFlight::new(
            &self.stats,
            &self.telemetry,
            &self.model,
            &method,
            &path,
            body.len(),
        );
        let abort = AbortGuard {
            forwarder: Arc::clone(self),
            method: method.clone(),
            path: path.clone(),
            armed: true,
        };

        let mut sent = self
            .send(
                &method,
                &path_and_query,
                &headers,
                wire.clone(),
                meter.clone(),
                (&in_flight, coding),
            )
            .await;
        if let Some(attempt) = &origin_attempt
            && let Ok((response, _, _)) = &sent
        {
            attempt.observe(response.status(), response.headers(), coding);
            if coding != Coding::None && matches!(response.status().as_u16(), 400 | 415) {
                self.telemetry.warn(&format!(
                    "origin {}: {} rejected {}; compression suspended for 600s",
                    self.model,
                    response.status().as_u16(),
                    coding.name().unwrap_or("identity")
                ));
            }
            if coding != Coding::None
                && response.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE
                && crate::origin::identity_allowed(response.headers())
            {
                // Close the unread refusal connection before dialing the identity retry.
                drop(sent);
                self.telemetry.warn("origin 415: retrying identity once");
                self.stats.retried_identity.fetch_add(1, Ordering::Relaxed);
                meter.as_ref().expect("origin meter").retry();
                coding = Coding::None;
                wire = body.clone();
                headers.remove(http::header::CONTENT_ENCODING);
                sent = self
                    .send(
                        &method,
                        &path_and_query,
                        &headers,
                        wire.clone(),
                        meter.clone(),
                        (&in_flight, coding),
                    )
                    .await;
            }
        }
        // A trial ends at response headers; it must not remain reserved for a long SSE stream.
        drop(origin_attempt);
        if let Ok((response, _, _)) = &mut sent
            && coding == Coding::Dcz
            && let Some(reason) = dict_refusal(response)
        {
            // Either way the turn still goes out compressed: plain zstd.
            // The refusal body on this connection is still unread, so the
            // connection must NOT go back to the pool: hyper will not write
            // the next request until the previous response body is consumed,
            // and the old response is only dropped when `sent` is reassigned
            // below — which never happens while the resend itself waits on
            // that connection (deadlock, seen live after every upstream recreate:
            // the agent hung at 'Working...' with in_flight 1 and no error).
            // Leave the lease alone instead: it drops with the old `sent`
            // tuple, the connection that owes bytes is closed, and the
            // resend dials a fresh one.
            match reason {
                DictRefusal::Miss | DictRefusal::Rejected => {
                    // Miss: upstream restart or eviction, costs this one round trip.
                    // Rejected should not happen; if the 400 was about the
                    // request itself, the zstd resend earns the same 400.
                    if matches!(reason, DictRefusal::Rejected) {
                        self.telemetry
                            .warn("400 for dcz: resending zstd without that dictionary");
                    }
                    self.stats.dict_misses.fetch_add(1, Ordering::Relaxed);
                    if let Some(gone) = &base {
                        self.ring.forget(gone);
                    }
                }
                DictRefusal::Unsupported => {
                    // The upstream behind the URL no longer takes dcz (rollback).
                    self.telemetry
                        .warn("415 for dcz: resending zstd, dictionaries OFF");
                    self.dict_off(CompressionIssue::DictionaryRefused);
                    hash = None;
                    headers.remove(dict::STORE_HEADER);
                }
            }
            coding = Coding::Zstd;
            let source = body.clone();
            let this = Arc::clone(self);
            let job = tokio::task::spawn_blocking(move || this.encode(&source, Coding::Zstd));
            match job.await {
                Ok(Ok(encoded)) if encoded.len() < body.len() => {
                    wire = encoded;
                    headers.insert(
                        http::header::CONTENT_ENCODING,
                        HeaderValue::from_static("zstd"),
                    );
                }
                _ => {
                    coding = Coding::None;
                    wire = body.clone();
                    headers.remove(http::header::CONTENT_ENCODING);
                    headers.remove(dict::STORE_HEADER);
                    hash = None;
                }
            }
            sent = self
                .send(
                    &method,
                    &path_and_query,
                    &headers,
                    wire.clone(),
                    meter.clone(),
                    (&in_flight, coding),
                )
                .await;
        }
        if let Ok((response, _, _)) = &sent
            && matches!(response.status().as_u16(), 502..=504)
        {
            // The upstream hop ends at the edge, so an upstream that is being
            // replaced does not break this connection — it shows up as the
            // edge's own 5xx. That is the redeploy signal on this path.
            self.mark_stale();
        }
        if let Ok((response, _, _)) = &mut sent
            && !self.origin_auto
            && response.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE
            && response.headers().contains_key("x-portway-decode-error")
            && coding != Coding::None
        {
            // The upstream behind the URL no longer inflates (rollback / other
            // image). Same pool rule as the dictionary refusal above: the
            // 415 body is unread, so drop the lease with the old `sent`
            // tuple instead of pooling the connection the identity resend
            // would immediately check back out.
            self.telemetry.warn(&format!(
                "415 for {}: resending identity, encoding OFF",
                coding.name().unwrap_or("identity")
            ));
            self.coding.store(Coding::None as u8, Ordering::Relaxed);
            self.identity_reason
                .store(CompressionIssue::EncodingRefused as u8, Ordering::Relaxed);
            self.encoding_refused_until.store(
                self.now() + REFUSAL_BACKOFF.as_millis() as u64,
                Ordering::Relaxed,
            );
            self.dict_off(CompressionIssue::EncodingRefused);
            self.mark_stale();
            self.stats.retried_identity.fetch_add(1, Ordering::Relaxed);
            coding = Coding::None;
            wire = body.clone();
            hash = None;
            headers.remove(http::header::CONTENT_ENCODING);
            headers.remove(dict::STORE_HEADER);
            sent = self
                .send(
                    &method,
                    &path_and_query,
                    &headers,
                    wire.clone(),
                    meter.clone(),
                    (&in_flight, coding),
                )
                .await;
        }

        let (response, lease, clock) = match sent {
            Ok(done) => done,
            Err(err) => {
                abort.disarm();
                self.stats.upstream_errors.fetch_add(1, Ordering::Relaxed);
                // An upstream that stopped answering is the one most likely to come
                // back as a different image.
                self.mark_stale();
                self.telemetry
                    .error(&format!("{method} {path} -> upstream error {err}"));
                return json_response(
                    StatusCode::BAD_GATEWAY,
                    serde_json::json!({
                        "detail": format!("portway: upstream request failed: {err}")
                    }),
                );
            }
        };
        abort.disarm();

        if coding != Coding::None {
            self.stats.encoded_requests.fetch_add(1, Ordering::Relaxed);
        }
        if coding == Coding::Dcz {
            self.stats.dict_hits.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(hash) = hash {
            self.note_stored(response.headers(), hash, &body, scope);
        }
        let uploaded = meter
            .as_ref()
            .map_or(wire.len() as u64, |meter| meter.bytes());
        self.stats
            .body_bytes
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        self.stats.wire_bytes.fetch_add(uploaded, Ordering::Relaxed);
        let ttfb = t0.elapsed().as_secs_f64();
        in_flight
            .flight()
            .responded(response.status().as_u16(), ttfb, uploaded, coding);

        let (parts, incoming) = response.into_parts();
        let upstream_encoding = parts
            .headers
            .get(http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("identity")
            .to_string();
        let no_body = method == Method::HEAD
            || parts.status == StatusCode::NO_CONTENT
            || parts.status == StatusCode::NOT_MODIFIED;
        let preserve =
            no_body || parts.status == StatusCode::PARTIAL_CONTENT || no_transform(&parts.headers);
        let decoder = if preserve {
            None
        } else {
            Decoder::for_encoding(&upstream_encoding)
        };
        // The agent leg is identity unless the agent itself offered a coding we
        // can make. An upstream coding we could not decode is relayed untouched,
        // so there is nothing to re-encode in that case.
        let encoder = if decoder.is_some() {
            Encoder::for_accept(
                client_headers
                    .get(http::header::ACCEPT_ENCODING)
                    .and_then(|value| value.to_str().ok()),
            )
        } else {
            None
        };

        let transformed = decoder.as_ref().is_some_and(|d| !d.is_identity()) || encoder.is_some();
        let mut out = Response::builder().status(parts.status);
        {
            let slot = out.headers_mut().expect("builder has headers");
            for (name, value) in &parts.headers {
                if transformed
                    && matches!(
                        name.as_str(),
                        "etag" | "content-md5" | "digest" | "content-digest" | "repr-digest"
                    )
                {
                    continue;
                }
                if !drop_from_response(name.as_str())
                    && !connection_header(&parts.headers, name.as_str())
                {
                    slot.append(name.clone(), value.clone());
                }
            }
            if (method == Method::HEAD || parts.status == StatusCode::NOT_MODIFIED)
                && let Some(length) = parts.headers.get(http::header::CONTENT_LENGTH)
            {
                slot.insert(http::header::CONTENT_LENGTH, length.clone());
            }
            if !preserve {
                let varies = parts
                    .headers
                    .get_all(http::header::VARY)
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .flat_map(|v| v.split(','))
                    .any(|v| v.trim().eq_ignore_ascii_case("accept-encoding") || v.trim() == "*");
                if !varies {
                    slot.append(
                        http::header::VARY,
                        HeaderValue::from_static("Accept-Encoding"),
                    );
                }
            }
            match &encoder {
                Some(encoder) => {
                    slot.insert(
                        http::header::CONTENT_ENCODING,
                        HeaderValue::from_static(encoder.name()),
                    );
                }
                // A coding we cannot decode is relayed untouched rather than
                // silently corrupted; we only ever ask for gzip/deflate/zstd.
                None if decoder.is_none() => {
                    slot.insert(
                        http::header::CONTENT_ENCODING,
                        HeaderValue::from_str(&upstream_encoding)
                            .expect("upstream header round-trips"),
                    );
                }
                None => {}
            }
        }
        if encoder.is_some() {
            self.stats.responses_encoded.fetch_add(1, Ordering::Relaxed);
        }

        let log = RequestLog::new(
            self.model.clone(),
            method,
            path,
            parts.status.as_u16(),
            clock,
            body.len() as u64,
            uploaded,
            coding,
            ttfb,
            upstream_encoding,
            encoder.as_ref().map(Encoder::name),
            Arc::clone(&self.stats),
            in_flight,
            Arc::clone(&self.telemetry),
        );
        out.body(OutBody::Relay(RelayBody::new(
            incoming,
            decoder.unwrap_or_else(Decoder::identity),
            encoder,
            lease,
            log,
        )))
        .expect("relay response is well formed")
    }

    /// A body becomes a base only once the upstream says it holds exactly these
    /// bytes. A different hash means the two ends disagree on what was sent,
    /// and a dictionary built on that would be garbage.
    fn note_stored(
        &self,
        response: &HeaderMap,
        hash: dict::Hash,
        body: &Bytes,
        scope: dict::DictionaryScope,
    ) {
        let Some(stored) = response
            .get(dict::STORED_HEADER)
            .and_then(|v| v.to_str().ok())
        else {
            return;
        };
        if stored.trim().eq_ignore_ascii_case(&dict::hex(&hash)) {
            self.ring.confirm_scoped(hash, body.clone(), scope);
        } else {
            self.stats
                .dict_hash_mismatches
                .fetch_add(1, Ordering::Relaxed);
            if self.dict.load(Ordering::Relaxed) {
                self.telemetry.warn(&format!(
                    "{}: the upstream stored a different body than was sent: dictionaries OFF",
                    self.model
                ));
                self.dict_off(CompressionIssue::HashMismatch);
            }
        }
    }

    pub fn view(&self) -> StatsView {
        let now = self.now();
        let remaining = |until: &AtomicU64| {
            until
                .load(Ordering::Relaxed)
                .saturating_sub(now)
                .div_ceil(1000)
        };
        StatsView {
            requests: self.stats.requests.load(Ordering::Relaxed),
            encoded_requests: self.stats.encoded_requests.load(Ordering::Relaxed),
            in_flight: self.stats.in_flight.load(Ordering::Relaxed),
            body_bytes: self.stats.body_bytes.load(Ordering::Relaxed),
            wire_bytes: self.stats.wire_bytes.load(Ordering::Relaxed),
            down_bytes: self.stats.down_bytes.load(Ordering::Relaxed),
            down_wire_bytes: self.stats.down_wire_bytes.load(Ordering::Relaxed),
            agent_bytes: self.stats.agent_bytes.load(Ordering::Relaxed),
            responses_encoded: self.stats.responses_encoded.load(Ordering::Relaxed),
            retried_identity: self.stats.retried_identity.load(Ordering::Relaxed),
            dict_hits: self.stats.dict_hits.load(Ordering::Relaxed),
            dict_misses: self.stats.dict_misses.load(Ordering::Relaxed),
            dict_hash_mismatches: self.stats.dict_hash_mismatches.load(Ordering::Relaxed),
            probe_failures: self.stats.probe_failures.load(Ordering::Relaxed),
            client_aborts: self.stats.client_aborts.load(Ordering::Relaxed),
            upstream_errors: self.stats.upstream_errors.load(Ordering::Relaxed),
            coding: self.coding(),
            dict: self.dict.load(Ordering::Relaxed),
            identity_reason: if self.origin_auto {
                None
            } else {
                CompressionIssue::from_u8(self.identity_reason.load(Ordering::Relaxed))
            },
            identity_backoff_secs: remaining(&self.encoding_refused_until),
            dict_backoff_reason: CompressionIssue::from_u8(
                self.dict_backoff_reason.load(Ordering::Relaxed),
            ),
            dict_backoff_secs: remaining(&self.dict_refused_until),
            last_probe_ok: match self.last_probe.load(Ordering::Relaxed) {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            },
            idle_conns: self.upstream.idle_count(),
        }
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let view = self.view();
        serde_json::json!({
            "origin_compression": if self.origin_auto { self.origin.read().expect("origin state").snapshot() } else { serde_json::json!({"mode":"off"}) },
            "requests": view.requests,
            "encoded_requests": view.encoded_requests,
            "in_flight": view.in_flight,
            "body_bytes": view.body_bytes,
            "wire_bytes": view.wire_bytes,
            "down_bytes": view.down_bytes,
            "down_wire_bytes": view.down_wire_bytes,
            "agent_bytes": view.agent_bytes,
            "responses_encoded": view.responses_encoded,
            "retried_identity": view.retried_identity,
            "dict": view.dict,
            "dict_hits": view.dict_hits,
            "dict_misses": view.dict_misses,
            "dict_hash_mismatches": view.dict_hash_mismatches,
            "probe_failures": view.probe_failures,
            "identity_reason": view.identity_reason.map(CompressionIssue::name),
            "identity_backoff_secs": view.identity_backoff_secs,
            "dict_backoff_reason": view.dict_backoff_reason.map(CompressionIssue::name),
            "dict_backoff_secs": view.dict_backoff_secs,
            "last_probe_ok": view.last_probe_ok,
            "client_aborts": view.client_aborts,
            "upstream_errors": view.upstream_errors,
            "coding": view.coding.name(),
            "saved_bytes": view.saved_bytes(),
        })
    }
}

enum DictRefusal {
    /// 412 + `X-Dict-Miss`: the upstream does not hold the named body.
    Miss,
    /// 400: the upstream held the body but could not open the frame with it.
    Rejected,
    Unsupported,
}

fn dict_refusal(response: &Response<Incoming>) -> Option<DictRefusal> {
    match response.status() {
        StatusCode::PRECONDITION_FAILED if response.headers().contains_key(dict::MISS_HEADER) => {
            Some(DictRefusal::Miss)
        }
        StatusCode::BAD_REQUEST if response.headers().contains_key("x-portway-decode-error") => {
            Some(DictRefusal::Rejected)
        }
        StatusCode::UNSUPPORTED_MEDIA_TYPE
            if response.headers().contains_key("x-portway-decode-error") =>
        {
            Some(DictRefusal::Unsupported)
        }
        _ => None,
    }
}

fn compress_zstd(body: &[u8], level: i32) -> std::io::Result<Vec<u8>> {
    use std::cell::RefCell;
    // Reusing the context keeps zstd's multi-megabyte working state off the
    // allocator on every turn.
    thread_local! {
        static CCTX: RefCell<Option<(i32, zstd::bulk::Compressor<'static>)>> =
            const { RefCell::new(None) };
    }
    CCTX.with(|cell| {
        let mut held = cell.borrow_mut();
        let reusable = matches!(held.as_ref(), Some((cached, _)) if *cached == level);
        if !reusable {
            *held = Some((level, zstd::bulk::Compressor::new(level)?));
        }
        let (_, compressor) = held.as_mut().expect("just populated");
        compressor.compress(body)
    })
}

/// Counts an agent that hung up before the response head arrived. The 499 the
/// Python forwarder returned had no one left to read it.
struct AbortGuard {
    forwarder: Arc<Forwarder>,
    method: Method,
    path: String,
    armed: bool,
}

impl AbortGuard {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for AbortGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.forwarder
            .stats
            .client_aborts
            .fetch_add(1, Ordering::Relaxed);
        self.forwarder.telemetry.info(&format!(
            "{} {} -> agent left before the first byte",
            self.method, self.path
        ));
    }
}

fn connection_header(headers: &HeaderMap, name: &str) -> bool {
    headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case(name))
}

fn no_transform(headers: &HeaderMap) -> bool {
    headers
        .get_all(http::header::CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case("no-transform"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forwarder(args: &ForwarderConfig) -> Forwarder {
        let upstream = Upstream::new("http://127.0.0.1:1", None).expect("a parseable base");
        Forwarder::new("test-model", Arc::new(upstream), args)
    }

    fn advertised(encodings: &[&str], dcz: bool) -> Advertised {
        Advertised {
            encodings: encodings.iter().map(|e| (*e).to_string()).collect(),
            dcz,
        }
    }

    /// Pretends `millis` have passed since the last /health answer, by moving
    /// the forwarder's own origin back rather than waiting.
    fn backdate(fwd: &mut Forwarder, millis: u64) {
        fwd.born = fwd
            .born
            .checked_sub(Duration::from_millis(millis))
            .expect("the process has been running long enough to age a probe");
    }

    #[test]
    fn a_fresh_answer_is_not_re_read_until_it_ages_or_something_breaks() {
        let mut fwd = forwarder(&ForwarderConfig::default());
        fwd.probed_at.store(fwd.now(), Ordering::Relaxed);
        assert!(!fwd.reprobe_due());

        // A failed request cannot wait for the interval: the upstream it could not
        // reach is the one most likely to come back as a different image.
        fwd.mark_stale();
        assert!(fwd.reprobe_due());

        fwd.stale.store(false, Ordering::Relaxed);
        assert!(!fwd.reprobe_due());
        backdate(&mut fwd, REPROBE_AFTER.as_millis() as u64 + 1);
        assert!(fwd.reprobe_due());
    }

    #[test]
    fn nothing_is_re_read_when_the_operator_turned_compression_off() {
        let args = ForwarderConfig {
            coding: CodingPreference::Off,
            ..ForwarderConfig::default()
        };
        let mut fwd = forwarder(&args);
        fwd.mark_stale();
        backdate(&mut fwd, REPROBE_AFTER.as_millis() as u64 * 10);
        assert!(!fwd.reprobe_due());
    }

    #[test]
    fn a_later_health_answer_turns_dictionaries_on_and_off() {
        let fwd = forwarder(&ForwarderConfig::default());
        assert!(fwd.apply(&advertised(&["zstd", "gzip"], false), true));
        assert_eq!(fwd.coding(), Coding::Zstd);
        assert!(!fwd.dict.load(Ordering::Relaxed));

        // The same answer again is not a change, so it is not announced.
        assert!(!fwd.apply(&advertised(&["zstd", "gzip"], false), false));

        assert!(fwd.apply(&advertised(&["zstd", "gzip"], true), false));
        assert!(fwd.dict.load(Ordering::Relaxed));
        assert!(fwd.apply(&advertised(&["zstd", "gzip"], false), false));
        assert!(!fwd.dict.load(Ordering::Relaxed));

        // gzip only: dictionaries ride on zstd, so they stay off.
        assert!(fwd.apply(&advertised(&["gzip"], true), false));
        assert_eq!(fwd.coding(), Coding::Gzip);
        assert!(!fwd.dict.load(Ordering::Relaxed));
    }

    #[test]
    fn a_415_holds_a_coding_down_even_while_health_still_advertises_it() {
        let mut fwd = forwarder(&ForwarderConfig::default());
        fwd.apply(&advertised(&["zstd"], true), true);
        assert_eq!(fwd.coding(), Coding::Zstd);

        // What the 415 path does: off, and not to be believed again yet.
        fwd.coding.store(Coding::None as u8, Ordering::Relaxed);
        fwd.encoding_refused_until.store(
            fwd.now() + REFUSAL_BACKOFF.as_millis() as u64,
            Ordering::Relaxed,
        );
        fwd.dict_off(CompressionIssue::EncodingRefused);
        fwd.apply(&advertised(&["zstd"], true), false);
        assert_eq!(fwd.coding(), Coding::None, "advertisement does not win yet");
        assert!(!fwd.dict.load(Ordering::Relaxed));
        assert_eq!(fwd.snapshot()["identity_reason"], "encoding_refused");
        assert_eq!(fwd.view().identity_backoff_secs, 600);

        // Time passing alone does not recover compression or erase the cause.
        backdate(&mut fwd, REFUSAL_BACKOFF.as_millis() as u64 + 1);
        assert_eq!(fwd.view().identity_backoff_secs, 0);
        assert_eq!(fwd.snapshot()["identity_reason"], "encoding_refused");
        assert_eq!(fwd.coding(), Coding::None);
        assert!(fwd.apply(&advertised(&["zstd"], true), false));
        assert_eq!(fwd.coding(), Coding::Zstd);
        assert!(fwd.dict.load(Ordering::Relaxed));
        assert_eq!(fwd.view().identity_reason, None);
        assert_eq!(fwd.view().dict_backoff_reason, None);
    }

    #[tokio::test]
    async fn diagnostics_distinguish_disabled_failed_and_unsupported_probes() {
        let disabled = forwarder(&ForwarderConfig {
            coding: CodingPreference::Off,
            ..ForwarderConfig::default()
        });
        disabled.negotiate().await;
        assert_eq!(disabled.snapshot()["identity_reason"], "configured_off");
        assert_eq!(disabled.view().last_probe_ok, None);
        assert_eq!(disabled.view().probe_failures, 0);

        let fwd = forwarder(&ForwarderConfig::default());
        assert_eq!(fwd.snapshot()["identity_reason"], "not_negotiated");
        fwd.negotiate().await;
        assert_eq!(fwd.snapshot()["identity_reason"], "probe_failed");
        assert_eq!(fwd.view().last_probe_ok, Some(false));
        assert_eq!(fwd.view().probe_failures, 1);

        fwd.note_probe(true);
        fwd.apply(&advertised(&[], false), false);
        assert_eq!(fwd.snapshot()["identity_reason"], "no_supported_coding");
        assert_eq!(fwd.view().last_probe_ok, Some(true));
        assert_eq!(fwd.view().probe_failures, 1);
    }

    #[test]
    fn hash_mismatch_diagnostics_survive_expiry_and_count_recurrences() {
        let mut fwd = forwarder(&ForwarderConfig::default());
        let body = Bytes::from_static(b"a confirmed request body");
        let mut response = HeaderMap::new();
        response.insert(dict::STORED_HEADER, HeaderValue::from_static("wrong hash"));
        fwd.apply(&advertised(&["zstd"], true), true);
        for count in 1..=2 {
            fwd.note_stored(&response, dict::sha256(&body), &body, Default::default());
            assert_eq!(fwd.view().dict_hash_mismatches, count);
            assert_eq!(fwd.coding(), Coding::Zstd);
            assert_eq!(fwd.view().identity_reason, None);
            assert_eq!(fwd.snapshot()["dict_backoff_reason"], "hash_mismatch");
            assert_eq!(fwd.view().dict_backoff_secs, 600);
            backdate(&mut fwd, REFUSAL_BACKOFF.as_millis() as u64 + 1);
            assert_eq!(fwd.view().dict_backoff_secs, 0);
            assert_eq!(fwd.snapshot()["dict_backoff_reason"], "hash_mismatch");
            fwd.apply(&advertised(&["zstd"], true), false);
            assert!(fwd.view().dict);
            assert_eq!(fwd.view().dict_backoff_reason, None);
        }
    }

    #[test]
    fn an_unreachable_upstream_keeps_its_last_known_answer() {
        let fwd = forwarder(&ForwarderConfig::default());
        fwd.apply(&advertised(&["zstd"], true), true);
        // reprobe() only calls apply() on a successful read; the probe against
        // this dead base fails, so the negotiation must survive it.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a test runtime");
        let fwd = Arc::new(fwd);
        runtime.block_on(async {
            fwd.mark_stale();
            fwd.reprobe_if_due();
            for _ in 0..600 {
                if !fwd.probing.load(Ordering::Relaxed) && !fwd.reprobe_due() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        assert_eq!(fwd.coding(), Coding::Zstd);
        assert!(fwd.dict.load(Ordering::Relaxed));
        assert_eq!(fwd.view().identity_reason, None);
        assert_eq!(fwd.view().last_probe_ok, Some(false));
        assert_eq!(fwd.view().probe_failures, 1);
    }
}
