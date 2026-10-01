use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use portway_core::{
    CodingPreference, Event, ForwarderConfig, Receiver, ReceiverConfig, Router, Telemetry, dict,
    relay::OutBody, server,
};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

#[derive(Clone)]
struct Seen {
    method: Method,
    uri: String,
    headers: HeaderMap,
    body: Bytes,
}
struct Origin {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn origin() -> Origin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let task = tokio::spawn(server::serve_with(
        listener,
        Arc::default(),
        move |request| {
            let sink = sink.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = portway_core::body::collect_raw(body, 2 << 20)
                    .await
                    .unwrap();
                let status = if parts.uri.path().ends_with("/400") {
                    400
                } else if parts.uri.path().ends_with("/415") {
                    415
                } else {
                    200
                };
                let broken = parts.uri.path().ends_with("/broken-gzip");
                sink.lock().unwrap().push(Seen {
                    method: parts.method,
                    uri: parts.uri.to_string(),
                    headers: parts.headers,
                    body: body.clone(),
                });
                let mut response = Response::builder()
                    .status(status)
                    .header("etag", "\"example\"");
                if broken {
                    response = response.header("content-encoding", "gzip");
                }
                response.body(OutBody::fixed(body)).unwrap()
            }
        },
    ));
    Origin { url, seen, task }
}
fn config() -> ForwarderConfig {
    ForwarderConfig {
        coding: CodingPreference::Off,
        ..ForwarderConfig::default()
    }
}
async fn send(
    router: &Arc<Router>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Bytes,
) -> (http::StatusCode, Bytes) {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router
        .clone()
        .handle(request.body(Full::new(body)).unwrap())
        .await;
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body)
}

#[tokio::test]
async fn single_upstream_preserves_raw_bodies_paths_queries_and_auth() {
    let origin = origin().await;
    let router = Router::single(&config(), &format!("{}/prefix", origin.url), None).unwrap();
    let data = Bytes::from_static(b"\x00\xffbinary\r\n\x00");
    for path in ["/health", "/v1/models", "/upload?q=%2F%26&x=a+b"] {
        let response = send(
            &router,
            "PUT",
            path,
            &[
                ("authorization", "Bearer client"),
                ("content-type", "application/octet-stream"),
                ("connection", "x-private"),
                ("x-private", "hop-only"),
            ],
            data.clone(),
        )
        .await;
        assert_eq!(response, (http::StatusCode::OK, data.clone()));
        let seen = origin.seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(seen.method, Method::PUT);
        assert_eq!(seen.uri, format!("/prefix{path}"));
        assert_eq!(seen.body, data);
        assert_eq!(seen.headers["authorization"], "Bearer client");
        assert!(!seen.headers.contains_key("x-private"));
    }
    let encoded = Bytes::from(zstd::bulk::compress(&data, 3).unwrap());
    send(
        &router,
        "POST",
        "/compressed",
        &[("content-encoding", "zstd")],
        encoded.clone(),
    )
    .await;
    let seen = origin.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(seen.body, encoded);
    assert_eq!(seen.headers["content-encoding"], "zstd");
}

#[tokio::test]
async fn real_sidecar_restores_dcz_and_does_not_retry_application_errors() {
    let origin = origin().await;
    let next = Router::single(&config(), &origin.url, None).unwrap();
    let receiver = Receiver::new(ReceiverConfig::default()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(server::serve_receiver(listener, next, receiver.clone()));
    let sender = Router::single(&ForwarderConfig::default(), &url, None).unwrap();
    sender.negotiate_all().await;
    let mut seed = 1u64;
    let mut payload: Vec<u8> = (0..40000)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 56) as u8
        })
        .collect();
    payload.extend_from_within(..);
    let data = Bytes::from(payload);
    assert_eq!(
        send(
            &sender,
            "POST",
            "/data",
            &[("authorization", "alice")],
            data.clone()
        )
        .await
        .1,
        data
    );
    let mut second = data.to_vec();
    second.extend_from_slice(b"one more turn");
    let second = Bytes::from(second);
    assert_eq!(
        send(
            &sender,
            "POST",
            "/data",
            &[("authorization", "alice")],
            second.clone()
        )
        .await
        .1,
        second
    );
    assert_eq!(receiver.snapshot()["dict_hits"], 1);
    let stats = sender.routes()[0].1.view();
    assert!(stats.wire_bytes < stats.body_bytes / 2);
    assert_eq!(stats.dict_hits, 1);
    for status in [400, 415] {
        let before = origin.seen.lock().unwrap().len();
        let (actual, body) = send(
            &sender,
            "POST",
            &format!("/{status}"),
            &[("authorization", "alice")],
            second.clone(),
        )
        .await;
        assert_eq!(actual.as_u16(), status);
        assert_eq!(body, second);
        assert_eq!(origin.seen.lock().unwrap().len(), before + 1);
    }
    for seen in origin.seen.lock().unwrap().iter() {
        assert!(!seen.headers.contains_key("content-encoding"));
        assert!(!seen.headers.contains_key("x-dict-store"));
    }
    task.abort();
}

#[tokio::test]
async fn a_dictionary_miss_then_a_zstd_refusal_retries_as_identity() {
    assert_three_attempt_fallback(StatusCode::PRECONDITION_FAILED).await;
}

#[tokio::test]
async fn a_dcz_refusal_then_a_zstd_refusal_retries_as_identity() {
    assert_three_attempt_fallback(StatusCode::UNSUPPORTED_MEDIA_TYPE).await;
}

/// Exercise both refusal checks on one request, over real TCP. The first upload
/// seeds an acknowledged base; only the second upload takes the three-step ladder.
async fn assert_three_attempt_fallback(dcz_status: StatusCode) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let task = tokio::spawn(server::serve_with(
        listener,
        Arc::default(),
        move |request| {
            let sink = sink.clone();
            async move {
                if request.uri().path() == "/__portway/capabilities" {
                    return Response::builder()
                        .header("x-request-dictionary", "dcz")
                        .body(OutBody::fixed(Bytes::from_static(
                            br#"{"request_encodings":["zstd"]}"#,
                        )))
                        .unwrap();
                }
                let (parts, body) = request.into_parts();
                let body = portway_core::body::collect_raw(body, 2 << 20)
                    .await
                    .unwrap();
                let mut calls = sink.lock().unwrap();
                calls.push(Seen {
                    method: parts.method,
                    uri: parts.uri.to_string(),
                    headers: parts.headers,
                    body: body.clone(),
                });
                let response = match calls.len() {
                    1 => {
                        let decoded = zstd::bulk::decompress(&body, 2 << 20).unwrap();
                        Response::builder()
                            .header("x-dict-stored", dict::hex(&dict::sha256(&decoded)))
                    }
                    2 => {
                        let response = Response::builder()
                            .status(dcz_status)
                            .header("x-portway-decode-error", "1");
                        if dcz_status == StatusCode::PRECONDITION_FAILED {
                            response.header("x-dict-miss", "1")
                        } else {
                            response
                        }
                    }
                    3 => Response::builder()
                        .status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
                        .header("x-portway-decode-error", "1"),
                    4 => Response::builder(),
                    _ => panic!("unexpected extra encoding attempt"),
                };
                response.body(OutBody::fixed(body)).unwrap()
            }
        },
    ));
    let upstream = Origin { url, seen, task };
    let sender = Router::single(&ForwarderConfig::default(), &upstream.url, None).unwrap();
    let seed = Bytes::from("a compressible conversation body ".repeat(2048));
    let next = Bytes::from([seed.as_ref(), b"one more turn"].concat());
    let headers = [
        ("authorization", "Bearer test"),
        ("content-type", "text/plain"),
    ];

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        sender.negotiate_all().await;
        assert_eq!(
            send(
                &sender,
                "POST",
                "/chat?stream=false",
                &headers,
                seed.clone()
            )
            .await
            .0,
            StatusCode::OK,
        );
        let response = send(
            &sender,
            "POST",
            "/chat?stream=false",
            &headers,
            next.clone(),
        )
        .await;
        assert_eq!(response, (StatusCode::OK, next.clone()));
    })
    .await
    .expect("both fallback attempts must complete without reusing an unread refusal connection");

    let calls = upstream.seen.lock().unwrap();
    let codings: Vec<_> = calls
        .iter()
        .map(|call| {
            call.headers
                .get("content-encoding")
                .map(|value| value.to_str().unwrap())
                .unwrap_or("identity")
        })
        .collect();
    assert_eq!(codings, ["zstd", "dcz", "zstd", "identity"]);
    for call in calls.iter() {
        assert_eq!(call.method, Method::POST);
        assert_eq!(call.uri, "/chat?stream=false");
        assert_eq!(call.headers["authorization"], "Bearer test");
        assert_eq!(call.headers["content-type"], "text/plain");
    }
    assert_eq!(calls[0].headers["x-dict-store"], "1");
    assert_eq!(calls[1].body[..8], dict::DCZ_MAGIC);
    assert_eq!(calls[1].body[8..40], dict::sha256(&seed));
    assert_eq!(
        zstd::bulk::decompress(&calls[2].body, 2 << 20).unwrap(),
        next
    );
    assert!(!calls[3].headers.contains_key("content-encoding"));
    assert!(!calls[3].headers.contains_key("x-dict-store"));
    assert_eq!(calls[3].body, next);

    let stats = sender.routes()[0].1.snapshot();
    assert_eq!(stats["requests"], 2, "retries are not new logical requests");
    assert_eq!(stats["dict_hits"], 0);
    assert_eq!(
        stats["dict_misses"],
        u64::from(dcz_status == StatusCode::PRECONDITION_FAILED)
    );
    assert_eq!(stats["retried_identity"], 1);
    assert_eq!(stats["coding"], serde_json::Value::Null);
    assert_eq!(stats["dict"], false);
}

#[tokio::test]
async fn separate_instances_have_separate_events_and_transport_counters() {
    let origin = origin().await;
    let (one, rx_one) = Telemetry::channel(16);
    let (two, rx_two) = Telemetry::channel(16);
    let a = Router::single(
        &ForwarderConfig {
            telemetry: one.clone(),
            ..config()
        },
        &origin.url,
        None,
    )
    .unwrap();
    let b = Router::single(
        &ForwarderConfig {
            telemetry: two.clone(),
            ..config()
        },
        &origin.url,
        None,
    )
    .unwrap();
    send(&a, "POST", "/one", &[], Bytes::from_static(b"first")).await;
    for _ in 0..50 {
        if a.routes()[0].1.view().in_flight == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(two.socket_bytes(), (0, 0));
    assert!(rx_two.try_recv().is_err());
    assert!(one.socket_bytes().0 > 0);
    let requests: Vec<_> = rx_one
        .try_iter()
        .filter_map(|e| {
            if let Event::Request(r) = e {
                Some(r)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/one");
    send(&b, "POST", "/two", &[], Bytes::from_static(b"second")).await;
    assert!(two.socket_bytes().0 > 0);
    assert_eq!(a.routes()[0].1.view().requests, 1);
}

#[tokio::test]
async fn malformed_configuration_oversized_bodies_and_unsupported_tunnels_fail_explicitly() {
    let origin = origin().await;
    for url in [
        "example.test",
        "ftp://example.test",
        "http://user:password@example.test",
        "http://example.test?q=1",
    ] {
        assert!(Router::single(&config(), url, None).is_err());
    }
    let router = Router::single(
        &ForwarderConfig {
            max_body_bytes: 3,
            ..config()
        },
        &origin.url,
        None,
    )
    .unwrap();
    assert_eq!(
        send(&router, "POST", "/data", &[], Bytes::from_static(b"1234"))
            .await
            .0,
        413
    );
    assert_eq!(
        send(&router, "CONNECT", "example.test:443", &[], Bytes::new())
            .await
            .0,
        501
    );
    assert!(origin.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_decoder_failure_does_not_return_the_connection_to_the_pool() {
    let origin = origin().await;
    let router = Router::single(&config(), &origin.url, None).unwrap();
    let reply = router
        .clone()
        .handle(
            Request::builder()
                .method("POST")
                .uri("/broken-gzip")
                .body(Full::new(Bytes::from_static(b"not a gzip frame")))
                .unwrap(),
        )
        .await;
    assert!(reply.into_body().collect().await.is_err());
    assert_eq!(router.routes()[0].1.view().idle_conns, 0);
    assert_eq!(
        send(&router, "POST", "/healthy", &[], Bytes::from_static(b"ok"))
            .await
            .1,
        b"ok"[..]
    );
}
#[tokio::test]
async fn response_transformation_updates_encoding_vary_and_validators() {
    let origin = origin().await;
    let router = Router::single(&config(), &origin.url, None).unwrap();
    let data = Bytes::from("repeat ".repeat(100));
    let request = Request::builder()
        .method("POST")
        .uri("/data")
        .header("accept-encoding", "gzip")
        .body(Full::new(data.clone()))
        .unwrap();
    let response = router.clone().handle(request).await;
    assert_eq!(response.headers()["content-encoding"], "gzip");
    assert!(!response.headers().contains_key("etag"));
    assert!(
        response
            .headers()
            .get_all("vary")
            .iter()
            .any(|v| v == "Accept-Encoding")
    );
    let wire = response.into_body().collect().await.unwrap().to_bytes();
    use std::io::Read;
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(&wire[..])
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, data);
    let request = Request::builder()
        .method("HEAD")
        .uri("/head")
        .header("accept-encoding", "gzip")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = router.clone().handle(request).await;
    assert_eq!(response.headers()["etag"], "\"example\"");
    assert_ne!(
        response
            .headers()
            .get("content-encoding")
            .and_then(|v| v.to_str().ok()),
        Some("gzip")
    );
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
}

/// A mount strips its own segment and keeps the upstream's path prefix, and
/// never reads the body: an encoded body goes through as it came.
#[tokio::test]
async fn a_mount_keeps_the_upstream_prefix_and_strips_its_own() {
    let origin = origin().await;
    let router = Router::build(
        &config(),
        &[("codex".to_owned(), format!("{}/backend", origin.url))],
        &[],
        None,
    )
    .unwrap();
    for (path, upstream) in [
        (
            "/codex/models?client_version=1",
            "/backend/models?client_version=1",
        ),
        ("/codex", "/backend/"),
        ("/codex/", "/backend/"),
        ("/codex?x=1", "/backend/?x=1"),
    ] {
        let (status, _) = send(&router, "GET", path, &[], Bytes::new()).await;
        assert_eq!(status, http::StatusCode::OK, "{path}");
        let seen = origin.seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(seen.uri, upstream, "{path}");
    }
    let data = Bytes::from_static(b"{\"model\":\"anything\"}");
    let encoded = Bytes::from(zstd::bulk::compress(&data, 3).unwrap());
    send(
        &router,
        "POST",
        "/codex/responses",
        &[("content-encoding", "zstd")],
        encoded.clone(),
    )
    .await;
    let seen = origin.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(seen.uri, "/backend/responses");
    assert_eq!(seen.body, encoded);
    assert_eq!(seen.headers["content-encoding"], "zstd");
    // Outside the mount nothing answers, and the origin is not asked.
    let before = origin.seen.lock().unwrap().len();
    let (status, _) = send(&router, "POST", "/responses", &[], data).await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert_eq!(origin.seen.lock().unwrap().len(), before);
}
