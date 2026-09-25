//! The console's listener: its own accept loop, the checks in `auth`, the
//! embedded page, and the JSON API. Route order is the security order: the
//! `Host` header first, then the files (which carry no data), the two routes a
//! page needs before it has a session, the session itself, and only then
//! anything that reads or changes state.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http::header::{self, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};

use crate::cli;
use crate::config::Prices;
use crate::control;
use crate::daemon;
use crate::report;
use crate::spend;
use crate::web::aggregate::{Frame, Shared};
use crate::web::api;
use crate::web::assets;
use crate::web::auth::{Auth, CSRF_HEADER};
use crate::web::body::WebBody;
use crate::web::{Control, ring::Gap};

/// Open streams, all pages together. Each is a task and a broadcast slot.
const MAX_STREAMS: usize = 16;
/// The largest body the console reads: a token and its braces.
const MAX_BODY: usize = 1024;
/// Events one `/api/events` page carries at most.
const MAX_PAGE: usize = 1000;
const PING: Duration = Duration::from_secs(15);
/// Long enough for the 202 and the `stopping` frame to reach the page.
const STOP_GRACE: Duration = Duration::from_millis(150);

const CSP: &str = "default-src 'self'; img-src 'self'; frame-ancestors 'none'; \
                   base-uri 'none'; form-action 'none'";

pub struct App {
    pub auth: Auth,
    pub shared: Arc<Shared>,
    /// Fixed at start; `snapshot` adds what moves.
    pub header: Value,
    pub control: Control,
    pub db: Option<std::path::PathBuf>,
    pub prices: Prices,
    pub streams: AtomicUsize,
    pub closing: watch::Receiver<bool>,
}

/// Accept until `closing` turns true, then drop every connection with it.
pub async fn serve(listener: TcpListener, app: Arc<App>, mut closing: watch::Receiver<bool>) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                let _ = stream.set_nodelay(true);
                let app = Arc::clone(&app);
                connections.spawn(async move {
                    let service = service_fn(move |request| {
                        let app = Arc::clone(&app);
                        async move { Ok::<_, Infallible>(handle(app, request).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
            _ = closing.changed() => break,
        }
    }
}

pub async fn handle(app: Arc<App>, request: Request<Incoming>) -> Response<WebBody> {
    // Exactly one: a second `Host` is a request that means two things.
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = match (hosts.next(), hosts.next()) {
        (Some(host), None) => host.to_str().ok().map(str::to_owned),
        _ => None,
    };
    if !app.auth.host_allowed(host.as_deref()) {
        return error(StatusCode::FORBIDDEN, "host not allowed");
    }
    let host = host.unwrap_or_default();
    let path = request.uri().path().to_owned();
    let method = request.method().clone();

    if !path.starts_with("/api/") {
        return match method {
            Method::GET | Method::HEAD => asset(&path, &request),
            _ => error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
        };
    }
    let session = app.auth.has_session(
        request
            .headers()
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok()),
    );
    if method == Method::POST {
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok());
        if !app.auth.origin_allowed(origin, &host) || !request.headers().contains_key(CSRF_HEADER) {
            return error(StatusCode::FORBIDDEN, "cross-site request refused");
        }
    }
    match (&method, path.as_str()) {
        (&Method::GET, "/api/health") => json(
            StatusCode::OK,
            &json!({
                "console": "portway",
                "version": env!("CARGO_PKG_VERSION"),
                "session": session,
            }),
        ),
        (&Method::POST, "/api/session") => open_session(&app, request).await,
        _ if !session => error(StatusCode::UNAUTHORIZED, "session required"),
        (&Method::GET, "/api/snapshot") => {
            let header = app.header.clone();
            let shared = Arc::clone(&app.shared);
            match tokio::task::spawn_blocking(move || shared.snapshot(header)).await {
                Ok(body) => json_text(StatusCode::OK, body),
                Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "snapshot failed"),
            }
        }
        (&Method::GET, "/api/stream") => {
            let after = query(&request, "after")
                .or_else(|| {
                    request
                        .headers()
                        .get("last-event-id")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned)
                })
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            stream(&app, after)
        }
        (&Method::GET, "/api/events") => {
            let before = query(&request, "before")
                .and_then(|value| value.parse().ok())
                .unwrap_or(u64::MAX);
            let limit = query(&request, "limit")
                .and_then(|value| value.parse().ok())
                .unwrap_or(MAX_PAGE)
                .min(MAX_PAGE);
            let (events, oldest) = app.shared.before(before, limit);
            json_text(
                StatusCode::OK,
                format!(
                    "{{\"events\":[{}],\"oldest\":{oldest}}}",
                    events
                        .iter()
                        .map(|event| &**event)
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            )
        }
        (&Method::GET, "/api/usage") => usage(&app, query(&request, "range")).await,
        (&Method::GET, "/api/report") => {
            let since = query(&request, "since");
            let model = query(&request, "model").filter(|model| !model.is_empty());
            let text = query(&request, "text").is_some_and(|value| value == "1");
            report(&app, since, model, text).await
        }
        (&Method::POST, "/api/reload") => reload(&app).await,
        (&Method::POST, "/api/stop") => stop(&app).await,
        (_, "/api/health" | "/api/session" | "/api/snapshot" | "/api/stream" | "/api/events")
        | (_, "/api/usage" | "/api/report" | "/api/reload" | "/api/stop") => {
            error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
        }
        _ => error(StatusCode::NOT_FOUND, "no such route"),
    }
}

/// Trade the launch token for the session cookie. The token is checked, never
/// stored by the page, and never needed again.
async fn open_session(app: &App, request: Request<Incoming>) -> Response<WebBody> {
    let body = match Limited::new(request.into_body(), MAX_BODY).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "body too large"),
    };
    let token = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value.get("token")?.as_str().map(str::to_owned));
    match token {
        Some(token) if app.auth.token_ok(&token) => {
            let mut response = respond(StatusCode::NO_CONTENT, None, WebBody::empty());
            if let Ok(cookie) = HeaderValue::from_str(&app.auth.set_cookie()) {
                response.headers_mut().insert(header::SET_COOKIE, cookie);
            }
            response
        }
        _ => error(StatusCode::UNAUTHORIZED, "invalid token"),
    }
}

/// The event stream: the backlog after `after`, then every frame as it is
/// broadcast, until the page goes, the console closes, or the page falls so
/// far behind that starting over is cheaper.
fn stream(app: &Arc<App>, after: u64) -> Response<WebBody> {
    if app.streams.fetch_add(1, Ordering::AcqRel) >= MAX_STREAMS {
        app.streams.fetch_sub(1, Ordering::AcqRel);
        return error(StatusCode::SERVICE_UNAVAILABLE, "too many open streams");
    }
    let guard = StreamSlot(Arc::clone(app));
    let (sender, receiver) = mpsc::channel::<Bytes>(64);
    let resumed = app.shared.resume(after);
    let mut closing = app.closing.clone();
    tokio::spawn(async move {
        let _slot = guard;
        let (mut frames, backlog) = match resumed {
            Ok(resumed) => resumed,
            Err(Gap) => {
                let _ = sender.send(sse("reset", None, r#"{"reason":"gap"}"#)).await;
                return;
            }
        };
        if sender
            .send(Bytes::from_static(b"retry: 2000\n\n"))
            .await
            .is_err()
        {
            return;
        }
        let mut last = after;
        for (seq, json) in backlog {
            if sender.send(sse("ev", Some(seq), &json)).await.is_err() {
                return;
            }
            last = seq;
        }
        let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING, PING);
        loop {
            let chunk = tokio::select! {
                frame = frames.recv() => match frame {
                    Ok(Frame::Event { seq, json }) if seq > last => {
                        last = seq;
                        sse("ev", Some(seq), &json)
                    }
                    Ok(Frame::Event { .. }) => continue,
                    Ok(Frame::Tick(json)) => sse("tick", None, &json),
                    Ok(Frame::Flights(json)) => sse("flights", None, &json),
                    Ok(Frame::Control(json)) => sse("control", None, &json),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = sender.send(sse("reset", None, r#"{"reason":"lagged"}"#)).await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = ping.tick() => Bytes::from_static(b": ping\n\n"),
                _ = sender.closed() => return,
                _ = closing.changed() => return,
            };
            if sender.send(chunk).await.is_err() {
                return;
            }
        }
    });
    let mut response = respond(
        StatusCode::OK,
        Some("text/event-stream"),
        WebBody::Stream(receiver),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// Gives the stream's slot back however its task ends.
struct StreamSlot(Arc<App>);

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.streams.fetch_sub(1, Ordering::AcqRel);
    }
}

fn sse(event: &str, id: Option<u64>, data: &str) -> Bytes {
    let mut frame = String::with_capacity(data.len() + 32);
    frame.push_str("event: ");
    frame.push_str(event);
    frame.push('\n');
    if let Some(id) = id {
        frame.push_str(&format!("id: {id}\n"));
    }
    // JSON has no raw newlines, but a frame must never be split by one.
    for line in data.split('\n') {
        frame.push_str("data: ");
        frame.push_str(line);
        frame.push('\n');
    }
    frame.push('\n');
    Bytes::from(frame)
}

async fn usage(app: &App, range: Option<String>) -> Response<WebBody> {
    let range = match range.as_deref() {
        None | Some("") => spend::Range::Today,
        Some(key) => match spend::Range::from_key(key) {
            Some(range) => range,
            None => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "range must be today, yesterday, week or month",
                );
            }
        },
    };
    let Some(db) = app.db.clone() else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "no database to read");
    };
    let prices = app.prices.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        let (since, until) = range.window(crate::logfmt::epoch());
        spend::load(&db, since, until, &prices)
    })
    .await;
    match loaded {
        Ok(Ok(table)) => json(StatusCode::OK, &api::usage(range, &table)),
        Ok(Err(message)) => error(StatusCode::UNPROCESSABLE_ENTITY, &message),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "usage read failed"),
    }
}

async fn report(
    app: &App,
    since: Option<String>,
    model: Option<String>,
    text: bool,
) -> Response<WebBody> {
    let since = match cli::parse_span(since.as_deref().unwrap_or("24h")) {
        Ok(since) => since,
        Err(message) => return error(StatusCode::BAD_REQUEST, &message),
    };
    let Some(db) = app.db.clone() else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "no database to read");
    };
    let loaded =
        tokio::task::spawn_blocking(move || report::load(&db, since, model.as_deref())).await;
    match loaded {
        Ok(Ok(report)) => json(StatusCode::OK, &api::report(&report, text)),
        Ok(Err(message)) => error(StatusCode::UNPROCESSABLE_ENTITY, &message),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "report read failed"),
    }
}

async fn reload(app: &App) -> Response<WebBody> {
    match &app.control {
        Control::Live { args, cell, .. } => match control::reload(args, cell).await {
            Ok(routes) => {
                let message = format!("configuration reloaded ({routes} route(s))");
                crate::logfmt::info(&format!("console: {message}"));
                json(
                    StatusCode::OK,
                    &json!({"message": message, "routes": routes}),
                )
            }
            Err(message) => {
                crate::logfmt::error(&format!(
                    "console: keeping existing configuration: {message}"
                ));
                error(StatusCode::UNPROCESSABLE_ENTITY, &message)
            }
        },
        Control::Attached { dir } => {
            let dir = dir.clone();
            match tokio::task::spawn_blocking(move || daemon::reload(&dir)).await {
                Ok(Ok(message)) => json(StatusCode::ACCEPTED, &json!({"message": message})),
                Ok(Err(message)) => error(StatusCode::CONFLICT, &message),
                Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "reload failed"),
            }
        }
    }
}

async fn stop(app: &App) -> Response<WebBody> {
    match &app.control {
        Control::Live { stop, .. } => {
            crate::logfmt::info("console: stop requested");
            app.shared.announce(json!({"event": "stopping"}));
            let stop = Arc::clone(stop);
            tokio::spawn(async move {
                tokio::time::sleep(STOP_GRACE).await;
                stop.notify_one();
            });
            json(StatusCode::ACCEPTED, &json!({"message": "stopping"}))
        }
        Control::Attached { dir } => {
            let dir = dir.clone();
            match tokio::task::spawn_blocking(move || daemon::stop(&dir)).await {
                Ok(Ok(message)) => json(StatusCode::OK, &json!({"message": message})),
                Ok(Err(message)) => error(StatusCode::CONFLICT, &message),
                Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "stop failed"),
            }
        }
    }
}

fn asset(path: &str, request: &Request<Incoming>) -> Response<WebBody> {
    let Some(file) = assets::get(path) else {
        return error(StatusCode::NOT_FOUND, "no such file");
    };
    let etag = file.etag();
    let fresh = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|tag| tag.trim() == etag));
    let mut response = if fresh {
        respond(StatusCode::NOT_MODIFIED, None, WebBody::empty())
    } else {
        respond(
            StatusCode::OK,
            Some(file.content_type),
            if request.method() == Method::HEAD {
                WebBody::empty()
            } else {
                WebBody::full(Bytes::from_static(file.content.as_bytes()))
            },
        )
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(etag) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, etag);
    }
    response
}

/// One query parameter, percent-decoded.
fn query(request: &Request<Incoming>, name: &str) -> Option<String> {
    request
        .uri()
        .query()?
        .split('&')
        .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
        .find(|(key, _)| decode(key) == name)
        .map(|(_, value)| decode(value))
}

fn decode(text: &str) -> String {
    let hex = |byte: u8| (byte as char).to_digit(16);
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' if at + 2 < bytes.len() => match (hex(bytes[at + 1]), hex(bytes[at + 2])) {
                (Some(high), Some(low)) => {
                    out.push((high * 16 + low) as u8);
                    at += 2;
                }
                _ => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn respond(
    status: StatusCode,
    content_type: Option<&'static str>,
    body: WebBody,
) -> Response<WebBody> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    if let Some(content_type) = content_type {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn json(status: StatusCode, value: &Value) -> Response<WebBody> {
    json_text(status, value.to_string())
}

fn json_text(status: StatusCode, text: String) -> Response<WebBody> {
    respond(
        status,
        Some("application/json; charset=utf-8"),
        WebBody::full(text),
    )
}

fn error(status: StatusCode, message: &str) -> Response<WebBody> {
    json(status, &api::error(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_percent_decoded() {
        assert_eq!(decode("a%2Fb+c"), "a/b c");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("%E2%9C%93"), "✓");
    }

    #[test]
    fn a_frame_is_never_split_by_a_newline() {
        assert_eq!(
            &sse("ev", Some(3), "{}")[..],
            b"event: ev\nid: 3\ndata: {}\n\n"
        );
        assert_eq!(
            &sse("x", None, "a\nb")[..],
            b"event: x\ndata: a\ndata: b\n\n"
        );
    }
}
