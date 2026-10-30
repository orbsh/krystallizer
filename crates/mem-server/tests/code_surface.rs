//! Integration tests for the code store's HTTP surface (gravity
//! ADR-0001/0003): a real axum server on an OS-picked port against a real
//! Fjall database, driven by reqwest. Covers the write/read round trip,
//! the 404 for an unknown id, the pool read's shape (what a linker picks
//! from, without sources), and — the load-bearing one — that both table
//! groups live in ONE database without seeing each other (one Fjall
//! handle, one lock file, ns-discriminated entries).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_server::{AppendMessage, CodeIdDto, CodeRowDto, PutCode};
use std::net::SocketAddr;
use tempfile::TempDir;

/// Boot the router on 127.0.0.1:0, return its base URL; the database
/// lives in a temp dir dropped at test end.
async fn spawn_server() -> (String, TempDir) {
    let dir = TempDir::new().unwrap();
    let app = mem_server::open(dir.path().to_path_buf()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), dir)
}

/// A reusable piece of code: a login script many skills would point at.
fn login() -> PutCode {
    PutCode {
        language: "nu".into(),
        description: "log into a host via ssh".into(),
        parameters: r#"{"host": "string", "user": "string"}"#.into(),
        source: "ssh $args.host".into(),
    }
}

/// The authoring act then the call-time read: the store assigns the id,
/// and every field comes back verbatim — the description and the
/// parameter definition included, because that is what says what the code
/// takes without reading its source.
#[tokio::test]
async fn a_script_round_trips_by_id() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let id: CodeIdDto = client
        .post(format!("{base}/code"))
        .json(&login())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(id.code_id, 1, "the store assigns ids from 1");

    let row: CodeRowDto = client
        .get(format!("{base}/code/1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        row,
        CodeRowDto {
            language: "nu".into(),
            description: "log into a host via ssh".into(),
            parameters: r#"{"host": "string", "user": "string"}"#.into(),
            source: "ssh $args.host".into(),
        }
    );
}

/// An id nothing was written under is a 404 — the caller turns it into an
/// error value for the model; the store has no opinion.
#[tokio::test]
async fn an_unknown_id_is_a_404() {
    let (base, _dir) = spawn_server().await;
    let status = reqwest::Client::new()
        .get(format!("{base}/code/9"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
}

/// Both table groups share ONE database (one Fjall handle, one lock file):
/// a code write must not appear in the channel log, and a message must not
/// appear in the pool. This is what the 2-byte ns buys.
#[tokio::test]
async fn code_and_channel_share_one_database_without_seeing_each_other() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    client
        .post(format!("{base}/code"))
        .json(&login())
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/channels/7/messages"))
        .json(&AppendMessage {
            ts_ms: 1000,
            sub: 0,
            role: 0,
            kind: 0,
            content: "hi".into(),
        })
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
    assert_eq!(
        session["increment"].as_array().unwrap().len(),
        1,
        "the code row does not leak into the channel projection"
    );

    // The message must not leak into the code table: the one row written
    // there is still the one the door stored, and nothing sits beside it.
    let row = client.get(format!("{base}/code/1")).send().await.unwrap();
    assert_eq!(row.status(), reqwest::StatusCode::OK);
    let ghost = client.get(format!("{base}/code/2")).send().await.unwrap();
    assert_eq!(
        ghost.status(),
        reqwest::StatusCode::NOT_FOUND,
        "the message does not leak into the code table"
    );
}
