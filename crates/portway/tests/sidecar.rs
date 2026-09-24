mod common;
use bytes::Bytes;
use common::{Health, Reply, forwarder, upstream};
use portway_core::{CodingPreference, ForwarderConfig, Receiver, ReceiverConfig, Router, server};
use std::sync::Arc;

async fn sidecar(url: &str) -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::single(
        &ForwarderConfig {
            coding: CodingPreference::Off,
            ..ForwarderConfig::default()
        },
        url,
        None,
    )
    .unwrap();
    let receiver = Receiver::new(ReceiverConfig::default()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(server::serve_receiver(listener, router, receiver)),
    )
}
#[tokio::test]
async fn cancelling_a_stream_crosses_both_forwarding_hops() {
    let origin = upstream(Health::JsonBare, Reply::Endless).await;
    let (url, task) = sidecar(&origin.base).await;
    let sender = forwarder(&[("sample", &url)], &[]).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        sender.read_then_abort("/stream", Bytes::from_static(br#"{"model":"sample"}"#), 2),
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !origin.aborted() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("origin stream must be cancelled");
    task.abort();
}
#[tokio::test]
async fn streamed_compression_survives_both_hops_and_client_recompression() {
    let origin = upstream(Health::JsonBare, Reply::ZstdChunks).await;
    let (url, task) = sidecar(&origin.base).await;
    let sender = forwarder(&[("sample", &url)], &[]).await;
    let reply = sender
        .send(
            "GET",
            "/sse?model=sample",
            &[("accept-encoding", "zstd")],
            Bytes::new(),
        )
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("content-encoding"), Some("zstd"));
    let body = zstd::bulk::decompress(&reply.body, 1024).unwrap();
    assert_eq!(body, b"data: one\n\ndata: two\n\n");
    task.abort();
}
#[tokio::test]
async fn discarding_the_server_future_cancels_its_owned_connections() {
    let origin = upstream(Health::JsonBare, Reply::Endless).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = Router::single(
        &ForwarderConfig {
            coding: CodingPreference::Off,
            ..ForwarderConfig::default()
        },
        &origin.base,
        None,
    )
    .unwrap();
    let task = tokio::spawn(server::serve(listener, Arc::clone(&router)));
    let client = common::Fwd {
        base: format!("http://127.0.0.1:{port}"),
    };
    let request = tokio::spawn(async move { client.post("/sse", Bytes::new()).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while origin.calls().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !origin.aborted() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    request.abort();
}
