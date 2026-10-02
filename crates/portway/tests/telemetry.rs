//! The structured events an in-process observer (the `--tui` dashboard) sees.
//!
//! Own test binary, and serialized inside it: the sink is process-wide, so a
//! concurrent test would find another one's events in the channel.

mod common;

use std::sync::mpsc::Receiver;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use common::{
    Health, Reply, USAGE_CACHED, USAGE_COMPLETION, USAGE_PROMPT, USAGE_REASONING, dead_base,
    forwarder, upstream,
};
use portway::forwarder::Coding;
use portway::logfmt::Level;
use portway::telemetry::{self, Event, RequestRecord, Sinks};
use portway::usage::Usage;
use std::sync::Arc;

static EVENTS: OnceLock<Mutex<Receiver<Event>>> = OnceLock::new();
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn events() -> &'static Mutex<Receiver<Event>> {
    EVENTS.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel();
        assert!(
            telemetry::install(Sinks {
                tui: Some(sender),
                ..Sinks::default()
            }),
            "the sink installs once"
        );
        Mutex::new(receiver)
    })
}

/// Everything queued right now. The guard never crosses an await.
fn drain() -> Vec<Event> {
    let receiver = events().lock().unwrap();
    let mut out = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        out.push(event);
    }
    out
}

/// Serializes the tests and hands each one an empty channel.
async fn sink() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().await;
    let _ = drain();
    guard
}

/// The record is emitted when the relay ends, which is just after the client
/// has read the last byte — poll rather than race it.
async fn next_request() -> RequestRecord {
    for _ in 0..400 {
        for event in drain() {
            if let Event::Request(record) = event {
                return (*record).clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no request record reached the sink");
}

fn chat_body(model: &str) -> Bytes {
    Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "에이전트 컨텍스트 ".repeat(5000)}],
        }))
        .unwrap(),
    )
}

#[tokio::test]
async fn a_finished_request_reaches_the_sink_with_both_sides_measured() {
    let _guard = sink().await;
    let up = upstream(Health::Json(vec!["zstd"]), Reply::GzipStream).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;

    let body = chat_body("model-alpha");
    let sent = body.len() as u64;
    fwd.post("/v1/chat/completions", body).await;
    let record = next_request().await;

    assert_eq!(record.model, "model-alpha");
    assert_eq!(record.method, http::Method::POST);
    assert_eq!(record.path, "/v1/chat/completions");
    assert_eq!(record.status, 200);
    assert_eq!(record.coding, Coding::Zstd);
    assert_eq!(record.body_len, sent);
    assert!(record.wire_len < record.body_len, "{record:?}");
    // The response arrived gzipped and left the forwarder identity, so the
    // decoded size has to exceed what came off the wire.
    assert_eq!(record.upstream_encoding, "gzip");
    assert!(record.received > record.received_wire, "{record:?}");
    assert!(record.complete, "a fully read body is complete: {record:?}");
    assert!(record.download.is_some(), "{record:?}");
    // The /health probe left a warm connection behind, so this rode it.
    assert!(record.reused(), "{record:?}");
    assert!(record.handshake().is_none(), "{record:?}");
}

#[tokio::test]
async fn an_agent_abort_leaves_the_record_incomplete() {
    let _guard = sink().await;
    let up = upstream(Health::Json(vec!["zstd"]), Reply::Endless).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;

    fwd.read_then_abort("/v1/chat/completions", chat_body("model-alpha"), 2)
        .await;
    let record = next_request().await;

    assert_eq!(record.status, 200);
    assert!(!record.complete, "an aborted stream is not complete");
    assert!(record.received > 0, "some frames did arrive: {record:?}");
}

/// The counts are lifted out of the answer itself, in both shapes an engine
/// sends them: the body of a buffered answer, and the last chunk of a stream.
#[tokio::test]
async fn the_counts_the_engine_reported_reach_the_record() {
    let _guard = sink().await;
    let buffered = upstream(Health::Json(vec!["zstd"]), Reply::UsageJson).await;
    let streamed = upstream(Health::Json(vec!["zstd"]), Reply::UsageStream).await;
    let fwd = forwarder(
        &[
            ("model-alpha", &buffered.base),
            ("model-zeta", &streamed.base),
        ],
        &[],
    )
    .await;

    let expected = Some(Usage {
        prompt: USAGE_PROMPT,
        cached: Some(USAGE_CACHED),
        completion: USAGE_COMPLETION,
        reasoning: Some(USAGE_REASONING),
    });
    fwd.post("/v1/chat/completions", chat_body("model-alpha"))
        .await;
    assert_eq!(next_request().await.usage, expected, "a buffered answer");
    fwd.post("/v1/chat/completions", chat_body("model-zeta"))
        .await;
    // The streamed reply carries no cache detail, and reports the thinking
    // beside the totals instead of under `completion_tokens_details`.
    assert_eq!(
        next_request().await.usage,
        expected.map(|usage| Usage {
            cached: None,
            ..usage
        }),
        "a streamed answer"
    );
}

#[tokio::test]
async fn the_stats_json_carries_the_live_fields_and_in_flight_settles_at_zero() {
    let _guard = sink().await;
    let up = upstream(Health::Json(vec!["zstd"]), Reply::GzipStream).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;

    let (up_before, down_before) = telemetry::core().socket_bytes();
    fwd.post("/v1/chat/completions", chat_body("model-alpha"))
        .await;
    next_request().await;

    let mine = fwd.get("/__portway/stats").await.json()["upstreams"]["model-alpha"].clone();
    assert_eq!(mine["in_flight"], 0, "{mine}");
    assert_eq!(mine["requests"], 1, "{mine}");
    assert!(mine["down_bytes"].as_u64().unwrap() > 0, "{mine}");
    assert!(
        mine["down_bytes"].as_u64().unwrap() > mine["down_wire_bytes"].as_u64().unwrap(),
        "the gzipped response decodes larger: {mine}"
    );
    assert!(mine["saved_bytes"].as_i64().unwrap() > 0, "{mine}");

    let (up_after, down_after) = telemetry::core().socket_bytes();
    assert!(up_after > up_before, "upstream writes are counted");
    assert!(down_after > down_before, "upstream reads are counted");
}

#[tokio::test]
async fn log_records_go_to_the_sink_instead_of_stderr() {
    let _guard = sink().await;
    // An upstream nothing listens on: negotiation fails and warns.
    let _fwd = forwarder(&[("model-zeta", &dead_base())], &[]).await;

    let warning = drain()
        .into_iter()
        .find_map(|event| match event {
            Event::Log {
                level: Level::Warning,
                message,
                ..
            } => Some(message),
            _ => None,
        })
        .expect("the failed probe warns through the sink");
    assert!(warning.contains("/health.request_encodings"), "{warning}");
}

/// The record keeps the route and the model apart: the route is where the
/// request went, the model is what its body asked for. Under a single
/// upstream the route is `upstream` and the model is still the model, so a
/// price table keyed by model name applies; under a mount a request that
/// names no model records none.
#[tokio::test]
async fn a_record_names_its_route_and_the_model_the_body_asked_for() {
    let _serial = sink().await;
    let up = upstream(Health::JsonBare, Reply::Ok).await;

    let fwd = common::single(&up.base, &[]).await;
    fwd.post("/v1/messages", chat_body("claude-3")).await;
    let record = next_request().await;
    assert_eq!(
        (record.upstream.as_str(), record.model.as_str()),
        ("upstream", "claude-3")
    );

    let fwd = common::router(&[("codex", &up.base)], &[("model-alpha", &up.base)], &[]).await;
    fwd.post("/codex/responses", chat_body("gpt-x")).await;
    let record = next_request().await;
    assert_eq!(
        (record.upstream.as_str(), record.model.as_str()),
        ("codex", "gpt-x")
    );
    fwd.get("/codex/models?client_version=1").await;
    let record = next_request().await;
    assert_eq!(
        (record.upstream.as_str(), record.model.as_str()),
        ("codex", "")
    );
    assert_eq!(record.path, "/models");
    fwd.post("/v1/chat/completions", chat_body("model-alpha"))
        .await;
    let record = next_request().await;
    assert_eq!(
        (record.upstream.as_str(), record.model.as_str()),
        ("model-alpha", "model-alpha")
    );
    let flights = Arc::new(portway::flights::Flights::default());
    let flight = flights.begin("codex", "gpt-x", &http::Method::POST, "/responses", 1);
    assert_eq!(
        (
            flight.view().upstream.as_str(),
            flight.view().model.as_str()
        ),
        ("codex", "gpt-x")
    );
}

/// The `service_tier` a body names rides beside its model, through a mount
/// and through a `[models]` route alike; a body that names none, as a vendor's
/// standard class does, records none.
#[tokio::test]
async fn a_record_keeps_the_service_tier_the_body_asked_for() {
    let _serial = sink().await;
    let up = upstream(Health::JsonBare, Reply::Ok).await;
    let fwd = common::router(&[("codex", &up.base)], &[("model-alpha", &up.base)], &[]).await;
    let tiered = |model: &str, tier: &str| {
        Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": model,
                "service_tier": tier,
                "input": "에이전트 컨텍스트 ".repeat(5000),
            }))
            .unwrap(),
        )
    };

    fwd.post("/codex/responses", tiered("gpt-x", "tier-a"))
        .await;
    let record = next_request().await;
    assert_eq!(
        (record.model.as_str(), record.service_tier.as_deref()),
        ("gpt-x", Some("tier-a"))
    );
    fwd.post("/codex/responses", chat_body("gpt-x")).await;
    assert_eq!(next_request().await.service_tier, None);
    fwd.post("/v1/chat/completions", tiered("model-alpha", "tier-b"))
        .await;
    let record = next_request().await;
    assert_eq!(
        (record.model.as_str(), record.service_tier.as_deref()),
        ("model-alpha", Some("tier-b"))
    );
}

/// An agent that stops reading at the answer's last event — Codex closes on
/// `response.completed` — leaves the upstream's EOF unread. The relay reads
/// on for a moment, so the answer is recorded whole, with its usage, and its
/// connection goes back to the pool. One whose body stays open past that
/// grace is cut, as an abort is.
#[tokio::test]
async fn an_answer_the_agent_stopped_reading_at_its_last_event_is_still_whole() {
    let _serial = sink().await;
    let up = upstream(Health::JsonBare, Reply::UsageStreamLingers(100)).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;
    let after_probe = up.connections();
    fwd.read_then_abort("/v1/chat/completions", chat_body("model-alpha"), 1)
        .await;
    let record = next_request().await;
    assert!(record.complete, "{record:?}");
    assert_eq!(record.usage.map(|usage| usage.prompt), Some(USAGE_PROMPT));
    // Read to its end, the connection is pooled: the next request dials nothing.
    fwd.post("/v1/chat/completions", chat_body("model-alpha"))
        .await;
    let _ = next_request().await;
    assert_eq!(up.connections(), after_probe);

    let up = upstream(Health::JsonBare, Reply::UsageStreamLingers(5_000)).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;
    fwd.read_then_abort("/v1/chat/completions", chat_body("model-alpha"), 1)
        .await;
    let record = next_request().await;
    assert!(!record.complete, "{record:?}");
    assert!(record.usage.is_none(), "{record:?}");
}
