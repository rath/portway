//! Loopback harness: a scriptable stand-in for an upstream, and a tiny client for
//! the forwarder itself.
//!
//! Unlike the Python suite's in-process transport mock, both ends here are
//! real HTTP/1.1 over TCP, so the connection pool, the Content-Length framing
//! and the abort path are exercised rather than simulated.

#![allow(dead_code)]

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::Write;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use portway::cli::Args;
use portway::router::Router;
use tokio::net::TcpListener;

pub type MockBody = BoxBody<Bytes, Infallible>;

fn boxed(bytes: impl Into<Bytes>) -> MockBody {
    Full::new(bytes.into()).boxed()
}

/// What the upstream advertises for compression discovery.
#[derive(Clone)]
pub enum Health {
    /// A Portway receiver: capabilities live on the reserved management path.
    Portway(Vec<&'static str>),
    /// JSON capability endpoint: JSON with a `request_encodings` field.
    Json(Vec<&'static str>),
    /// JSON capability endpoint behind the edge: the same JSON, gzip-compressed.
    GzipJson(Vec<&'static str>),
    /// JSON without the field at all.
    JsonBare,
    /// An SGLang build: plain text plus `X-Request-Encodings`.
    Plaintext(Option<Vec<&'static str>>),
    /// The same build with previous-body dictionaries: `X-Request-Dictionary`.
    PlaintextDict(Vec<&'static str>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Ok,
    /// Reject any request that arrives with a Content-Encoding.
    RejectEncoded,
    /// gzip-compressed SSE, as the edge delivers it.
    GzipStream,
    /// zstd SSE split across chunks with an idle gap between them.
    ZstdChunks,
    /// Stream until the peer goes away, recording when that happened.
    Endless,
    /// An upstream with a dictionary store: inflates zstd and dcz, keeps what
    /// `X-Dict-Store` asks it to, 412s a dictionary it does not hold.
    Dict,
    /// Acknowledges every store but never finds the dictionary again, like a
    /// upstream that restarts between turns.
    DictAmnesia,
    /// The same amnesia, but the 412 body is written in its own segment,
    /// after a pause — the way TLS record boundaries at the edge split a
    /// small refusal body from its response head.
    DictAmnesiaSplit,
    /// Advertised dcz at /health, then got rolled back: 415 on dcz, zstd fine.
    DictRolledBack,
    /// Acknowledges a store with the hash of some other body.
    DictWrongHash,
    /// Holds the dictionary but answers 400 to every dcz request.
    DictRejects,
    /// An upstream being replaced: the edge answers 503 until `UpstreamMock::came_back` is
    /// called, after which it behaves like `Dict`.
    Restarting,
    /// A buffered answer carrying the engine's usage object.
    UsageJson,
    /// An SSE answer that ends with the no-choices chunk that carries it, the
    /// way `stream_options.include_usage` is answered.
    UsageStream,
}

/// The counts both usage replies report, so a test can expect one set of
/// numbers whatever shape they arrived in.
pub const USAGE_PROMPT: u64 = 18_234;
pub const USAGE_CACHED: u64 = 18_200;
pub const USAGE_COMPLETION: u64 = 891;
pub const USAGE_REASONING: u64 = 742;

/// The buffered reply carries every number, in vLLM's layout: the cache detail
/// under `prompt_tokens_details`, the thinking under
/// `completion_tokens_details`.
fn usage_json() -> serde_json::Value {
    serde_json::json!({
        "prompt_tokens": USAGE_PROMPT,
        "prompt_tokens_details": {"cached_tokens": USAGE_CACHED},
        "completion_tokens": USAGE_COMPLETION,
        "completion_tokens_details": {"reasoning_tokens": USAGE_REASONING},
        "total_tokens": USAGE_PROMPT + USAGE_COMPLETION,
    })
}

/// The streamed reply is the SGLang shape, using alternate field placement: a null `prompt_tokens_details` and the thinking
/// beside the totals.
fn streamed_usage_json() -> serde_json::Value {
    serde_json::json!({
        "prompt_tokens": USAGE_PROMPT,
        "prompt_tokens_details": serde_json::Value::Null,
        "completion_tokens": USAGE_COMPLETION,
        "reasoning_tokens": USAGE_REASONING,
        "total_tokens": USAGE_PROMPT + USAGE_COMPLETION,
    })
}

/// Deltas, then the counts, then `[DONE]`: vLLM's own stream order.
fn usage_stream() -> Bytes {
    let delta = |content: &str| {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"index": 0, "delta": {"content": content}}]})
        )
    };
    let mut body = delta("he");
    body.push_str(&delta("llo"));
    body.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({"choices": [], "usage": streamed_usage_json()})
    ));
    body.push_str("data: [DONE]\n\n");
    Bytes::from(body)
}

#[derive(Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// The body as the dictionary replies inflated it; empty for the others.
    pub inflated: Bytes,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn has(&self, name: &str) -> bool {
        self.header(name).is_some()
    }
}

pub struct UpstreamMock {
    pub base: String,
    calls: Arc<Mutex<Vec<Recorded>>>,
    aborted: Arc<Mutex<bool>>,
    connections: Arc<Mutex<usize>>,
    health: Arc<Mutex<Health>>,
    probes: Arc<Mutex<usize>>,
    down: Arc<Mutex<bool>>,
}

impl UpstreamMock {
    pub fn calls(&self) -> Vec<Recorded> {
        self.calls.lock().unwrap().clone()
    }

    /// Change what /health says, the way a redeploy does.
    pub fn advertise(&self, health: Health) {
        *self.health.lock().unwrap() = health;
    }

    /// How many times the forwarder has read /health.
    pub fn probes(&self) -> usize {
        *self.probes.lock().unwrap()
    }

    /// End an outage: work requests are served again.
    pub fn came_back(&self) {
        *self.down.lock().unwrap() = false;
    }

    /// Start one: the edge answers 503 while the upstream is replaced. /health
    /// keeps answering, the way the edge's own health route does.
    pub fn went_down(&self) {
        *self.down.lock().unwrap() = true;
    }

    pub fn last(&self) -> Recorded {
        self.calls()
            .last()
            .expect("a request reached the upstream")
            .clone()
    }

    /// True once an in-flight response body was dropped by the forwarder,
    /// which is how an agent abort must reach the engine.
    pub fn aborted(&self) -> bool {
        *self.aborted.lock().unwrap()
    }

    /// How many TCP connections the forwarder opened: the pool's proof.
    pub fn connections(&self) -> usize {
        *self.connections.lock().unwrap()
    }
}

/// A base URL nothing listens on, for the connect-failure path.
pub fn dead_base() -> String {
    "http://127.0.0.1:1".to_string()
}

pub async fn upstream(health: Health, reply: Reply) -> UpstreamMock {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let aborted: Arc<Mutex<bool>> = Arc::default();
    let connections: Arc<Mutex<usize>> = Arc::default();
    let store: Store = Arc::default();
    let health = Arc::new(Mutex::new(health));
    let probes: Arc<Mutex<usize>> = Arc::default();
    let down = Arc::new(Mutex::new(reply == Reply::Restarting));

    let (c, a, n) = (
        Arc::clone(&calls),
        Arc::clone(&aborted),
        Arc::clone(&connections),
    );
    let (h, p, d) = (Arc::clone(&health), Arc::clone(&probes), Arc::clone(&down));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            *n.lock().unwrap() += 1;
            let (health, calls, aborted) = (Arc::clone(&h), Arc::clone(&c), Arc::clone(&a));
            let (probes, down) = (Arc::clone(&p), Arc::clone(&d));
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let (health, calls, aborted) = (
                        Arc::clone(&health),
                        Arc::clone(&calls),
                        Arc::clone(&aborted),
                    );
                    let (probes, down) = (Arc::clone(&probes), Arc::clone(&down));
                    let store = Arc::clone(&store);
                    async move {
                        respond(request, health, reply, calls, aborted, store, probes, down).await
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    UpstreamMock {
        base: format!("http://{addr}"),
        calls,
        aborted,
        connections,
        health,
        probes,
        down,
    }
}

#[allow(clippy::too_many_arguments)]
async fn respond(
    request: Request<Incoming>,
    health: Arc<Mutex<Health>>,
    reply: Reply,
    calls: Arc<Mutex<Vec<Recorded>>>,
    aborted: Arc<Mutex<bool>>,
    store: Store,
    probes: Arc<Mutex<usize>>,
    down: Arc<Mutex<bool>>,
) -> Result<Response<MockBody>, Infallible> {
    let (parts, incoming) = request.into_parts();
    if parts.uri.path() == "/__portway/capabilities" {
        return match health.lock().unwrap().clone() {
            Health::Portway(encodings) => Ok(Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .header("x-request-encodings", encodings.join(", "))
                .header("x-request-dictionary", "dcz")
                .body(boxed(
                    serde_json::to_vec(&serde_json::json!({
                        "request_encodings": encodings
                    }))
                    .unwrap(),
                ))
                .unwrap()),
            _ => Ok(Response::builder()
                .status(404)
                .body(boxed("not found"))
                .unwrap()),
        };
    }
    if parts.uri.path() == "/health" {
        *probes.lock().unwrap() += 1;
        let advertised = health.lock().unwrap().clone();
        return Ok(serve_health(advertised));
    }
    if *down.lock().unwrap() {
        // Drain the body so the connection stays reusable, the way the edge
        // does while it waits for an upstream that is not there yet.
        let _ = incoming.collect().await;
        return Ok(Response::builder()
            .status(503)
            .body(boxed(r#"{"detail":"no healthy upstream"}"#))
            .unwrap());
    }
    let body = incoming
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let dictionary = matches!(
        reply,
        Reply::Dict
            | Reply::DictAmnesia
            | Reply::DictAmnesiaSplit
            | Reply::DictRolledBack
            | Reply::DictWrongHash
            | Reply::DictRejects
            | Reply::Restarting
    )
    .then(|| dict_reply(reply, &parts.headers, &body, &store));
    let inflated = match &dictionary {
        Some(Ok((inflated, _))) => inflated.clone(),
        _ => Bytes::new(),
    };
    calls.lock().unwrap().push(Recorded {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or("").to_string(),
        headers: parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body,
        inflated,
    });

    match dictionary {
        Some(Ok((_, stored))) => {
            let mut builder = Response::builder()
                .status(200)
                .header("content-type", "application/json");
            if let Some(hash) = stored {
                builder = builder.header("x-dict-stored", hash);
            }
            return Ok(builder.body(boxed(r#"{"ok":true}"#)).unwrap());
        }
        Some(Err(refusal)) => return Ok(refusal.response()),
        None => {}
    }

    if reply == Reply::RejectEncoded && parts.headers.contains_key("content-encoding") {
        return Ok(Response::builder()
            .status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
            .header("x-portway-decode-error", "1")
            .body(boxed(r#"{"detail":"unsupported"}"#))
            .unwrap());
    }

    Ok(match reply {
        Reply::GzipStream => Response::builder()
            .status(200)
            .header("content-encoding", "gzip")
            .header("content-type", "text/event-stream")
            .body(boxed(gzip(&b"data: hello\n\n".repeat(200))))
            .unwrap(),
        Reply::ZstdChunks => Response::builder()
            .status(200)
            .header("content-encoding", "zstd")
            .header("content-type", "text/event-stream")
            .body(stream::Chunks::zstd_pair().boxed())
            .unwrap(),
        Reply::Endless => Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(stream::Endless::new(aborted).boxed())
            .unwrap(),
        Reply::UsageStream => Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(boxed(usage_stream()))
            .unwrap(),
        Reply::UsageJson => Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(boxed(
                serde_json::to_vec(&serde_json::json!({
                    "id": "chatcmpl-1",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "hello"},
                        "finish_reason": "stop"
                    }],
                    "usage": usage_json(),
                }))
                .unwrap(),
            ))
            .unwrap(),
        _ => Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(boxed(r#"{"ok":true}"#))
            .unwrap(),
    })
}

fn serve_health(health: Health) -> Response<MockBody> {
    match health {
        Health::Portway(_) => Response::builder()
            .status(404)
            .body(boxed("not found"))
            .unwrap(),
        Health::Json(encodings) => Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(boxed(
                serde_json::to_vec(&serde_json::json!({ "request_encodings": encodings })).unwrap(),
            ))
            .unwrap(),
        Health::GzipJson(encodings) => Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(boxed(gzip(
                &serde_json::to_vec(&serde_json::json!({ "request_encodings": encodings }))
                    .unwrap(),
            )))
            .unwrap(),
        Health::JsonBare => Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(boxed("{}"))
            .unwrap(),
        Health::PlaintextDict(encodings) => Response::builder()
            .status(200)
            .header("content-type", "text/plain")
            .header("x-request-encodings", encodings.join(", "))
            .header("x-request-dictionary", "dcz")
            .body(boxed("OK"))
            .unwrap(),
        Health::Plaintext(encodings) => {
            let mut builder = Response::builder()
                .status(200)
                .header("content-type", "text/plain");
            if let Some(list) = encodings {
                builder = builder.header("x-request-encodings", list.join(", "));
            }
            builder.body(boxed("OK")).unwrap()
        }
    }
}

type Store = Arc<Mutex<HashMap<String, Bytes>>>;

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct Refusal {
    status: u16,
    miss: bool,
    /// Withhold the body briefly so the head and body reach the client in
    /// separate segments.
    split: bool,
}

fn refuse(status: u16, miss: bool) -> Refusal {
    Refusal {
        status,
        miss,
        split: false,
    }
}

impl Refusal {
    fn response(&self) -> Response<MockBody> {
        let mut builder = Response::builder()
            .status(self.status)
            .header("x-portway-decode-error", "1");
        if self.miss {
            builder = builder.header("x-dict-miss", "1");
        }
        let body: MockBody = if self.split {
            DelayedFull {
                data: Bytes::from_static(br#"{"detail":"refused"}"#),
                sleep: Box::pin(tokio::time::sleep(std::time::Duration::from_millis(60))),
            }
            .boxed()
        } else {
            boxed(r#"{"detail":"refused"}"#)
        };
        builder.body(body).unwrap()
    }
}

/// A one-frame body that sleeps before yielding its data, so the response
/// head and the body arrive in separate segments instead of one write.
struct DelayedFull {
    data: Bytes,
    sleep: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl http_body::Body for DelayedFull {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        if this.sleep.as_mut().poll(cx).is_pending() {
            return std::task::Poll::Pending;
        }
        let data = std::mem::take(&mut this.data);
        if data.is_empty() {
            std::task::Poll::Ready(None)
        } else {
            std::task::Poll::Ready(Some(Ok(http_body::Frame::data(data))))
        }
    }
}

/// The upstream half of `docs/protocol.md`: the inflated body and the
/// hash it was stored under, or the refusal.
fn dict_reply(
    reply: Reply,
    headers: &HeaderMap,
    body: &Bytes,
    store: &Store,
) -> Result<(Bytes, Option<String>), Refusal> {
    const MAGIC: [u8; 8] = [0x5e, 0x2a, 0x4d, 0x18, 0x20, 0x00, 0x00, 0x00];
    let coding = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("identity");
    let inflated: Bytes = match coding {
        "identity" => body.clone(),
        "zstd" => zstd::bulk::decompress(body, 64 << 20)
            .map_err(|_| refuse(400, false))?
            .into(),
        "dcz" => {
            if reply == Reply::DictRolledBack {
                return Err(refuse(415, false));
            }
            if body.len() < 40 || body[..8] != MAGIC {
                return Err(refuse(400, false));
            }
            let named: String = body[8..40].iter().map(|b| format!("{b:02x}")).collect();
            let held = store.lock().unwrap().get(&named).cloned();
            let amnesia = matches!(reply, Reply::DictAmnesia | Reply::DictAmnesiaSplit);
            let Some(base) = held.filter(|_| !amnesia) else {
                return Err(Refusal {
                    status: 412,
                    miss: true,
                    split: matches!(reply, Reply::DictAmnesiaSplit),
                });
            };
            if reply == Reply::DictRejects {
                return Err(refuse(400, false));
            }
            zstd::bulk::Decompressor::with_dictionary(&base)
                .and_then(|mut d| d.decompress(&body[40..], 64 << 20))
                .map_err(|_| refuse(400, false))?
                .into()
        }
        _ => return Err(refuse(415, false)),
    };
    let stored = headers.contains_key("x-dict-store").then(|| {
        let hash = sha256_hex(&inflated);
        store.lock().unwrap().insert(hash.clone(), inflated.clone());
        if reply == Reply::DictWrongHash {
            sha256_hex(b"some other body")
        } else {
            hash
        }
    });
    Ok((inflated, stored))
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

// ---------------------------------------------------------------- forwarder

/// The forwarder under test, bound to loopback.
pub struct Fwd {
    pub base: String,
}

pub async fn forwarder(upstreams: &[(&str, &str)], argv: &[&str]) -> Fwd {
    use clap::Parser;
    let mut full = vec!["portway"];
    full.extend_from_slice(argv);
    let args = Args::parse_from(full);
    let mapped: Vec<(String, String)> = upstreams
        .iter()
        .map(|(model, url)| ((*model).to_string(), (*url).to_string()))
        .collect();
    let router = Router::build(&args.forwarder_config(), &mapped, None).unwrap();
    router.negotiate_all().await;

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(portway::server::serve(listener, router));
    Fwd {
        base: format!("http://{addr}"),
    }
}

pub struct Answer {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Answer {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("response is JSON")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

impl Fwd {
    pub async fn get(&self, path: &str) -> Answer {
        self.send("GET", path, &[], Bytes::new()).await
    }

    pub async fn post(&self, path: &str, body: impl Into<Bytes>) -> Answer {
        self.send("POST", path, &[], body.into()).await
    }

    pub async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Bytes,
    ) -> Answer {
        let (mut send, host) = self.connect().await;
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("host", host);
        if !body.is_empty() {
            builder = builder.header("content-length", body.len().to_string());
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(Full::new(body)).unwrap();
        let response = send.send_request(request).await.unwrap();
        let (parts, incoming) = response.into_parts();
        let body = incoming.collect().await.unwrap().to_bytes();
        Answer {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }

    /// Start a streaming request, read `chunks` frames, then drop everything —
    /// exactly what a coding agent does when the user hits Ctrl-C.
    pub async fn read_then_abort(&self, path: &str, body: Bytes, chunks: usize) {
        let (mut send, host) = self.connect().await;
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("host", host)
            .header("content-length", body.len().to_string())
            .body(Full::new(body))
            .unwrap();
        let response = send.send_request(request).await.unwrap();
        let mut incoming = response.into_body();
        for _ in 0..chunks {
            if incoming.frame().await.is_none() {
                break;
            }
        }
        drop(incoming);
        drop(send);
    }

    async fn connect(&self) -> (hyper::client::conn::http1::SendRequest<Full<Bytes>>, String) {
        let authority = self.base.trim_start_matches("http://").to_string();
        let stream = tokio::net::TcpStream::connect(&authority).await.unwrap();
        let (send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        (send, authority)
    }
}

pub fn empty() -> Empty<Bytes> {
    Empty::new()
}

/// Two hand-rolled response bodies; the suite needs no streaming crate.
mod stream {
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use bytes::Bytes;
    use http_body::{Body, Frame};

    /// Independent zstd frames delivered with an idle gap, the shape that
    /// breaks decoders built on AsyncRead adapters.
    pub struct Chunks {
        chunks: Vec<Vec<u8>>,
        sent: usize,
        gap: Option<Pin<Box<tokio::time::Sleep>>>,
    }

    impl Chunks {
        pub fn zstd_pair() -> Self {
            Chunks {
                chunks: vec![
                    zstd::bulk::compress(b"data: one\n\n", 3).unwrap(),
                    zstd::bulk::compress(b"data: two\n\n", 3).unwrap(),
                ],
                sent: 0,
                gap: None,
            }
        }
    }

    impl Body for Chunks {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            if self.sent >= self.chunks.len() {
                return Poll::Ready(None);
            }
            if self.sent > 0 {
                let gap = self
                    .gap
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(Duration::from_millis(40))));
                if gap.as_mut().poll(cx).is_pending() {
                    return Poll::Pending;
                }
            }
            let chunk = self.chunks[self.sent].clone();
            self.sent += 1;
            self.gap = None;
            Poll::Ready(Some(Ok(Frame::data(Bytes::from(chunk)))))
        }
    }

    /// Emits an SSE line every 5ms until the peer hangs up, flipping the flag
    /// when the body is dropped.
    pub struct Endless {
        tick: Pin<Box<tokio::time::Sleep>>,
        aborted: Arc<Mutex<bool>>,
    }

    impl Endless {
        pub fn new(aborted: Arc<Mutex<bool>>) -> Self {
            Endless {
                tick: Box::pin(tokio::time::sleep(Duration::from_millis(5))),
                aborted,
            }
        }
    }

    impl Body for Endless {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            if self.tick.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.tick
                .as_mut()
                .reset(tokio::time::Instant::now() + Duration::from_millis(5));
            Poll::Ready(Some(Ok(Frame::data(Bytes::from("data: tick\n\n")))))
        }
    }

    impl Drop for Endless {
        fn drop(&mut self) {
            *self.aborted.lock().unwrap() = true;
        }
    }
}

pub const MODEL_UPSTREAMS: &[(&str, &str)] = &[
    ("model-alpha", "http://alpha.example.test"),
    ("model-beta", "http://beta.example.test"),
    ("model-gamma", "http://gamma.example.test"),
];
