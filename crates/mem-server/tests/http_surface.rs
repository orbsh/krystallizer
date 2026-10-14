//! Integration tests for the HTTP surface: a real axum server on an
//! OS-picked port against a real Fjall keyspace, driven by reqwest.
//! Covers the three endpoints' round trips and the projection's
//! checkpoint/increment semantics over the wire (not just at the
//! store level — mem-core's own tests cover that).

use mem_core::channel::role;
use mem_server::{open, AppendCheckpoint, AppendMessage, MessageKeyDto};
use serde_json::json;
use std::net::SocketAddr;
use tempfile::TempDir;

/// Boot the router on 127.0.0.1:0, return its base URL; the keyspace
/// lives in a temp dir dropped at test end.
async fn spawn_server() -> (String, TempDir) {
    let dir = TempDir::new().unwrap();
    let app = open(dir.path().to_path_buf()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), dir)
}

/// One append round trip: the returned key echoes the request's clock
/// fields plus the path's channel id — the caller's delta record is
/// the key, so it must be reconstructible exactly.
#[tokio::test]
async fn append_returns_full_key() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let body = AppendMessage {
        ts_ms: 1000,
        sub: 1,
        role: role::USER,
        kind: 0, // kind::TEXT
        content: "hello".into(),
    };
    let key: MessageKeyDto = client
        .post(format!("{base}/channels/7/messages"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        key,
        MessageKeyDto {
            channel_id: 7,
            ts_ms: 1000,
            sub: 1
        },
        "the key must echo channel/ts/sub verbatim"
    );
}

/// The full Phase 0 flow over HTTP: append user + assistant, read the
/// projection back; then advance the checkpoint and confirm the
/// increment shrinks to strictly-after coverage.
#[tokio::test]
async fn append_fetch_checkpoint_flow() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    // user at ts 1000, assistant reply at ts 1001 (same sub — ts is the
    // order authority; sub only breaks ties within one ms).
    for (ts, r, text) in [
        (1000u64, role::USER, "hi"),
        (1001, role::ASSISTANT, "hello"),
    ] {
        let status = client
            .post(format!("{base}/channels/7/messages"))
            .json(&AppendMessage {
                ts_ms: ts,
                sub: 1,
                role: r,
                kind: 0,
                content: text.into(),
            })
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::OK);
    }

    // Fresh channel: projection == the full log (no checkpoint yet).
    let session: serde_json::Value = client
        .get(format!("{base}/channels/7/members/1/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["coverage_ts"], 0);
    assert_eq!(session["summary"], serde_json::Value::Null);
    assert_eq!(session["increment"].as_array().unwrap().len(), 2);
    // Key order == time order (BE layout), no server-side sort.
    assert_eq!(session["increment"][0]["key"]["ts_ms"], 1000);
    assert_eq!(session["increment"][1]["key"]["ts_ms"], 1001);
    assert_eq!(session["increment"][0]["row"]["content"], "hi");
    assert_eq!(session["increment"][1]["row"]["role"], role::ASSISTANT);

    // Advance the checkpoint past the user's message; the increment
    // keeps only strictly-after entries and the summary rides along.
    let status = client
        .post(format!("{base}/channels/7/members/1/checkpoint"))
        .json(&AppendCheckpoint {
            coverage_ts: 1000,
            summary: "greeting done".into(),
        })
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);

    let session: serde_json::Value = client
        .get(format!("{base}/channels/7/members/1/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["coverage_ts"], 1000);
    assert_eq!(session["summary"], "greeting done");
    let inc = session["increment"].as_array().unwrap();
    assert_eq!(inc.len(), 1, "only entries strictly after coverage remain");
    assert_eq!(inc[0]["key"]["ts_ms"], 1001);
}

/// Channels are prefix-isolated: entries on one channel never appear
/// in another's projection (the scan is channel-prefixed).
#[tokio::test]
async fn channels_are_isolated() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    for ch in [1u64, 2] {
        client
            .post(format!("{base}/channels/{ch}/messages"))
            .json(&AppendMessage {
                ts_ms: 500,
                sub: 0,
                role: role::USER,
                kind: 0,
                content: format!("ch{ch}"),
            })
            .send()
            .await
            .unwrap();
    }
    for ch in [1u64, 2] {
        let session: serde_json::Value = client
            .get(format!("{base}/channels/{ch}/members/1/session"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let inc = session["increment"].as_array().unwrap();
        assert_eq!(inc.len(), 1, "channel {ch} sees only its own entry");
        assert_eq!(inc[0]["row"]["content"], format!("ch{ch}"));
    }
}

/// Members are key-isolated too: two checkpoints on one channel
/// compress independently (ADR-0008 §2).
#[tokio::test]
async fn member_checkpoints_are_independent() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    client
        .post(format!("{base}/channels/9/messages"))
        .json(&json!({"ts_ms": 10, "sub": 0, "role": role::USER, "kind": 0, "content": "m"}))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{base}/channels/9/members/1/checkpoint"))
        .json(&AppendCheckpoint {
            coverage_ts: 10,
            summary: "m1".into(),
        })
        .send()
        .await
        .unwrap();

    // Member 2's projection is untouched by member 1's checkpoint.
    let s2: serde_json::Value = client
        .get(format!("{base}/channels/9/members/2/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s2["coverage_ts"], 0);
    assert_eq!(s2["increment"].as_array().unwrap().len(), 1);
}
