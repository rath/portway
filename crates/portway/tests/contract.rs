//! Contract tests for the forwarder: routing, compression negotiation, header
//! and body preservation, and abort propagation. CPU only — the two ends are
//! loopback sockets, no upstream and no network.

mod common;

use bytes::Bytes;
use common::{Health, Reply, dead_base, forwarder, upstream};

/// Big enough to clear --min-bytes and compress well, like a real agent turn.
fn big() -> Bytes {
    let content = "에이전트 컨텍스트 ".repeat(5000);
    Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "model-alpha",
            "messages": [{"role": "user", "content": content}],
        }))
        .unwrap(),
    )
}

fn small() -> Bytes {
    Bytes::from(r#"{"model":"model-alpha","messages":[]}"#)
}

fn body_for(model: &str) -> Bytes {
    Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "x".repeat(5000)}],
            "reasoning_effort": "low",
            "stream": true,
        }))
        .unwrap(),
    )
}

#[tokio::test]
async fn large_bodies_are_zstd_encoded_small_and_preencoded_are_not() {
    let up = upstream(Health::Json(vec!["zstd", "gzip"]), Reply::Ok).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;

    let answer = fwd
        .send(
            "POST",
            "/v1/chat/completions?x=1",
            &[
                ("authorization", "Bearer agent-key"),
                ("accept-encoding", "br"),
                ("x-custom", "1"),
            ],
            big(),
        )
        .await;
    assert_eq!(answer.json(), serde_json::json!({"ok": true}));

    let sent = up.last();
    assert_eq!(sent.path, "/v1/chat/completions");
    assert_eq!(sent.query, "x=1");
    assert_eq!(sent.header("content-encoding"), Some("zstd"));
    assert_eq!(
        zstd::bulk::decompress(&sent.body, 50 << 20).unwrap(),
        big().to_vec()
    );
    assert_eq!(
        sent.header("content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        sent.body.len()
    );
    assert!(sent.body.len() < big().len() / 4);
    // Auth passes through untouched; the forwarder never reads or swaps keys.
    assert_eq!(sent.header("authorization"), Some("Bearer agent-key"));
    assert_eq!(sent.header("x-custom"), Some("1"));
    assert!(sent.header("host").unwrap().starts_with("127.0.0.1:"));
    // The upstream leg negotiates its own accept-encoding.
    assert!(!sent.header("accept-encoding").unwrap().contains("br"));

    fwd.post("/v1/chat/completions", small()).await;
    assert!(!up.last().has("content-encoding"));
    assert_eq!(up.last().body, small());

    // A body the agent framed itself keeps its coding and is never re-encoded.
    // (A real coding is rejected by the router with a 415; identity is the one
    // value that reaches here.)
    fwd.send(
        "POST",
        "/v1/chat/completions",
        &[("content-encoding", "identity")],
        big(),
    )
    .await;
    assert_eq!(up.last().header("content-encoding"), Some("identity"));
    assert_eq!(up.last().body, big());

    let stats = fwd.get("/__portway/stats").await.json();
    let mine = &stats["models"]["model-alpha"];
    assert_eq!(mine["coding"], "zstd");
    assert_eq!(mine["encoded_requests"], 1);
    assert_eq!(mine["requests"], 3);
    assert!(mine["saved_bytes"].as_i64().unwrap() > (big().len() as i64) * 7 / 10);
    // The stats path never reaches the upstream.
    assert_eq!(up.calls().len(), 3);
}

#[tokio::test]
async fn coding_is_negotiated_from_health() {
    let cases: Vec<(Health, Vec<&str>, Option<&str>)> = vec![
        (Health::Json(vec!["zstd", "gzip"]), vec![], Some("zstd")),
        // The edge compresses /health too: the probe has to decode it before
        // it can read request_encodings (JSON capability endpoint, live).
        (Health::GzipJson(vec!["zstd", "gzip"]), vec![], Some("zstd")),
        (Health::GzipJson(vec!["gzip"]), vec![], Some("gzip")),
        (Health::Json(vec!["gzip"]), vec![], Some("gzip")),
        // An SGLang target: /health has no such field.
        (Health::JsonBare, vec![], None),
        (Health::Json(vec![]), vec![], None),
        (
            Health::Json(vec!["zstd", "gzip"]),
            vec!["--coding", "gzip"],
            Some("gzip"),
        ),
        // Never send what was not advertised.
        (Health::Json(vec!["gzip"]), vec!["--coding", "zstd"], None),
        (
            Health::Json(vec!["zstd", "gzip"]),
            vec!["--coding", "off"],
            None,
        ),
        // Plaintext /health (SGLang) advertises in a header.
        (
            Health::Plaintext(Some(vec!["zstd", "gzip"])),
            vec![],
            Some("zstd"),
        ),
        (Health::Plaintext(Some(vec!["gzip"])), vec![], Some("gzip")),
        // Header absent, and header present but empty.
        (Health::Plaintext(None), vec![], None),
        (Health::Plaintext(Some(vec![])), vec![], None),
        // A configured legacy fallback must not hide Portway receivers.
        (
            Health::Portway(vec!["zstd", "gzip"]),
            vec!["--probe-path", "/health"],
            Some("zstd"),
        ),
    ];

    for (health, argv, expected) in cases {
        let up = upstream(health, Reply::Ok).await;
        let fwd = forwarder(&[("model-alpha", &up.base)], &argv).await;
        fwd.post("/v1/chat/completions", big()).await;
        let sent = up.last();
        assert_eq!(sent.header("content-encoding"), expected, "argv {argv:?}");
        match expected {
            Some("zstd") => assert_eq!(
                zstd::bulk::decompress(&sent.body, 50 << 20).unwrap(),
                big().to_vec()
            ),
            Some("gzip") => {
                use std::io::Read;
                let mut out = Vec::new();
                flate2::read::GzDecoder::new(&sent.body[..])
                    .read_to_end(&mut out)
                    .unwrap();
                assert_eq!(out, big().to_vec());
            }
            _ => assert_eq!(sent.body, big()),
        }
    }
}

#[tokio::test]
async fn a_415_is_retried_identity_once_and_turns_encoding_off() {
    let up = upstream(Health::Json(vec!["zstd"]), Reply::RejectEncoded).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;

    assert_eq!(fwd.post("/v1/chat/completions", big()).await.status, 200);
    let encoded: Vec<bool> = up
        .calls()
        .iter()
        .map(|c| c.has("content-encoding"))
        .collect();
    assert_eq!(encoded, vec![true, false]);
    assert_eq!(up.last().body, big());

    // Encoding stays off for this model from now on: one retry, never two.
    assert_eq!(fwd.post("/v1/chat/completions", big()).await.status, 200);
    assert_eq!(up.calls().len(), 3);
    assert!(!up.last().has("content-encoding"));

    let mine = fwd.get("/__portway/stats").await.json()["models"]["model-alpha"].clone();
    assert_eq!(mine["retried_identity"], 1);
    assert_eq!(mine["coding"], serde_json::Value::Null);
    assert_eq!(mine["encoded_requests"], 0);
    assert_eq!(mine["identity_reason"], "encoding_refused");
    assert!((1..=600).contains(&mine["identity_backoff_secs"].as_u64().unwrap()));
    assert_eq!(mine["last_probe_ok"], true);
}

#[tokio::test]
async fn compressed_responses_reach_the_agent_as_identity() {
    let up = upstream(Health::JsonBare, Reply::GzipStream).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;
    let answer = fwd
        .send(
            "GET",
            "/gzip?model=model-alpha",
            &[("accept-encoding", "identity")],
            Bytes::new(),
        )
        .await;
    assert_eq!(answer.body, Bytes::from(b"data: hello\n\n".repeat(200)));
    assert_eq!(answer.header("content-encoding"), None);
    assert!(
        answer
            .header("content-type")
            .unwrap()
            .starts_with("text/event-stream")
    );
}

#[tokio::test]
async fn chunked_zstd_with_idle_gaps_decodes_incrementally() {
    let up = upstream(Health::JsonBare, Reply::ZstdChunks).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;
    let answer = fwd.get("/sse?model=model-alpha").await;
    assert_eq!(answer.body, Bytes::from("data: one\n\ndata: two\n\n"));
    assert_eq!(answer.header("content-encoding"), None);
}

#[tokio::test]
async fn upstream_failure_is_a_502_not_a_crash() {
    let fwd = forwarder(&[("model-alpha", &dead_base())], &[]).await;
    let answer = fwd.post("/v1/chat/completions", small()).await;
    assert_eq!(answer.status, 502);
    assert!(
        answer.json()["detail"]
            .as_str()
            .unwrap()
            .contains("portway")
    );
    let mine = fwd.get("/__portway/stats").await.json()["models"]["model-alpha"].clone();
    assert_eq!(mine["upstream_errors"], 1);
}

#[tokio::test]
async fn the_router_lists_all_models_and_routes_the_original_body_and_auth() {
    let mut origins = Vec::new();
    for _ in common::MODEL_UPSTREAMS {
        origins.push(upstream(Health::JsonBare, Reply::Ok).await);
    }
    let names: Vec<&str> = common::MODEL_UPSTREAMS
        .iter()
        .map(|(model, _)| *model)
        .collect();
    let upstreams: Vec<(&str, &str)> = names
        .iter()
        .zip(origins.iter())
        .map(|(model, up)| (*model, up.base.as_str()))
        .collect();
    let fwd = forwarder(&upstreams, &[]).await;

    let models = fwd.get("/v1/models").await.json();
    assert_eq!(models["object"], "list");
    assert_eq!(
        models["data"].as_array().unwrap().len(),
        common::MODEL_UPSTREAMS.len()
    );
    let listed: Vec<String> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listed, names);

    assert_eq!(
        fwd.get("/__portway/health").await.json(),
        serde_json::json!({"status": "ok", "mode": "router"})
    );
    assert!(origins.iter().all(|up| up.calls().is_empty()));

    for (model, up) in names.iter().zip(origins.iter()) {
        let body = body_for(model);
        let answer = fwd
            .send(
                "POST",
                "/v1/chat/completions?x=1",
                &[("authorization", "Bearer agent-key")],
                body.clone(),
            )
            .await;
        assert_eq!(answer.status, 200, "{model}");
        let sent = up.last();
        assert_eq!(sent.path, "/v1/chat/completions");
        assert_eq!(sent.query, "x=1");
        // Body bytes pass through untouched: no rewriting of sampling,
        // reasoning, tool or multimodal fields.
        assert_eq!(sent.body, body);
        assert_eq!(sent.header("authorization"), Some("Bearer agent-key"));
    }

    let stats = fwd.get("/__portway/stats").await.json();
    let models = stats["models"].as_object().unwrap();
    assert_eq!(models.len(), common::MODEL_UPSTREAMS.len());
    assert!(models.values().all(|s| s["requests"] == 1));
}

#[tokio::test]
async fn the_router_rejects_an_invalid_or_missing_model_without_forwarding() {
    for body in [
        &b"{"[..],
        b"[]",
        b"null",
        b"{}",
        br#"{"model": []}"#,
        br#"{"model": "unknown"}"#,
        br#"{"model": null}"#,
        b"\xff",
    ] {
        let up = upstream(Health::JsonBare, Reply::Ok).await;
        let fwd = forwarder(&[("model-zeta", &up.base)], &[]).await;
        let answer = fwd
            .post(
                "/v1/chat/completions?model=model-zeta",
                Bytes::from(body.to_vec()),
            )
            .await;
        assert_eq!(answer.status, 400, "{body:?}");
        assert_eq!(answer.json()["error"]["type"], "invalid_request_error");
        // A query parameter must never rescue a body that names no model.
        assert!(up.calls().is_empty());
    }
}

/// The 400 says which name was rejected, so a wrong `model` field can be fixed
/// from the error alone. A request that names no model gets the plain prompt.
#[tokio::test]
async fn an_unknown_model_is_named_in_the_error() {
    let up = upstream(Health::JsonBare, Reply::Ok).await;
    let fwd = forwarder(&[("model-zeta", &up.base)], &[]).await;

    let answer = fwd
        .post("/v1/chat/completions", body_for("no-such-model"))
        .await;
    assert_eq!(answer.status, 400);
    let error = answer.json();
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains(r#""no-such-model""#), "{message}");
    assert!(message.contains("model-zeta"), "{message}");
    assert!(up.calls().is_empty(), "nothing was forwarded");

    let answer = fwd
        .post("/v1/chat/completions", Bytes::from_static(b"{}"))
        .await;
    let error = answer.json();
    let message = error["error"]["message"].as_str().unwrap();
    assert!(!message.contains("Unknown"), "{message}");
    assert!(message.contains("model-zeta"), "{message}");
}

#[tokio::test]
async fn the_router_rejects_a_preencoded_body_and_an_ambiguous_get() {
    let up = upstream(Health::Json(vec!["zstd"]), Reply::Ok).await;
    let fwd = forwarder(&[("model-gamma", &up.base)], &[]).await;

    let answer = fwd
        .send(
            "POST",
            "/v1/chat/completions",
            &[("content-encoding", "gzip")],
            Bytes::from(common::gzip(&big())),
        )
        .await;
    assert_eq!(answer.status, 415);

    assert_eq!(fwd.get("/metrics-tiny").await.status, 400);
    assert!(up.calls().is_empty());

    assert_eq!(fwd.get("/metrics-tiny?model=model-gamma").await.status, 200);
    assert_eq!(up.last().path, "/metrics-tiny");
}

#[tokio::test]
async fn the_router_isolates_encoding_fallback_and_upstream_failure() {
    let flash = upstream(Health::Json(vec!["zstd"]), Reply::RejectEncoded).await;
    let other_model = upstream(Health::Json(vec!["zstd"]), Reply::Ok).await;
    let fwd = forwarder(
        &[
            ("model-gamma", &dead_base()),
            ("model-alpha", &flash.base),
            ("model-zeta", &other_model.base),
        ],
        &[],
    )
    .await;

    assert_eq!(
        fwd.post("/v1/chat/completions", body_for("model-gamma"))
            .await
            .status,
        502
    );
    assert_eq!(
        fwd.post("/v1/chat/completions", body_for("model-alpha"))
            .await
            .status,
        200
    );
    assert_eq!(
        fwd.post("/v1/chat/completions", body_for("model-zeta"))
            .await
            .status,
        200
    );

    let stats = fwd.get("/__portway/stats").await.json();
    let models = &stats["models"];
    assert_eq!(models["model-alpha"]["coding"], serde_json::Value::Null);
    assert_eq!(models["model-alpha"]["retried_identity"], 1);
    assert_eq!(models["model-zeta"]["coding"], "zstd");
    assert_eq!(models["model-zeta"]["encoded_requests"], 1);
    assert_eq!(models["model-gamma"]["upstream_errors"], 1);
    // The healthy upstreams were untouched by the broken one.
    assert!(!flash.last().has("content-encoding"));
    assert_eq!(other_model.last().header("content-encoding"), Some("zstd"));
}

#[tokio::test]
async fn an_agent_abort_closes_the_upstream_connection() {
    let up = upstream(Health::JsonBare, Reply::Endless).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;

    fwd.read_then_abort("/v1/chat/completions", body_for("model-alpha"), 3)
        .await;

    // Closing an unfinished HTTP/1.1 response closes its connection, and that
    // disconnect is what makes the engine abort the generation.
    for _ in 0..200 {
        if up.aborted() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the upstream response body was never dropped");
}

#[tokio::test]
async fn a_completed_response_keeps_the_connection_but_an_aborted_one_does_not() {
    let up = upstream(Health::JsonBare, Reply::Ok).await;
    let fwd = forwarder(&[("model-alpha", &up.base)], &[]).await;
    // The /health probe dialed the only connection there should ever be.
    let after_probe = up.connections();
    for _ in 0..3 {
        assert_eq!(fwd.post("/v1/chat/completions", small()).await.status, 200);
    }
    assert_eq!(
        up.connections(),
        after_probe,
        "fully read responses must leave the connection pooled"
    );
}

#[tokio::test]
async fn an_aborted_stream_does_not_poison_the_pool() {
    let endless = upstream(Health::JsonBare, Reply::Endless).await;
    let fwd = forwarder(&[("model-alpha", &endless.base)], &[]).await;
    fwd.read_then_abort("/v1/chat/completions", body_for("model-alpha"), 2)
        .await;
    // The next request must not be handed the connection that was torn down.
    let answer = fwd.read_then_abort("/v1/chat/completions", body_for("model-alpha"), 2);
    tokio::time::timeout(std::time::Duration::from_secs(5), answer)
        .await
        .expect("a request after an abort must still be served");
}

// ------------------------------------------------- previous-body dictionaries

/// A conversation as the agent re-uploads it: every turn is the previous body
/// plus one message. The content is noise so plain zstd cannot shrink it and
/// the sizes below can only come from the dictionary.
fn conversation(turns: usize) -> Bytes {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut messages = Vec::new();
    for turn in 0..turns {
        let content: String = (0..40_000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                char::from(b'a' + (seed % 26) as u8)
            })
            .collect();
        messages.push(serde_json::json!({"role": "user", "content": format!("{turn} {content}")}));
    }
    Bytes::from(
        serde_json::to_vec(&serde_json::json!({"model": "model-delta", "messages": messages}))
            .unwrap(),
    )
}

const DICT_MODEL: &str = "model-delta";

#[tokio::test]
async fn the_second_turn_goes_out_against_the_first() {
    let up = upstream(Health::PlaintextDict(vec!["zstd", "gzip"]), Reply::Dict).await;
    let fwd = forwarder(&[(DICT_MODEL, &up.base)], &[]).await;

    // Turn 1: nothing confirmed yet, so plain zstd, and the upstream is asked to keep it.
    fwd.post("/v1/chat/completions", conversation(1)).await;
    let first = up.last();
    assert_eq!(first.header("content-encoding"), Some("zstd"));
    assert_eq!(first.header("x-dict-store"), Some("1"));
    assert_eq!(first.inflated, conversation(1));

    // Turn 2: the RFC 9842 stream, naming turn 1 by hash.
    let answer = fwd.post("/v1/chat/completions", conversation(2)).await;
    assert_eq!(answer.json(), serde_json::json!({"ok": true}));
    // The bookkeeping headers are between the forwarder and the upstream.
    assert!(answer.header("x-dict-stored").is_none());
    let second = up.last();
    assert_eq!(second.header("content-encoding"), Some("dcz"));
    assert_eq!(second.header("x-dict-store"), Some("1"));
    assert_eq!(
        second.body[..8],
        [0x5e, 0x2a, 0x4d, 0x18, 0x20, 0x00, 0x00, 0x00]
    );
    assert_eq!(
        common::sha256_hex(&conversation(1)),
        second.body[8..40]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    assert_eq!(second.inflated, conversation(2));
    assert_eq!(
        second
            .header("content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        second.body.len()
    );
    // 40KB of new noise on top of 40KB already held: about half, not all of it.
    assert!(second.body.len() < conversation(2).len() * 6 / 10);

    // Turn 3 chains off turn 2, the longer shared prefix.
    fwd.post("/v1/chat/completions", conversation(3)).await;
    let third = up.last();
    assert_eq!(third.header("content-encoding"), Some("dcz"));
    assert_eq!(third.inflated, conversation(3));
    assert!(third.body.len() < conversation(3).len() * 4 / 10);

    // Small bodies stay out of it entirely.
    let tiny = Bytes::from(format!(r#"{{"model":"{DICT_MODEL}","messages":[]}}"#));
    fwd.post("/v1/chat/completions", tiny.clone()).await;
    assert_eq!(up.last().body, tiny);
    assert!(!up.last().has("content-encoding"));
    assert!(!up.last().has("x-dict-store"));

    let stats = fwd.get("/__portway/stats").await.json();
    let mine = &stats["models"][DICT_MODEL];
    assert_eq!(mine["coding"], "zstd");
    assert_eq!(mine["dict"], true);
    assert_eq!(mine["dict_hits"], 2);
    assert_eq!(mine["dict_misses"], 0);
    assert_eq!(mine["encoded_requests"], 3);
}

#[tokio::test]
async fn dictionaries_need_the_advertisement_and_the_flag() {
    // Same upstream behaviour, but /health does not advertise dcz.
    let plain = upstream(Health::Plaintext(Some(vec!["zstd", "gzip"])), Reply::Dict).await;
    let fwd = forwarder(&[(DICT_MODEL, &plain.base)], &[]).await;
    for turn in 1..=2 {
        fwd.post("/v1/chat/completions", conversation(turn)).await;
        assert_eq!(plain.last().header("content-encoding"), Some("zstd"));
        assert!(!plain.last().has("x-dict-store"));
    }

    let up = upstream(Health::PlaintextDict(vec!["zstd", "gzip"]), Reply::Dict).await;
    let off = forwarder(&[(DICT_MODEL, &up.base)], &["--dict", "off"]).await;
    for turn in 1..=2 {
        off.post("/v1/chat/completions", conversation(turn)).await;
        assert_eq!(up.last().header("content-encoding"), Some("zstd"));
        assert!(!up.last().has("x-dict-store"));
    }
    let stats = off.get("/__portway/stats").await.json();
    assert_eq!(stats["models"][DICT_MODEL]["dict"], false);

    // The miss path resends as zstd, so a gzip-only upstream gets no dictionaries.
    let gzip_only = upstream(Health::PlaintextDict(vec!["gzip"]), Reply::Dict).await;
    let fwd = forwarder(&[(DICT_MODEL, &gzip_only.base)], &[]).await;
    fwd.post("/v1/chat/completions", conversation(1)).await;
    let sent = &gzip_only.calls()[0];
    assert_eq!(sent.header("content-encoding"), Some("gzip"));
    assert!(!sent.has("x-dict-store"));
}

#[tokio::test]
async fn a_missing_dictionary_costs_one_resend_as_zstd() {
    let up = upstream(
        Health::PlaintextDict(vec!["zstd", "gzip"]),
        Reply::DictAmnesia,
    )
    .await;
    let fwd = forwarder(&[(DICT_MODEL, &up.base)], &[]).await;

    fwd.post("/v1/chat/completions", conversation(1)).await;
    let answer = fwd.post("/v1/chat/completions", conversation(2)).await;
    // The agent never sees the 412.
    assert_eq!(answer.status, 200);
    assert_eq!(answer.json(), serde_json::json!({"ok": true}));

    let calls = up.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].header("content-encoding"), Some("dcz"));
    assert_eq!(calls[2].header("content-encoding"), Some("zstd"));
    // Still asked to keep it: the next turn can use a dictionary again.
    assert_eq!(calls[2].header("x-dict-store"), Some("1"));
    assert_eq!(calls[2].inflated, conversation(2));

    let stats = fwd.get("/__portway/stats").await.json();
    let mine = &stats["models"][DICT_MODEL];
    assert_eq!(mine["dict"], true);
    assert_eq!(mine["dict_hits"], 0);
    assert_eq!(mine["dict_misses"], 1);
    assert_eq!(mine["coding"], "zstd");
    assert_eq!(mine["retried_identity"], 0);
}

/// The edge splits a small refusal body from its response head (TLS record
/// boundaries), which leaves the refusal connection holding an unread body.
/// The miss path must not hand that connection to the zstd resend: hyper
/// does not write the next request until the previous response body is
/// consumed, and the old response is only dropped once the resend returns —
/// a deadlock when the resend reuses the pooled refusal connection. Found
/// live: after an upstream recreate, every dcz miss hung the agent at
/// 'Working...' forever (in_flight 1, no upstream error, the upstream never saw
/// the resend).
#[tokio::test]
async fn a_dict_miss_with_a_split_refusal_body_still_resends() {
    let up = upstream(
        Health::PlaintextDict(vec!["zstd", "gzip"]),
        Reply::DictAmnesiaSplit,
    )
    .await;
    let fwd = forwarder(&[(DICT_MODEL, &up.base)], &[]).await;

    fwd.post("/v1/chat/completions", conversation(1)).await;
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fwd.post("/v1/chat/completions", conversation(2)),
    )
    .await
    .expect("the zstd resend must not wait on the pooled refusal connection");

    assert_eq!(answer.status, 200);
    assert_eq!(answer.json(), serde_json::json!({"ok": true}));
    let calls = up.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].header("content-encoding"), Some("dcz"));
    assert_eq!(calls[2].header("content-encoding"), Some("zstd"));
    assert_eq!(calls[2].inflated, conversation(2));
}

#[tokio::test]
async fn a_rolled_back_upstream_loses_dictionaries_but_keeps_zstd() {
    let up = upstream(
        Health::PlaintextDict(vec!["zstd", "gzip"]),
        Reply::DictRolledBack,
    )
    .await;
    let fwd = forwarder(&[(DICT_MODEL, &up.base)], &[]).await;

    fwd.post("/v1/chat/completions", conversation(1)).await;
    let answer = fwd.post("/v1/chat/completions", conversation(2)).await;
    assert_eq!(answer.status, 200);
    let calls = up.calls();
    assert_eq!(calls[1].header("content-encoding"), Some("dcz"));
    assert_eq!(calls[2].header("content-encoding"), Some("zstd"));
    assert!(!calls[2].has("x-dict-store"));
    assert_eq!(calls[2].inflated, conversation(2));

    // During backoff: no more dcz or store requests, zstd untouched.
    fwd.post("/v1/chat/completions", conversation(3)).await;
    assert_eq!(up.last().header("content-encoding"), Some("zstd"));
    assert!(!up.last().has("x-dict-store"));
    let stats = fwd.get("/__portway/stats").await.json();
    let mine = &stats["models"][DICT_MODEL];
    assert_eq!(mine["dict"], false);
    assert_eq!(mine["coding"], "zstd");
    assert_eq!(mine["retried_identity"], 0);
    assert_eq!(mine["identity_reason"], serde_json::Value::Null);
    assert_eq!(mine["dict_backoff_reason"], "dictionary_refused");
    assert!((1..=600).contains(&mine["dict_backoff_secs"].as_u64().unwrap()));
}

#[tokio::test]
async fn a_upstream_that_stored_other_bytes_is_never_used_as_a_dictionary() {
    let up = upstream(
        Health::PlaintextDict(vec!["zstd", "gzip"]),
        Reply::DictWrongHash,
    )
    .await;
    let fwd = forwarder(&[(DICT_MODEL, &up.base)], &[]).await;

    fwd.post("/v1/chat/completions", conversation(1)).await;
    fwd.post("/v1/chat/completions", conversation(2)).await;
    assert_eq!(up.last().header("content-encoding"), Some("zstd"));
    assert!(!up.last().has("x-dict-store"));
    let stats = fwd.get("/__portway/stats").await.json();
    assert_eq!(stats["models"][DICT_MODEL]["dict"], false);
    assert_eq!(stats["models"][DICT_MODEL]["dict_hits"], 0);
    assert_eq!(stats["models"][DICT_MODEL]["coding"], "zstd");
    assert_eq!(stats["models"][DICT_MODEL]["dict_hash_mismatches"], 1);
    assert_eq!(
        stats["models"][DICT_MODEL]["dict_backoff_reason"],
        "hash_mismatch"
    );
}

#[tokio::test]
async fn a_400_for_dcz_is_retried_without_the_dictionary() {
    let up = upstream(
        Health::PlaintextDict(vec!["zstd", "gzip"]),
        Reply::DictRejects,
    )
    .await;
    let fwd = forwarder(&[(DICT_MODEL, &up.base)], &[]).await;

    fwd.post("/v1/chat/completions", conversation(1)).await;
    let answer = fwd.post("/v1/chat/completions", conversation(2)).await;
    assert_eq!(answer.status, 200);
    let calls = up.calls();
    assert_eq!(calls[1].header("content-encoding"), Some("dcz"));
    assert_eq!(calls[2].header("content-encoding"), Some("zstd"));
    assert_eq!(calls[2].inflated, conversation(2));
    let stats = fwd.get("/__portway/stats").await.json();
    assert_eq!(stats["models"][DICT_MODEL]["dict_misses"], 1);
}

#[tokio::test]
async fn a_body_the_agent_encoded_itself_is_never_stored_or_rebased() {
    let up = upstream(Health::PlaintextDict(vec!["zstd", "gzip"]), Reply::Dict).await;
    let fwd = forwarder(&[(DICT_MODEL, &up.base)], &[]).await;

    fwd.post("/v1/chat/completions", conversation(1)).await;
    fwd.send(
        "POST",
        "/v1/chat/completions",
        &[("content-encoding", "identity"), ("x-dict-store", "1")],
        conversation(2),
    )
    .await;
    let sent = up.last();
    assert_eq!(sent.header("content-encoding"), Some("identity"));
    assert_eq!(sent.body, conversation(2));
    // Asking the upstream to keep a body is the forwarder's decision alone.
    assert!(!sent.has("x-dict-store"));
}

// -------------------------------------------- re-reading a redeployed upstream

/// Waits for a background /health re-read to be applied, so the assertions
/// below describe the forwarder rather than a race.
async fn settles(fwd: &common::Fwd, dict: bool, coding: &str) {
    for _ in 0..600 {
        let stats = fwd.get("/__portway/stats").await.json();
        let model = &stats["models"][DICT_MODEL];
        if model["dict"] == serde_json::json!(dict) && model["coding"] == serde_json::json!(coding)
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the negotiation never settled on dict={dict} coding={coding}");
}

#[tokio::test]
async fn a_upstream_that_comes_back_with_dictionaries_is_used_without_a_restart() {
    // The upstream is being replaced when the forwarder starts: the edge answers
    // 503 and /health is still the old image's, without dictionaries.
    let up = upstream(
        Health::Plaintext(Some(vec!["zstd", "gzip"])),
        Reply::Restarting,
    )
    .await;
    let fwd = forwarder(&[(DICT_MODEL, &up.base)], &[]).await;
    let probes = up.probes();
    assert_eq!(probes, 1, "one probe at startup");

    // A turn into the outage: relayed as the edge sent it, and it leaves the
    // negotiation marked stale.
    let answer = fwd.post("/v1/chat/completions", conversation(1)).await;
    assert_eq!(answer.status.as_u16(), 503);

    // The replacement is up, and this image advertises dictionaries.
    up.advertise(Health::PlaintextDict(vec!["zstd", "gzip"]));
    up.came_back();

    // The next request re-reads /health off to the side. It still goes out
    // under the old answer — nothing waits for a probe — so it is plain zstd
    // with no store asked for.
    fwd.post("/v1/chat/completions", conversation(1)).await;
    let during = up.last();
    assert_eq!(during.header("content-encoding"), Some("zstd"));
    assert!(!during.has("x-dict-store"));
    settles(&fwd, true, "zstd").await;
    assert!(up.probes() > probes, "the forwarder re-read /health");

    // From here it keeps bodies and chains turns on them, with no restart and
    // nothing asked of the agent.
    fwd.post("/v1/chat/completions", conversation(1)).await;
    assert_eq!(up.last().header("x-dict-store"), Some("1"));
    fwd.post("/v1/chat/completions", conversation(2)).await;
    let chained = up.last();
    assert_eq!(chained.header("content-encoding"), Some("dcz"));
    assert_eq!(chained.inflated, conversation(2));
    // The turn already held is gone from the wire; only the new one is left.
    assert!(chained.body.len() < conversation(2).len() * 6 / 10);
    let stats = fwd.get("/__portway/stats").await.json();
    assert_eq!(stats["models"][DICT_MODEL]["dict_hits"], 1);
}

#[tokio::test]
async fn a_upstream_that_comes_back_without_them_stops_being_sent_them() {
    let up = upstream(Health::PlaintextDict(vec!["zstd", "gzip"]), Reply::Dict).await;
    let fwd = forwarder(&[(DICT_MODEL, &up.base)], &[]).await;
    fwd.post("/v1/chat/completions", conversation(1)).await;
    fwd.post("/v1/chat/completions", conversation(2)).await;
    assert_eq!(up.last().header("content-encoding"), Some("dcz"));

    // Rolled back to an image that inflates zstd but holds no dictionaries.
    up.went_down();
    up.advertise(Health::Plaintext(Some(vec!["zstd", "gzip"])));
    fwd.post("/v1/chat/completions", conversation(3)).await;
    up.came_back();
    fwd.post("/v1/chat/completions", conversation(3)).await;
    settles(&fwd, false, "zstd").await;

    // Plain zstd from here, and the upstream is not asked to keep anything.
    fwd.post("/v1/chat/completions", conversation(4)).await;
    let plain = up.last();
    assert_eq!(plain.header("content-encoding"), Some("zstd"));
    assert!(!plain.has("x-dict-store"));
    assert_eq!(plain.inflated, conversation(4));
}
