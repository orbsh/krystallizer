//! Phase 2.6 over the wire: the compression verdict rides the projection
//! as a CONTROL FIELD (ADR-0008 §4), the read cursor is stamped only when
//! the caller supplies its clock, and the two signals are independent —
//! a big window with a long idle fires the cache clock, a small one fires
//! the budget.
//!
//! Real axum on an OS-picked port against a real Fjall keyspace, like the
//! other surface tests: the ordering inside `fetch_session` (measure the
//! idle against the OLD cursor, then stamp) is only observable here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_core::channel::role;
use mem_core::{CompressionParams, TAIL_PROMPT};
use serde_json::Value;
use std::net::SocketAddr;
use tempfile::TempDir;

async fn spawn(params: CompressionParams) -> (String, TempDir) {
    let dir = TempDir::new().unwrap();
    let app = mem_server::open_with_compression(dir.path().to_path_buf(), None, params).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), dir)
}

/// Params that only the cache clock can move: an enormous window means the
/// budget never fires, so any verdict here came from the cursor.
fn cache_only() -> CompressionParams {
    CompressionParams {
        context_bytes: 100_000_000,
        budget_percent: 50,
        cache_ttl_ms: 60_000,
        floor_bytes: 1,
    }
}

/// The compression loop over the wire, end to end: the read serves the
/// verdict, the caller returns ONLY the summary, and k10r lands it on the
/// split it proposed — coverage and all (ADR-0008 §4: the caller never
/// states a coverage timestamp).
#[tokio::test]
async fn a_summary_lands_on_the_served_split() {
    let (base, _dir) = spawn(budget_only()).await;
    let client = reqwest::Client::new();
    append_three(&base, &client, 7).await;

    let read = session(&base, &client, 7, None).await;
    let verdict = &read["tails"][0];
    assert_eq!(
        verdict["name"], "compress",
        "the operation table's one entry"
    );
    assert_eq!(verdict["to_ts"], 2_000, "the span's last record");

    let status = client
        .post(format!("{base}/channels/7/members/00000000-0000-0000-0000-000000000001/checkpoint"))
        .json(&serde_json::json!({ "summary": "the first two messages" }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);

    let after = session(&base, &client, 7, None).await;
    assert_eq!(after["coverage_ts"], 2_000, "derived from the verdict");
    assert_eq!(after["summary"], "the first two messages");
    assert_eq!(
        after["increment"].as_array().map(Vec::len),
        Some(1),
        "only the post-split record remains"
    );
}

/// Params that only the budget can move: a one-hour TTL, a tiny window.
fn budget_only() -> CompressionParams {
    CompressionParams {
        context_bytes: 100,
        budget_percent: 50,
        cache_ttl_ms: 3_600_000,
        floor_bytes: 1,
    }
}

/// Three finished exchanges, one per turn: the split the policy picks is a TURN
/// boundary (ADR-0024 §7), so the fixture is turns, not loose rows.
async fn append_three(base: &str, client: &reqwest::Client, channel: u64) {
    for (ts, text) in [
        (1_000u64, "first message of the span"),
        (2_000, "second message of the span"),
        (9_000, "third message of the span"),
    ] {
        post_finished(client, base, channel, ts, 0, role::USER, 0, text).await;
    }
}

async fn session(base: &str, client: &reqwest::Client, channel: u64, ts: Option<u64>) -> Value {
    let url = match ts {
        Some(ts) => format!("{base}/channels/{channel}/members/00000000-0000-0000-0000-000000000001/session?ts_ms={ts}"),
        None => format!("{base}/channels/{channel}/members/00000000-0000-0000-0000-000000000001/session"),
    };
    let response = client.get(url).send().await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    response.json().await.unwrap()
}

/// No clock on the read = nothing stamped: the cursor does not move, so
/// the cache clock never speaks, however long the wall clock runs. (The
/// budget is out of reach in these params, so absence is unambiguous.)
#[tokio::test]
async fn an_unstamped_read_never_fires_the_cache_clock() {
    let (base, _dir) = spawn(cache_only()).await;
    let client = reqwest::Client::new();
    append_three(&base, &client, 7).await;

    for _ in 0..3 {
        let body = session(&base, &client, 7, None).await;
        assert_eq!(
            body["tails"].as_array().map(Vec::len),
            Some(0),
            "an unstamped read must not produce a verdict"
        );
    }
}

/// The stamped read is what starts the clock, and the TTL is measured
/// from the PREVIOUS read — so a read that arrives early resets it. The
/// pair below is deliberate: channel 7 waits the full TTL, channel 8 stops
/// one millisecond short, and both start from a first stamped read.
///
/// The verdict's split is the largest inter-message gap (7000 ms between
/// the second and the third record), so the span ends at the second.
#[tokio::test]
async fn the_cache_clock_fires_after_the_ttl() {
    let (base, _dir) = spawn(cache_only()).await;
    let client = reqwest::Client::new();
    append_three(&base, &client, 7).await;
    append_three(&base, &client, 8).await;

    let first = session(&base, &client, 7, Some(10_000)).await;
    assert_eq!(
        first["tails"].as_array().map(Vec::len),
        Some(0),
        "the first stamped read has no previous cursor to measure against"
    );

    let fired = session(&base, &client, 7, Some(10_000 + 60_000)).await;
    let compress = &fired["tails"][0];
    assert_eq!(compress["name"], "compress");
    assert_eq!(compress["to_ts"], 2_000, "the span's last record");
    assert_eq!(
        compress["from_ts"], 0,
        "no summary yet, so the span starts at the log's head"
    );
    assert_eq!(
        compress["prompt"].as_str(),
        Some(TAIL_PROMPT),
        "the tail prompt is k10r's artifact, served verbatim"
    );

    let short = session(&base, &client, 8, Some(10_000)).await;
    assert_eq!(short["tails"].as_array().map(Vec::len), Some(0));
    let under = session(&base, &client, 8, Some(10_000 + 59_999)).await;
    assert_eq!(
        under["tails"].as_array().map(Vec::len),
        Some(0),
        "just under the TTL"
    );
}

/// The budget needs no clock at all: the same read answers a verdict with
/// no `ts_ms` on the query, and the increment is untouched by any of it.
#[tokio::test]
async fn the_budget_fires_without_a_clock() {
    let (base, _dir) = spawn(budget_only()).await;
    let client = reqwest::Client::new();
    append_three(&base, &client, 7).await;

    let body = session(&base, &client, 7, None).await;
    let compress = &body["tails"][0];
    assert_eq!(compress["name"], "compress");
    assert_eq!(compress["to_ts"], 2_000);
    assert_eq!(body["increment"].as_array().map(Vec::len), Some(3));

    // The floor is the last word: nothing fires on a view too small to be
    // worth a summary, whatever the window says.
    let (base, _dir) = spawn(CompressionParams {
        floor_bytes: usize::MAX,
        ..budget_only()
    })
    .await;
    let client = reqwest::Client::new();
    append_three(&base, &client, 7).await;
    let silenced = session(&base, &client, 7, None).await;
    assert_eq!(
        silenced["tails"].as_array().map(Vec::len),
        Some(0),
        "the floor outranks it"
    );
}

/// One finished exchange over the wire (k10r ADR-0024): open a turn, append the
/// record, close the turn keeping it. A closed turn's log is its records PLUS
/// its closing record — which is why a table's row count is turns + records.
#[allow(clippy::too_many_arguments)]
async fn post_finished(
    client: &reqwest::Client,
    base: &str,
    channel: u64,
    ts_ms: u64,
    sub: u16,
    role: u8,
    kind: u8,
    content: &str,
) -> mem_server::MessageKeyDto {
    let key: mem_server::MessageKeyDto = client
        .post(format!("{base}/channels/{channel}/messages"))
        .json(&mem_server::AppendMessage {
            turn_id: None,
            ts_ms,
            sub,
            role,
            kind,
            content: content.to_string(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let turn = key.turn_id;
    let closed = client
        .post(format!("{base}/channels/{channel}/turns/{turn}/close"))
        .json(&serde_json::json!({
            "ts_ms": ts_ms,
            "sub": sub + 1,
            "role": role,
            "content": "",
            "keep": [{ "ts_ms": ts_ms, "sub": sub }],
        }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(closed, reqwest::StatusCode::NO_CONTENT, "closing the turn");
    key
}
