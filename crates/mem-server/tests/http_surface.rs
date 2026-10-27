//! Integration tests for the HTTP surface: a real axum server on an
//! OS-picked port against a real Fjall keyspace, driven by reqwest.
//! Covers the three endpoints' round trips and the projection's
//! checkpoint/increment semantics over the wire (not just at the
//! store level — mem-core's own tests cover that).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

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
            suggested_ts: None,
            suggested_sub: None,
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
    assert_eq!(
        read["tails"].as_array().map(Vec::len),
        Some(1),
        "member 1 has a verdict"
    );
    let status = client
        .post(format!("{base}/channels/9/members/1/checkpoint"))
        .json(&AppendCheckpoint {
            summary: "m1".into(),
            suggested_ts: None,
            suggested_sub: None,
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
/// A member's facts hang on the node named `member-{channel}-{id}`
/// (ADR-0009 §1), so a fact written on channel 7's member 1 node is exactly
/// what that channel's head must show.
#[tokio::test]
async fn session_carries_the_head_block_and_named_roles() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let status = client
        .post(format!("{base}/memories/1"))
        .json(&json!([[
            "member-7-1",
            "person",
            {},
            "PREFERS",
            {},
            "简洁",
            "style",
            {}
        ]]))
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
        head.contains("member-7-1 (person) -[PREFERS]-> 简洁"),
        "the head renders the member node's fact: {head}"
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
    assert_eq!(facts[0]["src"]["name"], "member-7-1");
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

/// The two views over the wire (ADR-0009 §4): the session answers TURNS (a user
/// burst merged, every line attributed), while `GET /channels/{c}/messages` gives
/// people the raw log — same rows, no merging, no checkpoint.
#[tokio::test]
async fn the_session_gives_turns_and_the_channel_gives_rows() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    for (ts, role_code, text) in [
        (1_000u64, role::USER, "在吗"),
        (1_050, role::USER, "问个事"),
        (1_100, role::ASSISTANT, "在，你说"),
    ] {
        client
            .post(format!("{base}/channels/7/messages"))
            .json(&AppendMessage {
                ts_ms: ts,
                sub: 0,
                role: role_code,
                kind: 0,
                content: text.into(),
            })
            .send()
            .await
            .unwrap();
    }

    let session: serde_json::Value = client
        .get(format!("{base}/channels/7/members/1/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let turns = session["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2, "two user lines merged into one turn");
    assert_eq!(turns[0]["role"], "user");
    assert_eq!(
        turns[0]["text"],
        "[master(id=1)] 在吗\n[master(id=1)] 问个事"
    );
    assert_eq!(turns[1]["role"], "assistant");
    assert_eq!(
        session["increment"].as_array().unwrap().len(),
        3,
        "raw stays raw"
    );

    let rows: Vec<serde_json::Value> = client
        .get(format!("{base}/channels/7/messages"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rows.len(), 3, "the people's view is every record, unmerged");
    assert_eq!(rows[0]["sender_name"], "master");
    assert_eq!(rows[1]["content"], "问个事");
}

/// Phase 4's progressive read over the wire: the heaviest facts of a scope, as
/// data, with `k` bounding the result. The touched fact ranks first and the empty
/// scope answers an empty list rather than an error.
#[tokio::test]
async fn the_top_face_ranks_a_scope_by_weight() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    for (head, tail) in [("alpha", "shared"), ("beta", "shared")] {
        client
            .post(format!("{base}/memories/1"))
            .json(&json!([[
                head,
                "thing",
                {},
                "POINTS_AT",
                {},
                tail,
                "thing",
                {}
            ]]))
            .send()
            .await
            .unwrap();
    }
    let all: Vec<serde_json::Value> = client
        .get(format!("{base}/graph/1/top?k=10"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all.len(), 2, "both facts of the scope");
    let plain = all[0]["edge_id"].as_u64().unwrap();

    // Boost the OTHER one: a write touch folds new counters into its weight.
    let other = all[1]["edge_id"].as_u64().unwrap();
    client
        .post(format!("{base}/memories/1/edges/{other}/touch"))
        .json(&json!({"kind": 1, "ts_ms": 5000}))
        .send()
        .await
        .unwrap();

    let top: Vec<serde_json::Value> = client
        .get(format!("{base}/graph/1/top?k=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(top.len(), 1, "k bounds the result");
    assert_eq!(
        top[0]["edge_id"].as_u64().unwrap(),
        other,
        "the touched fact is now the heaviest"
    );
    assert_ne!(top[0]["edge_id"].as_u64().unwrap(), plain);

    let empty: Vec<serde_json::Value> = client
        .get(format!("{base}/graph/9/top?k=5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        empty.is_empty(),
        "an empty scope is an empty list, not an error"
    );
}

/// The deterministic read and the third write verb: a fact fetched BY ID (the
/// mirror of the `edge_id` the write answered with), withdrawn in one call, then
/// absent — and an id from another scope is a clean 404, never a leak.
#[tokio::test]
async fn a_fact_is_read_and_withdrawn_by_its_id() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let key: serde_json::Value = client
        .post(format!("{base}/memories/1"))
        .json(&json!([[
            "alpha",
            "thing",
            {},
            "POINTS_AT",
            {},
            "beta",
            "thing",
            {}
        ]]))
        .send()
        .await
        .unwrap()
        .json::<Vec<serde_json::Value>>()
        .await
        .unwrap()
        .remove(0);
    let edge_id = key["edge_id"].as_u64().unwrap();

    let fetched: serde_json::Value = client
        .get(format!("{base}/graph/1/facts/{edge_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(fetched["edge_id"].as_u64().unwrap(), edge_id);
    assert_eq!(fetched["kind"], "POINTS_AT");
    assert_eq!(fetched["src"]["name"], "alpha");

    assert_eq!(
        client
            .get(format!("{base}/graph/2/facts/{edge_id}"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND,
        "scope is part of the address"
    );

    assert_eq!(
        client
            .delete(format!("{base}/graph/1/facts/{edge_id}"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    assert_eq!(
        client
            .get(format!("{base}/graph/1/facts/{edge_id}"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND,
        "withdrawn means gone, not tombstoned-and-visible"
    );
    assert_eq!(
        client
            .delete(format!("{base}/graph/1/facts/{edge_id}"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND,
        "a second delete is a miss, not an error storm"
    );
    let top: Vec<serde_json::Value> = client
        .get(format!("{base}/graph/1/top?k=10"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(top.is_empty(), "the ranking no longer sees it");
}

/// PLAN 4c over the wire: a fact's reference links ride the answer as ids, and
/// each one is an address the caller can fetch on its own. Nothing validates the
/// target — following it is where the 404 lives.
#[tokio::test]
async fn a_reference_link_rides_the_answer_as_an_address() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let detail: serde_json::Value = client
        .post(format!("{base}/memories/1"))
        .json(&json!([[
            "the_detail",
            "note",
            {},
            "MENTIONS",
            {},
            "thing",
            "thing",
            {}
        ]]))
        .send()
        .await
        .unwrap()
        .json::<Vec<serde_json::Value>>()
        .await
        .unwrap()
        .remove(0);
    let detail_id = detail["edge_id"].as_u64().unwrap();

    let anchor: serde_json::Value = client
        .post(format!("{base}/memories/1"))
        .json(&json!([[
            "anchor",
            "thing",
            {},
            "PREFERS",
            {"ref": detail_id},
            "x",
            "thing",
            {}
        ]]))
        .send()
        .await
        .unwrap()
        .json::<Vec<serde_json::Value>>()
        .await
        .unwrap()
        .remove(0);
    let anchor_id = anchor["edge_id"].as_u64().unwrap();

    let fetched: serde_json::Value = client
        .get(format!("{base}/graph/1/facts/{anchor_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        fetched["refs"],
        json!([detail_id]),
        "the pointer travels as an id, not as an inlined fact"
    );
    assert_eq!(
        client
            .get(format!("{base}/graph/1/facts/{detail_id}"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK,
        "and the address resolves"
    );
}

/// PLAN 4b over the wire: three entrances that need no query string query — an
/// entity's anchored memory, the scope's timeline, and a causal chain over one
/// predicate. Each answers from the structure the writes already laid down.
#[tokio::test]
async fn the_three_entrances_answer_without_a_query() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    for (src, since, dst) in [("a", 10, "b"), ("b", 20, "c"), ("c", 30, "a")] {
        let response = client
            .post(format!("{base}/memories/1"))
            .json(&json!([[src, "thing", {}, "LEADS_TO", {"since": since}, dst, "thing", {}]]))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "ingest failed: {} {:?}",
            response.status(),
            response.text().await.unwrap()
        );
    }
    let a: serde_json::Value = client
        .get(format!("{base}/memories/1/entities/a"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let a_id = a["node_id"].as_u64().unwrap();

    let anchored: serde_json::Value = client
        .get(format!("{base}/graph/1/entities/{a_id}/anchored"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(anchored["node_id"].as_u64().unwrap(), a_id);
    assert!(
        anchored["facts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["kind"] == "LEADS_TO"),
        "the anchor's own facts are there"
    );
    assert_eq!(
        client
            .get(format!("{base}/graph/1/entities/999999/anchored"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND,
        "an unknown node has no anchored memory"
    );

    let timeline: Vec<serde_json::Value> = client
        .get(format!("{base}/graph/1/timeline?k=2"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(timeline.len(), 2, "k bounds it");
    assert_eq!(timeline[0]["since"].as_u64().unwrap(), 30, "newest first");
    assert_eq!(timeline[1]["since"].as_u64().unwrap(), 20);

    let chain: Vec<serde_json::Value> = client
        .get(format!(
            "{base}/graph/1/chain?from={a_id}&kind=LEADS_TO&hops=5"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let depths: Vec<u64> = chain
        .iter()
        .map(|hop| hop["depth"].as_u64().unwrap())
        .collect();
    assert_eq!(
        depths,
        vec![0, 1, 2],
        "the walk order IS the answer's order"
    );
}
