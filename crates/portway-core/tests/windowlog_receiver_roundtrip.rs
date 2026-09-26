//! The sender's window must always be one the receiver accepts, at every
//! context size the sender is willing to use as a base.
//!
//! `dict::compress` sets `windowLog = (base + body).ilog2() + 1`, capped at
//! `MAX_WINDOW_BYTES.ilog2()`. The receiver sets `WindowLogMax` to that same cap
//! and refuses a wider frame before the application runs, so the two must agree
//! at the boundary.
//!
//! This drives the real protocol: turn 1 goes out as plain zstd with
//! `X-Dict-Store: 1` so the receiver keeps it, and turn 2 is framed as `dcz`
//! against that exact body. Before the window fix, a context past the level's
//! default window inflated its frame back to the body's own size here; the
//! decode still succeeded, which is why only the ratio ever noticed.
use bytes::Bytes;
use http::Request;
use http_body_util::Full;
use portway_core::dict;
use portway_core::{Receiver, ReceiverConfig};
use std::sync::Arc;

fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 56) as u8
        })
        .collect()
}

fn request(coding: &str, body: impl Into<Bytes>, keep: bool) -> Request<Full<Bytes>> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-encoding", coding)
        .header("content-type", "application/json");
    if keep {
        request = request.header("x-dict-store", "1");
    }
    request.body(Full::new(body.into())).unwrap()
}

async fn decode(
    receiver: &Arc<Receiver>,
    coding: &str,
    body: impl Into<Bytes>,
    keep: bool,
) -> Bytes {
    match receiver.decode(request(coding, body, keep)).await {
        Ok(decoded) => decoded.request.into_body(),
        Err(e) => panic!("the receiver refused a {coding} frame: {}", e.status()),
    }
}

/// Contexts past the level's default window (4 MiB at level 11) must still
/// round-trip, and the frame must stay small — a frame that tracks the body's
/// own size means the dictionary's far end is out of the window.
#[tokio::test]
async fn a_large_context_round_trips_and_still_compresses() {
    for kb in [1024usize, 4096, 8192] {
        let receiver = Receiver::new(ReceiverConfig::default()).expect("receiver");
        let first = noise(kb * 1024, 1);

        // Turn 1: plain zstd, asking the receiver to keep the decoded body.
        let stored = decode(
            &receiver,
            "zstd",
            zstd::bulk::compress(&first, 11).expect("frame"),
            true,
        )
        .await;
        assert_eq!(stored.as_ref(), first.as_slice(), "{kb} KiB turn 1");

        // Turn 2: the previous body plus what changed, framed against it.
        let mut second = first.clone();
        second.extend_from_slice(&noise(16 * 1024, 77));
        let base = dict::Base {
            hash: dict::sha256(&first),
            body: first.clone().into(),
        };
        let wire = dict::compress(&second, &base, 11).expect("dcz frame");

        // The window the sender picks must be one the receiver accepts.
        assert!(
            wire.len() < 64 * 1024,
            "{kb} KiB: frame is {} B — the far end of the base is out of the window",
            wire.len()
        );

        let restored = decode(&receiver, dict::ADVERTISED_AS, wire, false).await;
        assert_eq!(restored.as_ref(), second.as_slice(), "{kb} KiB round trip");
    }
}
