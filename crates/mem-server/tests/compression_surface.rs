//! Phase 2.6 over the wire: the compression verdict rides the projection
//! as a CONTROL FIELD (ADR-0008 §4), the read cursor is stamped only when
//! the caller supplies its clock, and the two signals are independent —
//! a big window with a long idle fires the cache clock, a small one fires
//! the budget.
//!
//! Real axum on an OS-picked port against a real Fjall keyspace, like the
//! other surface tests: the ordering inside `fetch_session` (measure the
//! idle against the OLD cursor, then stamp) is only observable here.

use mem_core::channel::role;
use mem_core::{CompressionParams, TAIL_PROMPT};
use mem_server::AppendMessage;
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
    let verdict = read.get("compress").expect("the window was exceeded");
    assert_eq!(verdict["checkpoint"], 2_000);

    let status = client
        .post(format!("{base}/channels/7/members/1/checkpoint"))
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

async fn append_three(base: &str, client: &reqwest::Client, channel: u64) {
    for (ts, text) in [
        (1_000u64, "first message of the span"),
        (2_000, "second message of the span"),
        (9_000, "third message of the span"),
    ] {
        let status = client
            .post(format!("{base}/channels/{channel}/messages"))
            .json(&AppendMessage {
                ts_ms: ts,
                sub: 0,
                role: role::USER,
                kind: 0,
                content: text.into(),
            })
            .send()
            .await
            .unwrap()
            .status();
        assert!(status.is_success(), "append failed: {status}");
    }
}

async fn session(base: &str, client: &reqwest::Client, channel: u64, ts: Option<u64>) -> Value {
    let url = match ts {
        Some(ts) => format!("{base}/channels/{channel}/members/1/session?ts_ms={ts}"),
        None => format!("{base}/channels/{channel}/members/1/session"),
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
        assert!(
            body.get("compress").is_none(),
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
    assert!(
        first.get("compress").is_none(),
        "the first stamped read has no previous cursor to measure against"
    );

    let fired = session(&base, &client, 7, Some(10_000 + 60_000)).await;
    let compress = fired.get("compress").expect("the TTL was reached");
    assert_eq!(compress["checkpoint"], 2_000, "the span's last record");
    assert_eq!(
        compress["prompt"].as_str(),
        Some(TAIL_PROMPT),
        "the tail prompt is k10r's artifact, served verbatim"
    );

    let short = session(&base, &client, 8, Some(10_000)).await;
    assert!(short.get("compress").is_none());
    let under = session(&base, &client, 8, Some(10_000 + 59_999)).await;
    assert!(under.get("compress").is_none(), "just under the TTL");
}

/// The budget needs no clock at all: the same read answers a verdict with
/// no `ts_ms` on the query, and the increment is untouched by any of it.
#[tokio::test]
async fn the_budget_fires_without_a_clock() {
    let (base, _dir) = spawn(budget_only()).await;
    let client = reqwest::Client::new();
    append_three(&base, &client, 7).await;

    let body = session(&base, &client, 7, None).await;
    let compress = body.get("compress").expect("the window was exceeded");
    assert_eq!(compress["checkpoint"], 2_000);
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
    assert!(silenced.get("compress").is_none(), "the floor outranks it");
}
