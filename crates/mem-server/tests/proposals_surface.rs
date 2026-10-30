//! The proposal doors (ADR-0016): count a walk, list the queue, promote one —
//! over real axum on an OS-picked port against a real Fjall keyspace.
//!
//! What these pin beyond the core's own tests: the WIRE shape (the composition
//! key is the walker's spelling, `steps` is JSON TEXT), the query parameter the
//! queue filters with, and the fact that promotion is visible to the resolver
//! only after the human's call.
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

fn walk_body(key: &str, ts_ms: u64) -> Value {
    json!({
        "key": key,
        "steps": r#"[{"code_id":4,"note":"fetch"},{"code_id":7,"note":"format"}]"#,
        "ts_ms": ts_ms,
    })
}

#[tokio::test]
async fn a_walk_is_counted_and_the_queue_answers_it() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let first: Value = client
        .post(format!("{base}/skills/1/propose"))
        .json(&walk_body("4+7", 1_000))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["created"], true);
    assert_eq!(first["count"], 1);
    let node_id = first["node_id"].as_u64().unwrap();

    let second: Value = client
        .post(format!("{base}/skills/1/propose"))
        .json(&walk_body("4+7", 2_500))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second["node_id"].as_u64(), Some(node_id), "the same row");
    assert_eq!(second["created"], false);
    assert_eq!(second["count"], 2);

    let queue: Value = client
        .get(format!("{base}/skills/1/proposals"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = queue.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], "4+7");
    assert_eq!(rows[0]["count"], 2);
    assert_eq!(rows[0]["last_seen"], 2_500, "the caller's clock as given");
    assert_eq!(rows[0]["promoted_to"], "", "nothing has promoted it");
    assert!(
        rows[0]["steps"].as_str().unwrap().contains("code_id"),
        "the steps travel as the JSON text the walker reported"
    );

    // The threshold rides the query, and the store has no opinion of its own.
    let filtered: Value = client
        .get(format!("{base}/skills/1/proposals?min_count=3"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        filtered.as_array().unwrap().len(),
        0,
        "seen twice, asked for 3"
    );
}

#[tokio::test]
async fn promotion_is_a_humans_declaration_and_the_resolver_sees_the_skill() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let landed: Value = client
        .post(format!("{base}/skills/1/propose"))
        .json(&walk_body("4+7", 1_000))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let node_id = landed["node_id"].as_u64().unwrap();

    // Before the human's call: the resolver answers no skills at all.
    let resolved: Value = client
        .post(format!("{base}/skills/1/resolve"))
        .json(&json!({ "query": "fetch and format" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        resolved.as_array().unwrap().len(),
        0,
        "a draft is not a skill"
    );

    let promoted: Value = client
        .post(format!("{base}/skills/1/{node_id}/promote"))
        .json(&json!({
            "name": "fetch-then-format",
            "title": "Fetch, then format",
            "content": "What the run repeated.",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(promoted["created"], true);
    assert_eq!(promoted["name"], "fetch-then-format");
    let skill_node = promoted["skill_node_id"].as_u64().unwrap();

    // The skill reads back with the steps the proposal carried.
    let skill: Value = client
        .get(format!("{base}/skills/1/{skill_node}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(skill["name"], "fetch-then-format");
    assert_eq!(
        skill["steps"], r#"[{"code_id":4,"note":"fetch"},{"code_id":7,"note":"format"}]"#,
        "the promotion copies the run rather than retyping it"
    );

    // The queue drops the row, and the row keeps the record.
    let queue: Value = client
        .get(format!("{base}/skills/1/proposals"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(queue.as_array().unwrap().len(), 0);

    let resolved: Value = client
        .post(format!("{base}/skills/1/resolve"))
        .json(&json!({ "query": "fetch and format" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = resolved
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["fetch-then-format"],
        "the model can choose it now"
    );
}

#[tokio::test]
async fn the_doors_refuse_what_they_cannot_answer() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    // No key: the composition is the identity, so there is nothing to count.
    let refused = client
        .post(format!("{base}/skills/1/propose"))
        .json(&json!({ "key": "  ", "steps": "[]", "ts_ms": 1 }))
        .send()
        .await
        .unwrap();
    assert!(
        refused.status().is_client_error(),
        "an empty key is a refused body, not an empty row: {}",
        refused.status()
    );

    // An unknown id is absent for the promote door too.
    let missing = client
        .post(format!("{base}/skills/1/9999/promote"))
        .json(&json!({ "name": "nope" }))
        .send()
        .await
        .unwrap();
    assert!(missing.status().is_client_error(), "{}", missing.status());
}
