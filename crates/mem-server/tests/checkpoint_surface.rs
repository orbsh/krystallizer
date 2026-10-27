//! The compression products (ADR-0008 §4 + the suggested cut): the session door
//! hands the caller a tail operation with a span, and the checkpoint door takes
//! the summary plus the cut the pass would have chosen — VALIDATED against the
//! span it closes, because every later pass reads that cut as the start of a
//! range that existed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use serde_json::{json, Value};
use std::net::SocketAddr;
use tempfile::TempDir;

/// A server whose policy fires on a small budget: the deadline in these tests is
/// "the verdict happened", not how much text it took.
async fn spawn_server() -> (String, TempDir) {
    let dir = TempDir::new().unwrap();
    let app = mem_server::open_with_compression(
        dir.path().to_path_buf(),
        None,
        mem_core::CompressionParams {
            context_bytes: 100,
            budget_percent: 50,
            cache_ttl_ms: 3_600_000,
            floor_bytes: 1,
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), dir)
}

async fn say(client: &reqwest::Client, base: &str, ts_ms: u64, content: &str) {
    client
        .post(format!("{base}/channels/3/messages"))
        .json(&json!({
            "ts_ms": ts_ms,
            "sub": 0,
            "role": 0,
            "kind": 0,
            "content": content,
        }))
        .send()
        .await
        .unwrap();
}

async fn session(client: &reqwest::Client, base: &str) -> Value {
    client
        .get(format!("{base}/channels/3/members/1/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_session_asks_for_an_operation_and_states_its_span() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    // Long enough for the byte budget, and spaced so the split has a largest
    // gap to land in (between the third record and the fourth).
    for (ts, text) in [
        (100, "the first thing the user said, at some length"),
        (200, "then a second line, also long enough to weigh"),
        (300, "and a third one that pushes the budget over"),
        (4_000, "much later, the follow-up after a break"),
    ] {
        say(&client, &base, ts, text).await;
    }

    let view = session(&client, &base).await;
    assert_eq!(view["suggested_ts"], 0, "no compression has passed an opinion yet");
    let tails = view["tails"].as_array().unwrap();
    assert!(!tails.is_empty(), "the budget fired: an operation is asked for");
    let compress = &tails[0];
    assert_eq!(compress["name"], "compress");
    let prompt = compress["prompt"].as_str().unwrap();
    assert!(
        prompt.contains("\"cut\""),
        "the pass is asked for TWO products: {prompt}"
    );
    assert_eq!(compress["from_ts"], 0, "the span starts where the summary ends");
    let to = compress["to_ts"].as_u64().unwrap();
    assert_eq!(to, 300, "the split lands in the largest gap, not at the log's end");
}

#[tokio::test]
async fn the_cut_a_pass_chooses_is_recorded_beside_the_summary() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    // Long enough for the byte budget, and spaced so the split has a largest
    // gap to land in (between the third record and the fourth).
    for (ts, text) in [
        (100, "the first thing the user said, at some length"),
        (200, "then a second line, also long enough to weigh"),
        (300, "and a third one that pushes the budget over"),
        (4_000, "much later, the follow-up after a break"),
    ] {
        say(&client, &base, ts, text).await;
    }
    let view = session(&client, &base).await;
    let to = view["tails"][0]["to_ts"].as_u64().unwrap();

    // The pass says the conversation really breaks after the first message.
    let status = client
        .post(format!("{base}/channels/3/members/1/checkpoint"))
        .json(&json!({
            "summary": "they said hi",
            "suggested_ts": 100,
            "suggested_sub": 0,
        }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);

    let view = session(&client, &base).await;
    assert_eq!(view["coverage_ts"].as_u64().unwrap(), to, "the node closes where the verdict said");
    assert_eq!(view["suggested_ts"], 100, "the opinion rides the node");
    assert_eq!(view["suggested_sub"], 0);
    assert_eq!(view["summary"], "they said hi");
    let increments = view["increment"].as_array().unwrap();
    assert!(
        increments
            .iter()
            .all(|entry| entry["key"]["ts_ms"].as_u64().unwrap() > to),
        "the LABEL opens no range: the increment is still cut at the coverage point"
    );
}

#[tokio::test]
async fn a_cut_outside_the_span_is_refused_rather_than_stored() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    // Long enough for the byte budget, and spaced so the split has a largest
    // gap to land in (between the third record and the fourth).
    for (ts, text) in [
        (100, "the first thing the user said, at some length"),
        (200, "then a second line, also long enough to weigh"),
        (300, "and a third one that pushes the budget over"),
        (4_000, "much later, the follow-up after a break"),
    ] {
        say(&client, &base, ts, text).await;
    }
    let view = session(&client, &base).await;
    let to = view["tails"][0]["to_ts"].as_u64().unwrap();

    // Beyond the node the pass was told to cover: the summary cannot have
    // reached it, so a later pass would read a range that never existed.
    let beyond = client
        .post(format!("{base}/channels/3/members/1/checkpoint"))
        .json(&json!({ "summary": "s", "suggested_ts": to + 1000 }))
        .send()
        .await
        .unwrap();
    assert_eq!(beyond.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = beyond.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("outside the span"),
        "the refusal names what is wrong: {body}"
    );

    // Nothing was stored, and the node is still open for the real answer.
    let view = session(&client, &base).await;
    assert_eq!(view["summary"], Value::Null, "the summary did not land");
    assert_eq!(view["suggested_ts"], 0);
    let ok = client
        .post(format!("{base}/channels/3/members/1/checkpoint"))
        .json(&json!({ "summary": "landed", "suggested_ts": 100 }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), reqwest::StatusCode::NO_CONTENT);
}