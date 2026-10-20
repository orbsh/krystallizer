//! Integration tests for the HTTP surface: a real axum server on an
//! OS-picked port against a real Fjall keyspace, driven by reqwest.
//! Covers the three endpoints' round trips and the projection's
//! checkpoint/increment semantics over the wire (not just at the
//! store level — mem-core's own tests cover that).

use mem_core::channel::role;
use mem_server::{AppendCheckpoint, AppendMessage, MessageKeyDto};
use serde_json::json;
use std::net::SocketAddr;
use tempfile::TempDir;

/// Boot the router on 127.0.0.1:0, return its base URL; the keyspace
/// lives in a temp dir dropped at test end. The compression numbers are
/// k10r's defaults, which are far above these fixtures — a case that needs
/// a verdict boots through `spawn_server_with` instead.
async fn spawn_server() -> (String, TempDir) {
    spawn_server_with(mem_core::CompressionParams::default()).await
}

/// `spawn_server` with the compression policy stated.
async fn spawn_server_with(params: mem_core::CompressionParams) -> (String, TempDir) {
    let dir = TempDir::new().unwrap();
    let app = mem_server::open_with_compression(dir.path().to_path_buf(), None, params).unwrap();
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
    assert_eq!(session["increment"][0]["content"], "hi");
    assert_eq!(session["increment"][1]["role"], "assistant");
    assert_eq!(
        session["head"], "",
        "no facts in the scope yet: an empty head, never an error"
    );
    assert_eq!(
        session["format"], "plaintext",
        "the default representation is echoed"
    );

    // A checkpoint with nothing behind it is refused: the coverage point comes
    // from the verdict k10r served, so a bare summary is the caller's bug.
    let status = client
        .post(format!("{base}/channels/7/members/1/checkpoint"))
        .json(&AppendCheckpoint {
            summary: "greeting done".into(),
        })
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "no verdict was served, so there is no coverage point to land on"
    );
    let unchanged: serde_json::Value = client
        .get(format!("{base}/channels/7/members/1/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(unchanged["coverage_ts"], 0);
    assert_eq!(unchanged["increment"].as_array().unwrap().len(), 2);
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
        assert_eq!(inc[0]["content"], format!("ch{ch}"));
    }
}

/// Members are key-isolated too: a verdict and the summary that lands on it
/// belong to ONE member, so member 2's projection is untouched by member 1's
/// compression (ADR-0008 §2). The policy is dialled down so the read serves
/// a verdict for member 1 alone.
#[tokio::test]
async fn member_checkpoints_are_independent() {
    let (base, _dir) = spawn_server_with(mem_core::CompressionParams {
        context_bytes: 1,
        budget_percent: 50,
        cache_ttl_ms: u64::MAX,
        floor_bytes: 1,
    })
    .await;
    let client = reqwest::Client::new();

    client
        .post(format!("{base}/channels/9/messages"))
        .json(&json!({"ts_ms": 10, "sub": 0, "role": role::USER, "kind": 0, "content": "m"}))
        .send()
        .await
        .unwrap();

    // Member 1 reads (the verdict is theirs), then lands the summary.
    let read: serde_json::Value = client
        .get(format!("{base}/channels/9/members/1/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(read.get("compress").is_some(), "member 1 has a verdict");
    let status = client
        .post(format!("{base}/channels/9/members/1/checkpoint"))
        .json(&AppendCheckpoint {
            summary: "m1".into(),
        })
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);

    let s1: serde_json::Value = client
        .get(format!("{base}/channels/9/members/1/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s1["coverage_ts"], 10);

    // The checkpoint is the CHANNEL's (ADR-0008 §2: one voice per channel),
    // so member 2 sees the same coverage and summary — what stays per member is
    // the read cursor, not the compression.
    let s2: serde_json::Value = client
        .get(format!("{base}/channels/9/members/2/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s2["coverage_ts"], 10, "the node belongs to the channel");
    assert_eq!(s2["summary"], "m1");
    assert_eq!(s2["increment"].as_array().unwrap().len(), 0);
}

/// Phase 2.5 over the wire: the projection carries the HEAD block (the
/// user node's incident facts, weight-descending) ahead of the checkpoint
/// and the increment, and the increment's role arrives as the LLM role
/// name rather than the stored code.
///
/// The head is RENDERED by k10r in the requested format (ADR-0010 §5): the
/// default is the line form, and `format=struct` hands the same answer back
/// as data for a consumer that assembles it itself.
///
/// The user node is node 1 of scope 1 today (`DEFAULT_USER_NODE`), and the
/// door passes both defaults — so a fact whose head resolves first in
/// scope 1 is exactly what the head block must show.
#[tokio::test]
async fn session_carries_the_head_block_and_named_roles() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    // The first entity scope 1 resolves takes node id 1.
    let status = client
        .post(format!("{base}/memories/1"))
        .json(&json!([
            "用户",
            "person",
            {},
            "PREFERS",
            {},
            "简洁",
            "style",
            {}
        ]))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, reqwest::StatusCode::OK);

    client
        .post(format!("{base}/channels/7/messages"))
        .json(&json!({"ts_ms": 1000, "sub": 0, "role": role::USER, "kind": 0, "content": "hi"}))
        .send()
        .await
        .unwrap();

    let session: serde_json::Value = client
        .get(format!("{base}/channels/7/members/1/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let head = session["head"]
        .as_str()
        .expect("the head is rendered text, not data");
    assert!(
        head.contains("用户 (person) -[PREFERS]-> 简洁"),
        "the head renders the user node's fact: {head}"
    );
    assert!(head.contains("w="), "the line carries its counters: {head}");

    assert_eq!(session["increment"][0]["role"], "user");
    assert_eq!(session["increment"][0]["kind"], 0);
    assert_eq!(session["increment"][0]["content"], "hi");
    assert_eq!(
        session["increment"][0]["key"]["channel_id"], 7,
        "the key still rides every entry (a delta record is written from it)"
    );

    // `struct` is the SAME answer as data: the head becomes the fact array,
    // and the increment stays structural either way.
    let structured: serde_json::Value = client
        .get(format!("{base}/channels/7/members/1/session?format=struct"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(structured["format"], "struct");
    let facts = structured["head"].as_array().unwrap();
    assert_eq!(facts.len(), 1, "the user node's one fact");
    assert_eq!(facts[0]["kind"], "PREFERS");
    assert_eq!(facts[0]["src"]["name"], "用户");
    assert_eq!(facts[0]["dst"]["name"], "简洁");
    assert_eq!(facts[0]["src"]["node_id"], 1, "the default user node");

    // An unknown representation is the caller's bug, never a silent fallback
    // to another one.
    let bad = client
        .get(format!("{base}/channels/7/members/1/session?format=json"))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = bad.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("unknown format `json`"),
        "{body}"
    );
}
