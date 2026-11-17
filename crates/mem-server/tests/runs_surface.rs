//! The run surface (gravity ADR-0011 §7): PUT the engine's document whole, GET it
//! back whole.
//!
//! What these tests pin is the DIVISION: k10r stores a run's state and never
//! decodes it, so the assertions are about the STORE's promises — the document
//! comes back byte-identical, the revision moves on every write, and a run that
//! was never written is a 404 rather than an empty document (an engine resuming
//! must be able to tell "no such run" from "a run with nothing in it").
//!
//! Real axum on an OS-picked port against a real Fjall keyspace, driven by
//! reqwest — the harness `tasks_surface.rs` uses.
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

/// A snapshot is a VALUE: the bytes come back exactly, and each write is a new
/// revision — the second write does not merge, extend or reorder anything.
#[tokio::test]
async fn a_run_snapshot_is_stored_whole_and_revised() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let first = json!({
        "steps": {"fetch": {"pass": 1, "attempt": 1, "done": true}},
        "ready": ["judge"],
        "note": "状态里的中文也是字节"
    });
    let res = client
        .put(format!("{base}/runs/7/1"))
        .json(&json!({"doc": first.to_string(), "ts_ms": 1000}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["revision"], 1, "the first write is revision 1");

    let res = client.get(format!("{base}/runs/7/1")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["revision"], 1);
    assert_eq!(body["updated_at"], 1000, "the caller's clock, recorded");
    assert_eq!(
        body["doc"].as_str().unwrap(),
        first.to_string(),
        "the document is opaque to k10r: it comes back byte for byte"
    );

    // A second write replaces the value and moves the revision; nothing of the
    // first survives, which is what "snapshot" means.
    let second = json!({"steps": {}, "ready": [], "done": "end"});
    let res = client
        .put(format!("{base}/runs/7/1"))
        .json(&json!({"doc": second.to_string(), "ts_ms": 2000}))
        .send()
        .await
        .unwrap();
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["revision"], 2);

    let body: Value = client
        .get(format!("{base}/runs/7/1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["revision"], 2);
    assert_eq!(body["updated_at"], 2000);
    assert_eq!(body["doc"].as_str().unwrap(), second.to_string());
}

/// A run is addressed by (scope, id): the same id in another scope is another
/// run, and one that was never written is a 404 — "no such run" and "a run with
/// nothing in it" must not look alike to an engine that is resuming.
#[tokio::test]
async fn runs_are_per_scope_and_absent_is_not_empty() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let res = client.get(format!("{base}/runs/1/9")).send().await.unwrap();
    assert_eq!(res.status(), 404, "an unwritten run is absent, not empty");

    let doc = json!({"ready": ["a"]}).to_string();
    client
        .put(format!("{base}/runs/1/9"))
        .json(&json!({"doc": doc, "ts_ms": 1}))
        .send()
        .await
        .unwrap();

    let one: Value = client
        .get(format!("{base}/runs/1/9"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one["doc"].as_str().unwrap(), doc);

    let other = client.get(format!("{base}/runs/2/9")).send().await.unwrap();
    assert_eq!(other.status(), 404, "another scope, another run");
}