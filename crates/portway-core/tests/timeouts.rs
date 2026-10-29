use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Full};
use portway_core::{
    CodingPreference, Event, ForwarderConfig, Receiver, ReceiverConfig, RouteTimeouts, Router,
    Telemetry, origin::OriginCompressionMode, relay::OutBody, server,
};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::Receiver as Events,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinSet,
    time::{sleep, timeout},
};

struct Origin {
    url: String,
    connections: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn origin<F, Fut>(handler: F) -> Origin
where
    F: Fn(TcpStream, usize) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    let task = tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, _) = accepted.unwrap();
                    let index = count.fetch_add(1, Ordering::SeqCst);
                    tasks.spawn(handler(socket, index));
                }
                Some(result) = tasks.join_next(), if !tasks.is_empty() => { result.unwrap(); }
            }
        }
    });
    Origin {
        url,
        connections,
        task,
    }
}

/// Read one buffered request, without waiting for the client to close.
async fn request(socket: &mut TcpStream) -> String {
    let mut data = Vec::new();
    let end = loop {
        let byte = socket.read_u8().await.unwrap();
        data.push(byte);
        if data.ends_with(b"\r\n\r\n") {
            break data.len();
        }
    };
    let headers = String::from_utf8(data).unwrap().to_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .map(|n| n.trim().parse().unwrap())
        })
        .unwrap_or(0);
    assert!(end < 16384);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    headers
}

fn config(name: &str) -> (ForwarderConfig, Events<Event>) {
    let (telemetry, events) = Telemetry::channel(64);
    let mut config = ForwarderConfig {
        coding: CodingPreference::Off,
        telemetry,
        ..Default::default()
    };
    config.timeouts.routes.insert(
        name.into(),
        RouteTimeouts {
            response_headers_seconds: Some(1),
        },
    );
    (config, events)
}

async fn send(router: &Arc<Router>, path: &str) -> http::Response<OutBody> {
    router
        .clone()
        .handle(
            Request::post(path)
                .header("authorization", "Bearer private-test-key")
                .body(Full::new(Bytes::from_static(b"private-test-body")))
                .unwrap(),
        )
        .await
}

fn logs(events: &Events<Event>) -> Vec<String> {
    events
        .try_iter()
        .filter_map(|event| match event {
            Event::Log { message, .. } => Some(message),
            _ => None,
        })
        .collect()
}

fn assert_reason(messages: &[String], reason: &str) {
    let reasons: Vec<_> = messages
        .iter()
        .filter(|line| line.contains("reason="))
        .collect();
    assert_eq!(reasons.len(), 1, "{messages:?}");
    assert!(
        reasons[0].contains(&format!("reason={reason}")),
        "{messages:?}"
    );
    for secret in ["private-test-key", "private-test-body", "private-query"] {
        assert!(
            !messages.iter().any(|line| line.contains(secret)),
            "{messages:?}"
        );
    }
}

fn failed_record(events: &Events<Event>, status: u16, upstream: &str) -> Vec<String> {
    let mut messages = Vec::new();
    let mut records = Vec::new();
    for event in events.try_iter() {
        match event {
            Event::Request(record) => records.push(record),
            Event::Log { message, .. } => messages.push(message),
        }
    }
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.status, status);
    assert_eq!(record.upstream, upstream);
    assert_eq!(record.path, "/responses");
    assert!(!record.complete);
    assert!(record.usage.is_none());
    assert!(record.flight.is_some());
    assert_eq!(record.received, 0);
    assert_eq!(record.body_len, b"private-test-body".len() as u64);
    assert!(record.ttfb > 0.0);
    messages
}

#[tokio::test]
async fn incomplete_client_headers_are_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::single(
        &ForwarderConfig {
            coding: CodingPreference::Off,
            ..Default::default()
        },
        "http://127.0.0.1:9",
        None,
    )
    .unwrap();
    let task = tokio::spawn(server::serve(listener, router));
    let mut client = TcpStream::connect(address).await.unwrap();
    client
        .write_all(b"POST / HTTP/1.1\r\nHost: unfinished")
        .await
        .unwrap();
    // Let the socket register its header timer before advancing virtual time.
    sleep(Duration::from_millis(20)).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    let mut reply = Vec::new();
    timeout(Duration::from_secs(1), client.read_to_end(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.is_empty() || reply.starts_with(b"HTTP/1.1 408"));
    task.abort();
}

#[tokio::test]
async fn stalled_client_bodies_expire_before_forwarding_or_decoding() {
    for receive in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::single(
            &ForwarderConfig {
                coding: CodingPreference::Off,
                ..Default::default()
            },
            "http://127.0.0.1:9",
            None,
        )
        .unwrap();
        let task = if receive {
            tokio::spawn(server::serve_receiver(
                listener,
                router.clone(),
                Receiver::new(Default::default()).unwrap(),
            ))
        } else {
            tokio::spawn(server::serve(listener, router.clone()))
        };
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut interim = Vec::new();
        timeout(Duration::from_secs(1), async {
            while !interim.ends_with(b"\r\n\r\n") {
                interim.push(client.read_u8().await.unwrap());
            }
        })
        .await
        .unwrap();
        assert!(interim.starts_with(b"HTTP/1.1 100"));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::time::resume();
        let mut reply = Vec::new();
        timeout(Duration::from_secs(1), client.read_to_end(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert!(
            reply.starts_with(b"HTTP/1.1 408"),
            "{}",
            String::from_utf8_lossy(&reply)
        );
        assert_eq!(router.routes()[0].1.view().requests, 0);
        if receive {
            assert!(String::from_utf8_lossy(&reply).contains("x-portway-decode-error: 1"));
        }
        task.abort();
    }
}

#[tokio::test]
async fn stalled_headers_close_the_socket_without_replaying_the_post() {
    let (closed, mut closure) = mpsc::unbounded_channel();
    let up = origin(move |mut socket, index| {
        let closed = closed.clone();
        async move {
            request(&mut socket).await;
            if index == 0 {
                assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
                closed.send(()).unwrap();
            } else {
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await
                    .unwrap();
            }
        }
    })
    .await;
    let (config, events) = config("codex");
    let router = Router::build(&config, &[("codex".into(), up.url.clone())], &[], None).unwrap();
    let started = Instant::now();
    let response = timeout(
        Duration::from_secs(3),
        send(&router, "/codex/responses?private-query=1"),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("response headers"));
    assert!(started.elapsed() < Duration::from_secs(2));
    timeout(Duration::from_secs(1), closure.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(up.connections.load(Ordering::SeqCst), 1);
    let stats = router.routes()[0].1.view();
    assert_eq!(
        (stats.in_flight, stats.idle_conns, stats.upstream_errors),
        (0, 0, 1)
    );
    assert!(config.telemetry.flights().views().is_empty());
    let messages = failed_record(&events, 504, "codex");
    assert_reason(&messages, "response_headers_timeout");
    assert!(messages[0].contains("limit=1s"));
    assert_eq!(
        send(&router, "/codex/responses")
            .await
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
        "ok"
    );
    assert_eq!(up.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn timely_headers_allow_a_long_body_and_unconfigured_routes_can_wait() {
    let up = origin(|mut socket, _| async move {
        let headers = request(&mut socket).await;
        if headers.starts_with("post /slow") {
            sleep(Duration::from_millis(1150)).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
        } else {
            sleep(Duration::from_millis(750)).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\no")
                .await
                .unwrap();
            sleep(Duration::from_millis(1100)).await;
            socket.write_all(b"k").await.unwrap();
        }
    })
    .await;
    let (config, events) = config("codex");
    let router = Router::build(
        &config,
        &[
            ("codex".into(), up.url.clone()),
            ("other".into(), up.url.clone()),
        ],
        &[],
        None,
    )
    .unwrap();
    let router = &router;
    let check = |path| async move {
        let response = send(router, path).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "ok"
        );
    };
    tokio::join!(check("/codex/responses"), check("/other/slow"));
    assert!(logs(&events).iter().all(|line| !line.contains("reason=")));
}

#[tokio::test]
async fn a_client_leaving_before_headers_closes_the_upstream_socket() {
    let (state, mut changes) = mpsc::unbounded_channel();
    let up = origin(move |mut socket, _| {
        let state = state.clone();
        async move {
            request(&mut socket).await;
            state.send("received").unwrap();
            assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
            state.send("closed").unwrap();
        }
    })
    .await;
    let (config, events) = config("upstream");
    let router = Router::single(&config, &up.url, None).unwrap();
    let sending = {
        let router = router.clone();
        tokio::spawn(async move { send(&router, "/responses").await })
    };
    assert_eq!(
        timeout(Duration::from_secs(1), changes.recv())
            .await
            .unwrap(),
        Some("received")
    );
    sending.abort();
    assert!(matches!(sending.await, Err(error) if error.is_cancelled()));
    assert_eq!(
        timeout(Duration::from_secs(1), changes.recv())
            .await
            .unwrap(),
        Some("closed")
    );
    let messages = logs(&events);
    assert_reason(&messages, "client_disconnect");
    // Recorded dashboards recognize this suffix when counting header aborts.
    assert!(messages[0].ends_with("agent left before the first byte"));
    let stats = router.routes()[0].1.view();
    assert_eq!(
        (stats.in_flight, stats.client_aborts, stats.idle_conns),
        (0, 1, 0)
    );
    assert_eq!(up.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_disconnect_before_headers_is_a_502_and_is_not_retried() {
    let up = origin(|mut socket, _| async move {
        request(&mut socket).await;
    })
    .await;
    let (config, events) = config("upstream");
    let router = Router::single(&config, &up.url, None).unwrap();
    assert_eq!(
        send(&router, "/responses").await.status(),
        StatusCode::BAD_GATEWAY
    );
    assert_reason(
        &failed_record(&events, 502, "upstream"),
        "upstream_disconnect",
    );
    assert_eq!(up.connections.load(Ordering::SeqCst), 1);
    assert_eq!(router.routes()[0].1.view().in_flight, 0);
}

#[tokio::test]
async fn a_reused_connection_is_closed_when_its_next_response_stalls() {
    let (closed, mut closure) = mpsc::unbounded_channel();
    let up = origin(move |mut socket, _| {
        let closed = closed.clone();
        async move {
            request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
            request(&mut socket).await;
            assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
            closed.send(()).unwrap();
        }
    })
    .await;
    let (config, events) = config("upstream");
    let router = Router::single(&config, &up.url, None).unwrap();
    send(&router, "/responses")
        .await
        .into_body()
        .collect()
        .await
        .unwrap();
    assert_eq!(router.routes()[0].1.view().idle_conns, 1);
    assert_eq!(
        send(&router, "/responses").await.status(),
        StatusCode::GATEWAY_TIMEOUT
    );
    timeout(Duration::from_secs(1), closure.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(up.connections.load(Ordering::SeqCst), 1);
    assert_reason(&logs(&events), "response_headers_timeout");
    assert_eq!(router.routes()[0].1.view().idle_conns, 0);
}

#[tokio::test]
async fn compression_fallback_shares_the_original_deadline() {
    let (closed, mut closure) = mpsc::unbounded_channel();
    let up = origin(move |mut socket, index| {
        let closed = closed.clone();
        async move {
            let headers = request(&mut socket).await;
            if index == 0 {
                assert!(headers.contains("content-encoding: gzip"));
                sleep(Duration::from_millis(700)).await;
                socket
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            } else {
                assert!(!headers.contains("content-encoding:"));
                assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
                closed.send(()).unwrap();
            }
        }
    })
    .await;
    let (mut config, events) = config("upstream");
    config.origin_compression.mode = OriginCompressionMode::Auto;
    let router = Router::single(&config, &up.url, None).unwrap();
    let started = Instant::now();
    let response = router
        .clone()
        .handle(
            Request::post("/responses")
                .body(Full::new(Bytes::from("a".repeat(4000))))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(started.elapsed() < Duration::from_millis(1500));
    timeout(Duration::from_secs(1), closure.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(up.connections.load(Ordering::SeqCst), 2);
    assert_reason(&logs(&events), "response_headers_timeout");
}

#[tokio::test]
async fn a_receiver_timeout_reaches_the_sender_as_504() {
    let up = origin(|mut socket, _| async move {
        request(&mut socket).await;
        assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
    })
    .await;
    let (config, events) = config("codex");
    let router = Router::build(&config, &[("codex".into(), up.url.clone())], &[], None).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/codex", listener.local_addr().unwrap());
    let task = tokio::spawn(server::serve_receiver(
        listener,
        router,
        Receiver::new(ReceiverConfig::default()).unwrap(),
    ));
    let sender = Router::build(
        &ForwarderConfig::default(),
        &[("codex".into(), url)],
        &[],
        None,
    )
    .unwrap();
    sender.negotiate_all().await;
    let response = send(&sender, "/codex/responses").await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(
        String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes())
            .contains("response headers")
    );
    assert_reason(&logs(&events), "response_headers_timeout");
    task.abort();
}

#[tokio::test]
async fn body_disconnects_and_decode_errors_have_distinct_reasons() {
    for (wire, reason) in [
        (
            &b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort"[..],
            "upstream_disconnect",
        ),
        (
            &b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nContent-Encoding: gzip\r\n\r\nnotgzipbytes"
                [..],
            "decode_error",
        ),
    ] {
        let up = origin(move |mut socket, _| async move {
            request(&mut socket).await;
            socket.write_all(wire).await.unwrap();
        })
        .await;
        let (config, events) = config("upstream");
        let router = Router::single(&config, &up.url, None).unwrap();
        let response = send(&router, "/responses?private-query=1").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.into_body().collect().await.is_err());
        let all: Vec<_> = events.try_iter().collect();
        let records: Vec<_> = all
            .iter()
            .filter_map(|event| match event {
                Event::Request(record) => Some(record),
                _ => None,
            })
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, 200);
        assert!(!records[0].complete);
        let messages: Vec<_> = all
            .into_iter()
            .filter_map(|event| match event {
                Event::Log { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert_reason(&messages, reason);
        assert_eq!(router.routes()[0].1.view().idle_conns, 0);
    }
}

#[tokio::test]
async fn a_read_timeout_closes_the_socket_and_is_not_a_client_disconnect() {
    let (closed, mut closure) = mpsc::unbounded_channel();
    let up = origin(move |mut socket, _| {
        let closed = closed.clone();
        async move {
            request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n")
                .await
                .unwrap();
            assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
            closed.send(()).unwrap();
        }
    })
    .await;
    let (config, events) = config("upstream");
    let router = Router::single(&config, &up.url, None).unwrap();
    let mut body = send(&router, "/responses").await.into_body();
    body.frame().await.unwrap().unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(601)).await;
    assert!(body.frame().await.unwrap().is_err());
    tokio::time::resume();
    // Closure is required even while the caller retains the failed body.
    timeout(Duration::from_secs(1), closure.recv())
        .await
        .unwrap()
        .unwrap();
    drop(body);
    assert_reason(&logs(&events), "read_timeout");
    assert_eq!(router.routes()[0].1.view().in_flight, 0);
}

#[tokio::test]
async fn client_disconnects_are_logged_but_a_successful_grace_drain_is_not() {
    for finish in [true, false] {
        let (closed, mut closure) = mpsc::unbounded_channel();
        let up = origin(move |mut socket, _| {
            let closed = closed.clone();
            async move {
                request(&mut socket).await;
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n")
                    .await
                    .unwrap();
                if finish {
                    sleep(Duration::from_millis(25)).await;
                    socket.write_all(b"0\r\n\r\n").await.unwrap();
                } else {
                    // An active generation exceeds the 1KiB grace allowance.
                    sleep(Duration::from_millis(25)).await;
                    socket
                        .write_all(format!("800\r\n{}\r\n", "x".repeat(2048)).as_bytes())
                        .await
                        .unwrap();
                    assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
                }
                closed.send(()).unwrap();
            }
        })
        .await;
        let (config, events) = config("upstream");
        let router = Router::single(&config, &up.url, None).unwrap();
        let mut body = send(&router, "/responses").await.into_body();
        body.frame().await.unwrap().unwrap();
        drop(body);
        timeout(Duration::from_secs(1), closure.recv())
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while router.routes()[0].1.view().in_flight != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let messages = logs(&events);
        if finish {
            assert!(
                messages.iter().all(|line| !line.contains("reason=")),
                "{messages:?}"
            );
        } else {
            assert_reason(&messages, "client_disconnect");
        }
    }
}
