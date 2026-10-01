//! Receiver-to-origin compression over real HTTP, without provider credentials.
mod common;
use bytes::Bytes;
use common::Fwd;
use http::{HeaderMap, Response};
use http_body_util::BodyExt;
use portway::{cli::Mode, config::Config};
use portway_core::{
    Receiver, ReceiverConfig, Router,
    origin::{OriginCompressionConfig, OriginCompressionMode},
    relay::OutBody,
    server,
};
use std::{
    collections::VecDeque,
    io::Read,
    sync::{Arc, Mutex},
};
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct Reply {
    status: u16,
    accept: Option<&'static str>,
}
#[derive(Clone)]
struct Call {
    headers: HeaderMap,
    body: Bytes,
}
impl Call {
    fn coding(&self) -> &str {
        self.headers
            .get("content-encoding")
            .map_or("identity", |v| v.to_str().unwrap())
    }
    fn plain(&self) -> Vec<u8> {
        match self.coding() {
            "gzip" => {
                let mut out = Vec::new();
                flate2::read::GzDecoder::new(&self.body[..])
                    .read_to_end(&mut out)
                    .unwrap();
                out
            }
            "zstd" => zstd::bulk::decompress(&self.body, 1 << 20).unwrap(),
            "identity" => self.body.to_vec(),
            other => panic!("unexpected coding {other}"),
        }
    }
}
struct Origin {
    url: String,
    calls: Arc<Mutex<Vec<Call>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn origin(replies: Vec<Reply>) -> Origin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let task = tokio::spawn(server::serve_with(
        listener,
        Arc::default(),
        move |request| {
            let recorded = recorded.clone();
            let replies = replies.clone();
            async move {
                let (parts, body) = request.into_parts();
                assert!(
                    !parts.uri.path().starts_with("/__portway/"),
                    "origin must never be probed"
                );
                assert_ne!(parts.uri.path(), "/health");
                let body = body.collect().await.unwrap().to_bytes();
                recorded.lock().unwrap().push(Call {
                    headers: parts.headers,
                    body,
                });
                let reply = replies.lock().unwrap().pop_front().unwrap_or_default();
                let mut response =
                    Response::builder().status(if reply.status == 0 { 200 } else { reply.status });
                if let Some(accept) = reply.accept {
                    response = response.header("accept-encoding", accept);
                }
                response
                    .body(OutBody::fixed(Bytes::from_static(b"data: done\n\n")))
                    .unwrap()
            }
        },
    ));
    Origin { url, calls, task }
}
fn config(url: &str, enabled: bool) -> Config {
    let mut config = Config {
        upstream: Some(url.into()),
        ..Config::default()
    };
    config.receiver.origin_compression = OriginCompressionConfig {
        mode: if enabled {
            OriginCompressionMode::Auto
        } else {
            OriginCompressionMode::Off
        },
    };
    config
}
async fn receiver(config: &Config) -> (Fwd, Arc<Router>, tokio::task::JoinHandle<()>) {
    let router = config.router(Mode::Receive).unwrap();
    router.negotiate_all().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Fwd {
        base: format!("http://{}", listener.local_addr().unwrap()),
    };
    let task = tokio::spawn(server::serve_receiver(
        listener,
        router.clone(),
        Receiver::new(config.receiver.clone()).unwrap(),
    ));
    (client, router, task)
}
fn payload() -> Bytes {
    Bytes::from(format!("{{\"prompt\":\"{}\"}}", "repeat ".repeat(2000)))
}
fn encodings(origin: &Origin) -> Vec<String> {
    origin
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.coding().to_owned())
        .collect()
}

#[tokio::test]
async fn decoded_sender_body_is_recompressed_for_origin_and_responses_survive() {
    let origin = origin(vec![]).await;
    let (client, router, task) = receiver(&config(&origin.url, true)).await;
    let plain = payload();
    let encoded = Bytes::from(zstd::bulk::compress(&plain, 3).unwrap());
    let response = client
        .send(
            "POST",
            "/v1/messages",
            &[
                ("content-encoding", "zstd"),
                ("accept-encoding", "zstd"),
                ("x-dict-store", "1"),
            ],
            encoded,
        )
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(
        zstd::bulk::decompress(&response.body, 1024).unwrap(),
        b"data: done\n\n"
    );
    let calls = origin.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].coding(), "gzip");
    assert_eq!(calls[0].plain(), plain);
    assert!(!calls[0].headers.contains_key("x-dict-store"));
    assert_eq!(
        calls[0].headers["content-length"],
        calls[0].body.len().to_string()
    );
    let stats = router.routes()[0].1.snapshot();
    assert_eq!(
        stats["origin_compression"]["wire_bytes"],
        calls[0].body.len()
    );
    assert_eq!(stats["origin_compression"]["encoded_attempts"], 1);
    task.abort();
}

#[tokio::test]
async fn bare_415_retries_identity_once_and_remembers_refusal_across_reload() {
    let origin = origin(vec![Reply {
        status: 415,
        accept: None,
    }])
    .await;
    let configuration = config(&origin.url, true);
    let (client, router, task) = receiver(&configuration).await;
    assert_eq!(client.post("/v1/messages", payload()).await.status, 200);
    assert_eq!(client.post("/v1/messages", payload()).await.status, 200);
    assert_eq!(encodings(&origin), ["gzip", "identity", "identity"]);
    let expected_wire: usize = origin
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.body.len())
        .sum();
    let stats = router.routes()[0].1.snapshot();
    assert_eq!(stats["wire_bytes"], expected_wire);
    assert_eq!(stats["retried_identity"], 1);
    assert_eq!(stats["origin_compression"]["wire_bytes"], expected_wire);
    assert_eq!(stats["origin_compression"]["backoff_entries"], 1);
    let next = configuration.router(Mode::Receive).unwrap();
    next.inherit_origin_state(&router);
    let response = next.routes()[0]
        .1
        .forward(http::Request::post("/v1/messages").body(payload()).unwrap())
        .await;
    response.into_body().collect().await.unwrap();
    assert_eq!(encodings(&origin).last().unwrap(), "identity");
    task.abort();
}

#[tokio::test]
async fn unencoded_success_advertises_weighted_request_codings_and_q_zero_is_honored() {
    let origin = origin(vec![
        Reply {
            status: 200,
            accept: Some("gzip;q=0, zstd;q=1"),
        },
        Reply {
            status: 200,
            accept: Some(""),
        },
    ])
    .await;
    let (client, _, task) = receiver(&config(&origin.url, true)).await;
    client.post("/v1/messages", Bytes::from_static(b"{}")).await;
    client.post("/v1/messages", payload()).await;
    client.post("/v1/messages", payload()).await;
    assert_eq!(encodings(&origin), ["identity", "zstd", "identity"]);
    assert_eq!(origin.calls.lock().unwrap()[1].plain(), payload());
    task.abort();
}

#[tokio::test]
async fn application_errors_are_not_replayed_and_only_400_suspends_compression() {
    for status in [400, 401, 403, 429, 500, 502, 503, 504] {
        let origin = origin(vec![Reply {
            status,
            accept: None,
        }])
        .await;
        let (client, _, task) = receiver(&config(&origin.url, true)).await;
        assert_eq!(client.post("/v1/messages", payload()).await.status, status);
        assert_eq!(origin.calls.lock().unwrap().len(), 1);
        client.post("/v1/messages", payload()).await;
        assert_eq!(
            encodings(&origin),
            ["gzip", if status == 400 { "identity" } else { "gzip" }]
        );
        task.abort();
    }
}

#[tokio::test]
async fn refusal_is_isolated_by_target_method_and_credentials() {
    let origin = origin(vec![Reply {
        status: 415,
        accept: None,
    }])
    .await;
    let (client, _, task) = receiver(&config(&origin.url, true)).await;
    client.post("/v1/messages", payload()).await;
    for (method, path, headers) in [
        ("POST", "/v1/messages", vec![]),
        ("POST", "/v1/other", vec![]),
        ("PUT", "/v1/messages", vec![]),
        (
            "POST",
            "/v1/messages",
            vec![("authorization", "Bearer other")],
        ),
        ("POST", "/v1/messages", vec![("x-api-key", "other-key")]),
        ("POST", "/v1/messages?version=2", vec![]),
    ] {
        client.send(method, path, &headers, payload()).await;
    }
    assert_eq!(
        encodings(&origin),
        [
            "gzip", "identity", "identity", "gzip", "gzip", "gzip", "gzip", "gzip"
        ]
    );
    task.abort();
}

#[tokio::test]
async fn explicit_identity_refusal_prevents_fallback_and_identity_retry_is_never_repeated() {
    for (advertisement, expected_calls) in [(Some("identity;q=0"), 1), (None, 2)] {
        let origin = origin(vec![
            Reply {
                status: 415,
                accept: advertisement,
            },
            Reply {
                status: 415,
                accept: None,
            },
        ])
        .await;
        let (client, _, task) = receiver(&config(&origin.url, true)).await;
        assert_eq!(client.post("/v1/messages", payload()).await.status, 415);
        assert_eq!(origin.calls.lock().unwrap().len(), expected_calls);
        task.abort();
    }
}

#[tokio::test]
async fn reload_to_a_different_destination_does_not_inherit_refusal() {
    let old_origin = origin(vec![Reply {
        status: 415,
        accept: None,
    }])
    .await;
    let (client, old_router, task) = receiver(&config(&old_origin.url, true)).await;
    client.post("/v1/messages", payload()).await;
    let new_origin = origin(vec![]).await;
    let next = config(&new_origin.url, true).router(Mode::Receive).unwrap();
    next.inherit_origin_state(&old_router);
    let response = next.routes()[0]
        .1
        .forward(http::Request::post("/v1/messages").body(payload()).unwrap())
        .await;
    response.into_body().collect().await.unwrap();
    assert_eq!(encodings(&new_origin), ["gzip"]);
    task.abort();
}

#[tokio::test]
async fn off_small_and_no_transform_requests_remain_identity() {
    let origin = origin(vec![]).await;
    let (client, _, task) = receiver(&config(&origin.url, false)).await;
    client.post("/v1/messages", payload()).await;
    task.abort();
    let (client, _, task) = receiver(&config(&origin.url, true)).await;
    client.post("/v1/messages", Bytes::from_static(b"{}")).await;
    client
        .send(
            "POST",
            "/v1/messages",
            &[("cache-control", "no-transform")],
            payload(),
        )
        .await;
    assert_eq!(encodings(&origin), ["identity", "identity", "identity"]);
    task.abort();
}

#[test]
fn receiver_configuration_accepts_only_documented_modes() {
    let config: Config = toml::from_str(
        "upstream='http://example.test'\n[receiver.origin_compression]\nmode='auto'",
    )
    .unwrap();
    assert_eq!(
        config.receiver.origin_compression.mode,
        OriginCompressionMode::Auto
    );
    assert_eq!(
        ReceiverConfig::default().origin_compression.mode,
        OriginCompressionMode::Off
    );
    assert!(toml::from_str::<Config>("[receiver.origin_compression]\nmode='force'").is_err());
}

#[tokio::test]
async fn compressed_upload_preserves_incremental_sse_and_cancellation() {
    let upstream = common::upstream(common::Health::JsonBare, common::Reply::ZstdChunks).await;
    let (client, router, task) = receiver(&config(&upstream.base, true)).await;
    let response = client
        .send("POST", "/sse", &[("accept-encoding", "zstd")], payload())
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(
        zstd::bulk::decompress(&response.body, 1024).unwrap(),
        b"data: one\n\ndata: two\n\n"
    );
    assert_eq!(
        router.routes()[0].1.snapshot()["origin_compression"]["encoded_attempts"],
        1
    );
    task.abort();

    let upstream = common::upstream(common::Health::JsonBare, common::Reply::Endless).await;
    let (client, router, task) = receiver(&config(&upstream.base, true)).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        client.read_then_abort("/stream", payload(), 2),
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !upstream.aborted() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        router.routes()[0].1.snapshot()["origin_compression"]["encoded_attempts"],
        1
    );
    assert_eq!(
        router.routes()[0].1.snapshot()["origin_compression"]["probing_entries"],
        0
    );
    task.abort();
}

#[tokio::test]
async fn connection_closed_after_upload_is_not_replayed_and_trial_is_released() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let origin_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = vec![0; 8192];
        assert!(stream.read(&mut bytes).await.unwrap() > 0);
        drop(stream);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "request was replayed after a transport error"
        );
    });
    let (client, router, task) = receiver(&config(&url, true)).await;
    assert_eq!(client.post("/v1/messages", payload()).await.status, 502);
    origin_task.await.unwrap();
    let stats = router.routes()[0].1.snapshot();
    assert_eq!(stats["origin_compression"]["attempts"], 1);
    assert_eq!(stats["origin_compression"]["probing_entries"], 0);
    task.abort();
}

#[tokio::test]
async fn sender_dictionaries_and_origin_gzip_work_independently() {
    let origin = origin(vec![]).await;
    let (receiver, _, task) = receiver(&config(&origin.url, true)).await;
    let sender = common::forwarder(&[("sample", &receiver.base)], &[]).await;
    let first = Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "sample", "prompt": "context ".repeat(6000),
        }))
        .unwrap(),
    );
    let second = Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "sample", "prompt": format!("{}new turn", "context ".repeat(6000)),
        }))
        .unwrap(),
    );
    assert_eq!(sender.post("/v1/messages", first.clone()).await.status, 200);
    assert_eq!(
        sender.post("/v1/messages", second.clone()).await.status,
        200
    );
    let stats = sender.get("/__portway/stats").await.json();
    assert_eq!(stats["upstreams"]["sample"]["dict_hits"], 1);
    let calls = origin.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    for (call, plain) in calls.iter().zip([first, second]) {
        assert_eq!(call.coding(), "gzip");
        assert_eq!(call.plain(), plain);
        assert!(!call.headers.contains_key("x-dict-store"));
    }
    task.abort();
}

#[tokio::test]
async fn incompressible_and_integrity_protected_uploads_are_not_transformed() {
    let origin = origin(vec![]).await;
    let (client, _, task) = receiver(&config(&origin.url, true)).await;
    let mut seed = 123456789u64;
    let random = Bytes::from(
        (0..8192)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect::<Vec<_>>(),
    );
    client.post("/binary", random.clone()).await;
    client
        .send(
            "POST",
            "/signed",
            &[("content-digest", "sha-256=:placeholder:")],
            payload(),
        )
        .await;
    let calls = origin.calls.lock().unwrap();
    assert_eq!(calls[0].coding(), "identity");
    assert_eq!(calls[0].body, random);
    assert_eq!(calls[1].coding(), "identity");
    assert_eq!(calls[1].headers["content-digest"], "sha-256=:placeholder:");
    task.abort();
}

/// One listener, several origins: the routing table serves the receiving side
/// too, so a single receiver fronts providers that speak different APIs.
#[tokio::test]
async fn a_receiver_routes_each_model_to_its_own_origin() {
    let first = origin(vec![]).await;
    let second = origin(vec![]).await;
    let mut config = config(&first.url, false);
    config.upstream = None;
    config
        .models
        .insert("claude-opus-5".into(), first.url.clone());
    config
        .models
        .insert("gpt-6-astra".into(), second.url.clone());
    let (client, _, task) = receiver(&config).await;

    // The capability probe arrives before any request names a model, so it
    // must be answered by the receiver rather than refused by the router.
    let probe = client.get("/__portway/capabilities").await;
    assert_eq!(probe.status, 200);
    assert_eq!(probe.json()["request_encodings"][0], "zstd");

    let routed = Bytes::from_static(br#"{"model":"gpt-6-astra","input":"hi"}"#);
    assert_eq!(client.post("/responses", routed.clone()).await.status, 200);
    assert_eq!(
        client
            .post(
                "/v1/messages",
                Bytes::from_static(br#"{"model":"claude-opus-5"}"#)
            )
            .await
            .status,
        200
    );
    assert_eq!(first.calls.lock().unwrap().len(), 1);
    assert_eq!(
        first.calls.lock().unwrap()[0].plain(),
        br#"{"model":"claude-opus-5"}"#
    );
    assert_eq!(second.calls.lock().unwrap().len(), 1);
    assert_eq!(second.calls.lock().unwrap()[0].plain(), routed);

    // A model outside the table is refused before anything reaches an origin.
    let refused = client
        .post("/responses", Bytes::from_static(br#"{"model":"nope"}"#))
        .await;
    assert_eq!(refused.status, 400);
    assert!(
        refused.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("gpt-6-astra")
    );
    assert_eq!(first.calls.lock().unwrap().len(), 1);
    assert_eq!(second.calls.lock().unwrap().len(), 1);
    task.abort();
}
