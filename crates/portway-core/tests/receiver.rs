use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::Full;
use portway_core::{
    Receiver, ReceiverConfig,
    dict::{self, DictionaryScope},
    receiver::ERROR_HEADER,
};
use std::sync::Arc;

fn config() -> ReceiverConfig {
    ReceiverConfig {
        min_dictionary_bytes: 16,
        max_dictionary_bytes: 4096,
        max_body_bytes: 8192,
        dictionary_bytes: 8192,
        ..ReceiverConfig::default()
    }
}
fn request(coding: &str, body: impl Into<Bytes>, auth: &str, keep: bool) -> Request<Full<Bytes>> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/data")
        .header("content-encoding", coding)
        .header("authorization", auth);
    if keep {
        request = request.header("x-dict-store", "1");
    }
    request.body(Full::new(body.into())).unwrap()
}
async fn decode(
    receiver: &Arc<Receiver>,
    coding: &str,
    body: impl Into<Bytes>,
    auth: &str,
    keep: bool,
) -> portway_core::receiver::DecodedRequest {
    match receiver.decode(request(coding, body, auth, keep)).await {
        Ok(d) => d,
        Err(e) => panic!("decode failed: {}", e.status()),
    }
}
async fn rejected(receiver: &Arc<Receiver>, coding: &str, body: impl Into<Bytes>, status: u16) {
    match receiver.decode(request(coding, body, "alice", true)).await {
        Ok(_) => panic!("accepted invalid input"),
        Err(e) => {
            assert_eq!(e.status().as_u16(), status);
            assert!(e.into_response().headers().contains_key(ERROR_HEADER));
        }
    }
}
fn body() -> Bytes {
    Bytes::from("dictionary content ".repeat(64))
}
fn zstd(body: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(body, 3).unwrap()
}
fn gzip(body: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    w.write_all(body).unwrap();
    w.finish().unwrap()
}
fn dcz(body: &[u8], base: &Bytes) -> Vec<u8> {
    let base = dict::Base {
        hash: dict::sha256(base),
        body: base.clone(),
    };
    dict::compress(body, &base, 3).unwrap()
}

#[tokio::test]
async fn dictionaries_round_trip_only_in_their_context() {
    let receiver = Receiver::new(config()).unwrap();
    let first = body();
    let decoded = decode(&receiver, "zstd", zstd(&first), "alice", true).await;
    assert_eq!(decoded.request.body(), &first);
    assert_eq!(decoded.request.headers()["authorization"], "alice");
    assert!(!decoded.request.headers().contains_key("content-encoding"));
    let (_, ack) = decoded.into_parts();
    let reply = ack.finish(http::Response::new(()));
    assert_eq!(
        reply.headers()["x-dict-stored"],
        dict::hex(&dict::sha256(&first))
    );
    let base = dict::Base {
        hash: dict::sha256(&first),
        body: first.clone(),
    };
    let second = Bytes::from(format!("{}another turn", String::from_utf8_lossy(&first)));
    let wire = dict::compress(&second, &base, 3).unwrap();
    let decoded = decode(&receiver, "dcz", wire.clone(), "alice", true).await;
    assert_eq!(decoded.request.body(), &second);
    let error = receiver
        .decode(request("dcz", wire, "bob", false))
        .await
        .err()
        .unwrap()
        .into_response();
    assert_eq!(error.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(error.headers()["x-dict-miss"], "1");
    assert_eq!(receiver.snapshot()["dict_hits"], 1);
}

#[tokio::test]
async fn explicit_scope_overrides_credential_partition() {
    let receiver = Receiver::new(config()).unwrap();
    let first = body();
    let scope = DictionaryScope::new(b"application-user-42");
    let mut req = request("zstd", zstd(&first), "rotating-token-a", true);
    req.extensions_mut().insert(scope.clone());
    assert!(receiver.decode(req).await.is_ok());
    let wire = dict::compress(
        &first,
        &dict::Base {
            hash: dict::sha256(&first),
            body: first.clone(),
        },
        3,
    )
    .unwrap();
    let mut req = request("dcz", wire, "rotating-token-b", false);
    req.extensions_mut().insert(scope);
    assert!(receiver.decode(req).await.is_ok());
}

#[tokio::test]
async fn malformed_truncated_concatenated_and_oversized_bodies_are_rejected() {
    let receiver = Receiver::new(config()).unwrap();
    rejected(&receiver, "dcz", b"bad".to_vec(), 400).await;
    rejected(&receiver, "zstd", b"bad".to_vec(), 400).await;
    let encoded = zstd(&body());
    rejected(
        &receiver,
        "zstd",
        encoded[..encoded.len() - 1].to_vec(),
        400,
    )
    .await;
    let mut trailing = encoded.clone();
    trailing.extend_from_slice(&encoded);
    rejected(&receiver, "zstd", trailing, 400).await;
    rejected(&receiver, "zstd", zstd(&vec![0; 8193]), 413).await;
    rejected(&receiver, "identity", vec![0; 8193], 413).await;
    rejected(&receiver, "br", vec![], 415).await;
    rejected(&receiver, "gzip, zstd", vec![], 415).await;
    let mut req = request("gzip", vec![], "alice", false);
    req.headers_mut()
        .append("content-encoding", http::HeaderValue::from_static("zstd"));
    assert_eq!(receiver.decode(req).await.err().unwrap().status(), 415);
}

#[tokio::test]
async fn checksum_is_mandatory_and_must_match() {
    let receiver = Receiver::new(config()).unwrap();
    let first = body();
    decode(&receiver, "zstd", zstd(&first), "alice", true).await;
    let mut wire = dict::compress(
        &first,
        &dict::Base {
            hash: dict::sha256(&first),
            body: first.clone(),
        },
        3,
    )
    .unwrap();
    let last = wire.len() - 1;
    wire[last] ^= 1;
    rejected(&receiver, "dcz", wire, 400).await;
    let mut header = vec![0x5e, 0x2a, 0x4d, 0x18, 0x20, 0, 0, 0];
    header.extend_from_slice(&dict::sha256(&first));
    header.extend_from_slice(&zstd(&first));
    rejected(&receiver, "dcz", header, 400).await;
}

#[tokio::test]
async fn gzip_is_bounded_and_requires_one_complete_member() {
    let receiver = Receiver::new(config()).unwrap();
    let data = body();
    let wire = gzip(&data);
    assert_eq!(
        decode(&receiver, "gzip", wire.clone(), "alice", true)
            .await
            .request
            .body(),
        &data
    );
    rejected(&receiver, "gzip", wire[..wire.len() - 1].to_vec(), 400).await;
    let mut trailing = wire.clone();
    trailing.extend(wire);
    rejected(&receiver, "gzip", trailing, 400).await;
    rejected(&receiver, "gzip", gzip(&vec![0; 8193]), 413).await;
}

#[tokio::test]
async fn a_context_keeps_only_its_newest_dictionaries() {
    let receiver = Receiver::new(ReceiverConfig {
        dictionaries_per_scope: 2,
        ..config()
    })
    .unwrap();
    let turn = |n: usize| Bytes::from(format!("turn {n} ").repeat(64));
    let other = Bytes::from("another caller ".repeat(64));
    decode(&receiver, "zstd", zstd(&turn(0)), "alice", true).await;
    decode(&receiver, "zstd", zstd(&other), "bob", true).await;
    decode(&receiver, "zstd", zstd(&turn(1)), "alice", true).await;
    decode(&receiver, "zstd", zstd(&turn(2)), "alice", true).await;
    // Alice's third body pushed out her first, not Bob's older one.
    assert_eq!(receiver.snapshot()["dictionary_entries"], 3);
    rejected(&receiver, "dcz", dcz(&turn(3), &turn(0)), 412).await;
    decode(&receiver, "dcz", dcz(&turn(3), &turn(1)), "alice", false).await;
    decode(&receiver, "dcz", dcz(&turn(3), &other), "bob", false).await;
}

#[tokio::test]
async fn a_decoded_body_carries_no_spare_capacity() {
    // Past the decoders' 64KiB step, so the buffer has grown beyond it.
    let plain = Bytes::from("x".repeat(100_000));
    let receiver = Receiver::new(ReceiverConfig::default()).unwrap();
    for (coding, wire) in [("zstd", zstd(&plain)), ("gzip", gzip(&plain))] {
        let (request, _) = decode(&receiver, coding, wire, "alice", false)
            .await
            .into_parts();
        let body = request.into_body().try_into_mut().expect("sole reference");
        assert_eq!(body.capacity(), plain.len(), "{coding}");
    }
}

#[tokio::test]
async fn eviction_and_disabled_storage_do_not_acknowledge_unavailable_bases() {
    let mut cfg = config();
    cfg.dictionary_bytes = 1400;
    let receiver = Receiver::new(cfg).unwrap();
    let first = body();
    decode(&receiver, "zstd", zstd(&first), "alice", true).await;
    let second = Bytes::from("different context ".repeat(64));
    decode(&receiver, "zstd", zstd(&second), "alice", true).await;
    let wire = dict::compress(
        &first,
        &dict::Base {
            hash: dict::sha256(&first),
            body: first.clone(),
        },
        3,
    )
    .unwrap();
    rejected(&receiver, "dcz", wire.clone(), 412).await;
    assert!(receiver.snapshot()["dictionary_bytes"].as_u64().unwrap() <= 1400);
    let mut cfg = config();
    cfg.dictionary_bytes = 0;
    let off = Receiver::new(cfg).unwrap();
    assert!(
        !off.capabilities()
            .headers()
            .contains_key("x-request-dictionary")
    );
    rejected(&off, "dcz", wire, 415).await;
    let decoded = decode(&off, "zstd", zstd(&second), "alice", true).await;
    assert!(
        !decoded
            .into_parts()
            .1
            .finish(http::Response::new(()))
            .headers()
            .contains_key("x-dict-stored")
    );
}

#[tokio::test]
async fn plain_and_unrequested_bodies_are_not_stored_and_origin_errors_stay_origin_errors() {
    let receiver = Receiver::new(config()).unwrap();
    for (coding, body, keep) in [
        ("identity", body(), true),
        ("zstd", zstd(&body()).into(), false),
    ] {
        let decoded = decode(&receiver, coding, body, "alice", keep).await;
        let (_, ack) = decoded.into_parts();
        let response = ack.finish(
            http::Response::builder()
                .status(415)
                .header(ERROR_HEADER, "1")
                .header("x-dict-miss", "1")
                .body("origin response")
                .unwrap(),
        );
        assert_eq!(response.status(), 415);
        assert_eq!(response.body(), &"origin response");
        for key in [ERROR_HEADER, "x-dict-miss", "x-dict-stored"] {
            assert!(!response.headers().contains_key(key));
        }
    }
    assert_eq!(receiver.snapshot()["dictionary_entries"], 0);
}
