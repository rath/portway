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
                let encoding = if parts.uri.path().ends_with("/broken-gzip") {
                    Some("gzip")
                } else {
                    ["gzip", "deflate", "zstd"]
                        .into_iter()
                        .find(|coding| parts.uri.path().ends_with(&format!("/{coding}")))
                };
                sink.lock().unwrap().push(Seen {
                    method: parts.method,
                    uri: parts.uri.to_string(),
                    headers: parts.headers,
                    body: body.clone(),
                });
                let mut response = Response::builder()
                    .status(status)
                    .header("etag", "\"example\"");
                if let Some(encoding) = encoding {
                    response = response.header("content-encoding", encoding);
                }
                if parts.uri.path().ends_with("/stacked") {
                    response = response
                        .header("content-encoding", "gzip")
                        .header("content-encoding", "gzip");
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

#[tokio::test]
async fn repeated_encoding_headers_keep_the_whole_representation() {
    use std::io::Write;
    let origin = origin().await;
    let router = Router::single(&config(), &origin.url, None).unwrap();
    let mut wire = b"a layered response".to_vec();
    for _ in 0..2 {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&wire).unwrap();
        wire = encoder.finish().unwrap();
    }
    for preserve in [false, true] {
        let mut request = Request::post("/stacked")
            .header("accept-encoding", "gzip")
            .header("accept-encoding", "identity");
        if preserve {
            request = request.header("cache-control", "no-transform");
        }
        let response = router
            .clone()
            .handle(request.body(Full::new(Bytes::from(wire.clone()))).unwrap())
            .await;
        let codings: Vec<_> = response
            .headers()
            .get_all("content-encoding")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(codings, ["gzip", "gzip"]);
        assert_eq!(response.headers()["etag"], "\"example\"");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            wire
        );
        if preserve {
            let seen = origin.seen.lock().unwrap();
            let accepted: Vec<_> = seen
                .last()
                .unwrap()
                .headers
                .get_all("accept-encoding")
                .iter()
                .map(|v| v.to_str().unwrap())
                .collect();
            assert_eq!(accepted, ["gzip", "identity"]);
        }
    }
    let response = router
        .handle(
            Request::post("/plain")
                .header("accept-encoding", "gzip;q=0")
                .header("accept-encoding", "zstd;q=1")
                .body(Full::new(Bytes::from_static(b"plain response")))
                .unwrap(),
        )
        .await;
    assert_eq!(response.headers()["content-encoding"], "zstd");
    let wire = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        zstd::bulk::decompress(&wire, 100).unwrap(),
        b"plain response"
    );
}

#[tokio::test]
async fn compressed_responses_expand_in_bounded_frames() {
    use std::io::Write;
    let origin = origin().await;
    let plain = vec![b'x'; 8 << 20];
    for coding in ["gzip", "deflate", "zstd"] {
        let compressed = match coding {
            "gzip" => {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(&plain).unwrap();
                encoder.finish().unwrap()
            }
            "deflate" => {
                let mut encoder =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(&plain).unwrap();
                encoder.finish().unwrap()
            }
            _ => zstd::bulk::compress(&plain, 3).unwrap(),
        };
        let router = Router::single(&config(), &origin.url, None).unwrap();
        let response = router
            .clone()
            .handle(
                Request::post(format!("/{coding}"))
                    .body(Full::new(Bytes::from(compressed)))
                    .unwrap(),
            )
            .await;
        let mut body = response.into_body();
        let mut total = 0;
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame.unwrap().into_data() {
                assert!(
                    data.len() <= 32 * 1024,
                    "{coding}: {} decoded bytes in one frame",
                    data.len()
                );
                assert!(data.iter().all(|b| *b == b'x'));
                total += data.len();
            }
        }
        assert_eq!(total, plain.len(), "{coding}");
        assert_eq!(router.routes()[0].1.view().idle_conns, 1);
    }
}

#[tokio::test]
async fn disconnect_stops_expanding_the_buffered_compressed_response() {
    let origin = origin().await;
    let (telemetry, events) = Telemetry::channel(16);
    let router = Router::single(
        &ForwarderConfig {
            telemetry,
            ..config()
        },
        &origin.url,
        None,
    )
    .unwrap();
    let wire = zstd::bulk::compress(&vec![b'x'; 8 << 20], 3).unwrap();
    let response = router
        .clone()
        .handle(
            Request::post("/zstd")
                .body(Full::new(Bytes::from(wire)))
                .unwrap(),
        )
        .await;
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(!first.is_empty());
    drop(body);

    let record = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(record) = events.try_iter().find_map(|event| match event {
                Event::Request(record) => Some(record),
                _ => None,
            }) {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!record.complete);
    assert!(record.received <= first.len() as u64 + 32 * 1024);
    assert_eq!(router.routes()[0].1.view().idle_conns, 0);
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
async fn a_truncated_codec_is_incomplete_even_after_client_disconnect() {
    use std::io::Write;
    let origin = origin().await;
    for drain in [false, true] {
        let (telemetry, events) = Telemetry::channel(16);
        let router = Router::single(
            &ForwarderConfig {
                telemetry,
                ..config()
            },
            &origin.url,
            None,
        )
        .unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"complete plaintext").unwrap();
        let mut wire = encoder.finish().unwrap();
        wire.truncate(wire.len() - 8);
        let reply = router
            .clone()
            .handle(
                Request::post("/gzip")
                    .body(Full::new(Bytes::from(wire)))
                    .unwrap(),
            )
            .await;
        let mut body = reply.into_body();
        if drain {
            assert!(body.frame().await.unwrap().is_ok());
            drop(body);
        } else {
            assert!(body.collect().await.is_err());
        }
        let record = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(record) = events.try_iter().find_map(|event| match event {
                    Event::Request(record) => Some(record),
                    _ => None,
                }) {
                    break record;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!record.complete);
        assert!(record.usage.is_none());
        assert_eq!(router.routes()[0].1.view().idle_conns, 0);
    }
}

#[tokio::test]
async fn request_no_transform_preserves_the_response_bytes_and_validator() {
    use std::io::Write;
    let origin = origin().await;
    let router = Router::single(&config(), &origin.url, None).unwrap();
    let mut encoder = flate2::GzBuilder::new()
        .mtime(123)
        .write(Vec::new(), flate2::Compression::default());
    encoder.write_all(b"payload").unwrap();
    let wire = Bytes::from(encoder.finish().unwrap());
    let response = router
        .clone()
        .handle(
            Request::post("/gzip")
                .header("cache-control", "no-transform")
                .header("accept-encoding", "gzip")
                .body(Full::new(wire.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(response.headers()["content-encoding"], "gzip");
    assert_eq!(response.headers()["etag"], "\"example\"");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        wire
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
    // Recoded bytes keep the validator, weakened: same content, other bytes.
    assert_eq!(response.headers()["etag"], "W/\"example\"");
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
