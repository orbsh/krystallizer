//! The task surface (k10r ADR-0021): open, move, list — the three things a task
//! door does that a vertex cannot do itself.
//!
//! The point these tests pin is the DIVISION: the state lives on the sheet and is
//! enumerated by the sheet's face, while a dependency is an ordinary fact through
//! `POST /memories/{scope}` — there is no `/tasks/{scope}/deps` door, because one
//! graph does not need a second vocabulary for its edges.
//!
//! Real axum on an OS-picked port against a real Fjall keyspace, driven by
//! reqwest — the same harness `skills_surface.rs` uses.
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

#[tokio::test]
async fn a_task_is_opened_moved_and_listed() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    // Open with the caller's clock.
    let opened: Value = client
        .post(format!("{base}/tasks/37"))
        .json(&json!({ "name": "ship the face change", "ts_ms": 1_000 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let node_id = opened["node_id"].as_u64().unwrap();
    assert_eq!(opened["created"], json!(true));

    // It lists as open, and open is what the face answers with.
    let open: Vec<Value> = client
        .get(format!("{base}/tasks/37?state=open"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0]["node_id"].as_u64().unwrap(), node_id);
    assert_eq!(open[0]["state"], json!("open"));
    assert!(
        open[0].get("next").is_none(),
        "an empty next action is absent, not an empty string on the wire"
    );

    // Move it.
    let moved: Value = client
        .post(format!("{base}/tasks/37/{node_id}/move"))
        .json(&json!({ "state": "doing", "next": "read the diff", "ts_ms": 2_000 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(moved["state"], json!("doing"));
    assert_eq!(moved["next"], json!("read the diff"));
    assert_eq!(moved["updated_at"], json!(2_000));

    // The face followed the move in both directions.
    let open: Vec<Value> = client
        .get(format!("{base}/tasks/37?state=open"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(open.is_empty(), "a task is in one state at a time");
    let doing: Vec<Value> = client
        .get(format!("{base}/tasks/37?state=doing"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(doing.len(), 1);

    // Everything, without a state, is the kind scan.
    let all: Vec<Value> = client
        .get(format!("{base}/tasks/37"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all.len(), 1);
}

#[tokio::test]
async fn a_dependency_is_an_ordinary_fact() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    // No task door was called: the fact names both ends as `task` and the
    // resolver opens them — a task mentioned by a fact exists.
    let landed: Value = client
        .post(format!("{base}/memories/37"))
        .json(&json!([[
            "task-12",
            "task",
            {},
            "depends_on",
            {},
            "task-9",
            "task",
            {}
        ]]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(landed[0]["outcome"], json!("linked"));

    let open: Vec<Value> = client
        .get(format!("{base}/tasks/37?state=open"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = open
        .iter()
        .map(|task| task["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["task-12", "task-9"]);

    // The edge is visible from the graph door the facts already use.
    let graph: Value = client
        .get(format!("{base}/graph/37"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(graph["links"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn the_doors_keep_the_three_mistakes_apart() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    // A task name that is already a person is not re-labelled, so it is not a
    // task and has no sheet: the move answers 400 naming the kind it found.
    client
        .post(format!("{base}/memories/37"))
        .json(&json!([[
            "ada",
            "person",
            {},
            "OWNS",
            {},
            "notes",
            "thing",
            {}
        ]]))
        .send()
        .await
        .unwrap();
    let graph: Value = client
        .get(format!("{base}/graph/37"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ada = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == json!("ada"))
        .unwrap()["id"]
        .as_u64()
        .unwrap();

    let not_a_task = client
        .post(format!("{base}/tasks/37/{ada}/move"))
        .json(&json!({ "state": "done" }))
        .send()
        .await
        .unwrap();
    assert_eq!(not_a_task.status(), 400);
    assert!(not_a_task.text().await.unwrap().contains("not a task"));

    // An unknown id is 404, not 400: the request is fine, the row is not there.
    let absent = client
        .post(format!("{base}/tasks/37/9999/move"))
        .json(&json!({ "state": "done" }))
        .send()
        .await
        .unwrap();
    assert_eq!(absent.status(), 404);

    // A state that is not a state is refused: a typo must not become a state no
    // listing can find.
    let opened: Value = client
        .post(format!("{base}/tasks/37"))
        .json(&json!({ "name": "typo guard", "ts_ms": 1_000 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = opened["node_id"].as_u64().unwrap();
    let typo = client
        .post(format!("{base}/tasks/37/{id}/move"))
        .json(&json!({ "state": "openn" }))
        .send()
        .await
        .unwrap();
    assert_eq!(typo.status(), 400);
    assert!(typo.text().await.unwrap().contains("not a task state"));

    // A move that names nothing is refused rather than treated as a no-op.
    let empty = client
        .post(format!("{base}/tasks/37/{id}/move"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 400);
}
