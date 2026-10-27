//! The skill node's HTTP surface (ADR-0015): declare, resolve, read — plus the
//! three answers the doors must keep apart (created vs merged, absent vs
//! another kind, a scan that reports no score).
//!
//! Real axum on an OS-picked port against a real Fjall keyspace, driven by
//! reqwest — the same shape `http_surface.rs` uses, because the doors are the
//! surface gravity actually calls.
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

/// No embedding engine: the resolver's kind-face fallback is the path under
/// test, and the ranking path is mem-core's own (`tests/skills.rs`).
async fn spawn_server() -> (String, TempDir) {
    let dir = TempDir::new().unwrap();
    let app = mem_server::open_with_compression(
        dir.path().to_path_buf(),
        None,
        mem_core::CompressionParams::default(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), dir)
}

/// The three step-shaped parts travel as JSON TEXT (ADR-0015 §3): the wire
/// shape says so, and a caller that sends an array is told its body is wrong
/// instead of having the array quietly stringified.
fn skill_body(name: &str, content: &str) -> Value {
    json!({
        "name": name,
        "title": format!("{name} (title)"),
        "content": content,
        "preconditions": r#"["the repo is cloned"]"#,
        "steps": r#"[{"code_id":1,"note":"run it"}]"#,
        "verification": r#"["the output parses"]"#,
    })
}

#[tokio::test]
async fn a_skill_is_declared_resolved_and_read_back() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let landed: Value = client
        .post(format!("{base}/skills/1"))
        .json(&skill_body("github-issues", "Lists a repo's open issues."))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(landed["created"], true);
    let node_id = landed["node_id"].as_u64().unwrap();

    // Declaring the same name again merges: not a second skill, not a new id.
    let again: Value = client
        .post(format!("{base}/skills/1"))
        .json(&skill_body("github-issues", "Lists a repo's OPEN issues."))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["created"], false);
    assert_eq!(again["node_id"].as_u64().unwrap(), node_id);

    let read: Value = client
        .get(format!("{base}/skills/1/{node_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(read["name"], "github-issues");
    assert_eq!(read["content"], "Lists a repo's OPEN issues.");
    // The parts travel as TEXT because that is how they were stored: the store
    // does not parse what the producer owns.
    assert_eq!(read["steps"], r#"[{"code_id":1,"note":"run it"}]"#);

    let resolved: Vec<Value> = client
        .post(format!("{base}/skills/1/resolve"))
        .json(&json!({"query": "list github issues"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["name"], "github-issues");
    assert_eq!(
        resolved[0]["steps"], r#"[{"code_id":1,"note":"run it"}]"#,
        "the candidate carries its steps so the model can decide"
    );
    assert_eq!(
        resolved[0]["score"], 0.0,
        "no engine, no distance: the scan reports none rather than a fake one"
    );
}

#[tokio::test]
async fn the_doors_keep_absence_and_a_wrong_kind_apart() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    assert_eq!(
        client
            .get(format!("{base}/skills/1/999"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );

    // A fact's endpoint is a node in the same id space: a skill read of it is
    // a WRONG REQUEST, not a missing id.
    let alice: Value = client
        .post(format!("{base}/memories/1"))
        .json(&json!([["alice", "person", {}, "likes", {}, "tea", "drink", {}]]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let _ = alice;
    let entity: Value = client
        .get(format!("{base}/memories/1/entities/alice"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let alice_id = entity["node_id"].as_u64().unwrap();

    let response = client
        .get(format!("{base}/skills/1/{alice_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let error: Value = response.json().await.unwrap();
    assert!(
        error["error"].as_str().unwrap().contains("not a skill"),
        "the error says what is actually there: {error}"
    );
}

#[tokio::test]
async fn the_resolver_answers_skills_and_only_skills() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    for name in ["github-issues", "read-a-pdf"] {
        client
            .post(format!("{base}/skills/1"))
            .json(&skill_body(name, "does a thing"))
            .send()
            .await
            .unwrap();
    }
    client
        .post(format!("{base}/memories/1"))
        .json(&json!([["alice", "person", {}, "likes", {}, "tea", "drink", {}]]))
        .send()
        .await
        .unwrap();

    let resolved: Vec<Value> = client
        .post(format!("{base}/skills/1/resolve"))
        .json(&json!({"query": "anything", "limit": 1}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resolved.len(), 1, "the limit is honoured");
    assert_eq!(resolved[0]["name"], "github-issues", "id order without a query landing");

    // Another scope answers from its own corpus: the resolver is scoped like
    // every other door, never a global list.
    let empty: Vec<Value> = client
        .post(format!("{base}/skills/2/resolve"))
        .json(&json!({"query": "anything"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty.is_empty(), "scope 2 has declared nothing: {empty:?}");
}