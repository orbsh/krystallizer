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

use mem_server::{CodeIdDto, CodeRowDto, PutCode};
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
        code_id: None,
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
        .post(format!("{base}/code/37"))
        .json(&login())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(id.code_id, 1, "the store assigns ids from 1");

    let row: CodeRowDto = client
        .get(format!("{base}/code/37/1"))
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
        .get(format!("{base}/code/37/9"))
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
        .post(format!("{base}/code/37"))
        .json(&login())
        .send()
        .await
        .unwrap();
    post_finished(&client, &base, 7, 1000, 0, 0, 0, "hi").await;

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
    let row = client.get(format!("{base}/code/37/1")).send().await.unwrap();
    assert_eq!(row.status(), reqwest::StatusCode::OK);
    let ghost = client.get(format!("{base}/code/37/2")).send().await.unwrap();
    assert_eq!(
        ghost.status(),
        reqwest::StatusCode::NOT_FOUND,
        "the message does not leak into the code table"
    );
}

/// The code door is SCOPED since k10r ADR-0023 (a code row is a vertex of that
/// corpus's graph). One scope's row is invisible to another — the same rule the
/// facts follow, because it is now literally the same table family.
#[tokio::test]
async fn a_code_row_belongs_to_one_scope() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    client
        .post(format!("{base}/code/37"))
        .json(&login())
        .send()
        .await
        .unwrap();

    let here = client.get(format!("{base}/code/37/1")).send().await.unwrap();
    assert_eq!(here.status(), reqwest::StatusCode::OK);
    let elsewhere = client.get(format!("{base}/code/38/1")).send().await.unwrap();
    assert_eq!(elsewhere.status(), reqwest::StatusCode::NOT_FOUND);
}

/// A correction APPENDS: the door takes the same `code_id` and answers the next
/// revision, the ordinary read follows the newest one, and the version that ran
/// yesterday is still readable at its own address (ADR-0023's chain).
#[tokio::test]
async fn a_revision_appends_and_the_door_answers_both_versions() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let first: serde_json::Value = client
        .post(format!("{base}/code/37"))
        .json(&login())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["code_id"], 1);
    assert_eq!(first["rev"], 1, "a new script starts at version 1");

    let fixed: serde_json::Value = client
        .post(format!("{base}/code/37"))
        .json(&PutCode {
            code_id: Some(1),
            language: "nu".into(),
            description: "log into a host via ssh".into(),
            parameters: r#"{"host": "string", "user": "string"}"#.into(),
            source: "ssh $args.user@$args.host".into(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(fixed["code_id"], 1, "the address does not move");
    assert_eq!(fixed["rev"], 2, "the correction is a new version");

    let latest: serde_json::Value = client
        .get(format!("{base}/code/37/1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(latest["rev"], 2);
    assert_eq!(latest["source"], "ssh $args.user@$args.host");

    let old: serde_json::Value = client
        .get(format!("{base}/code/37/1/1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(old["rev"], 1);
    assert_eq!(old["source"], "ssh $args.host", "version 1 is immutable");

    let absent = client
        .get(format!("{base}/code/37/1/3"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        absent.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a version nobody wrote is absent, and never the latest"
    );
}

/// A revision of a script this store never wrote is a caller error, refused BY
/// ID: a phantom script would answer calls with a body nobody delivered.
#[tokio::test]
async fn a_revision_of_an_unknown_script_is_a_400_that_names_it() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let answer = client
        .post(format!("{base}/code/37"))
        .json(&PutCode {
            code_id: Some(99),
            language: "nu".into(),
            description: "never delivered".into(),
            parameters: "{}".into(),
            source: "ssh $args.host".into(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), reqwest::StatusCode::BAD_REQUEST);
    let body = answer.text().await.unwrap();
    assert!(body.contains("99"), "the refusal names the id: {body}");
}

/// The touch door counts the SCRIPT on its vertex: an attempt before the run and
/// a success after it, so `calls > ok` is what a failure looks like. The count is
/// per script, not per caller — one `generate-pdf` row can be reached from many
/// skills, and the row is where those paths meet.
#[tokio::test]
async fn a_touch_counts_the_script_and_names_what_is_missing() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let code: serde_json::Value = client
        .post(format!("{base}/code/37"))
        .json(&login())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let code_id = code["code_id"].as_u64().unwrap();

    let attempt: serde_json::Value = client
        .post(format!("{base}/code/37/{code_id}/touch"))
        .json(&serde_json::json!({ "event": "attempt", "ts_ms": 1_700_000_000_000u64 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(attempt["code_id"], code_id);
    assert_eq!((attempt["calls"].as_u64(), attempt["ok"].as_u64()), (Some(1), Some(0)));

    let success: serde_json::Value = client
        .post(format!("{base}/code/37/{code_id}/touch"))
        .json(&serde_json::json!({ "event": "success", "ts_ms": 1_700_000_002_000u64 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (success["calls"].as_u64(), success["ok"].as_u64()),
        (Some(1), Some(1)),
        "a success is the second record, not a second call"
    );

    // An unknown script is a 404 and an unknown event a 400: the counters never
    // invent the row they count, and a typo is not a new phase.
    let absent = client
        .post(format!("{base}/code/37/999/touch"))
        .json(&serde_json::json!({ "event": "attempt", "ts_ms": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(absent.status(), reqwest::StatusCode::NOT_FOUND);

    let typo = client
        .post(format!("{base}/code/37/{code_id}/touch"))
        .json(&serde_json::json!({ "event": "started", "ts_ms": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(typo.status(), reqwest::StatusCode::BAD_REQUEST);
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
    let opened: serde_json::Value = client
        .post(format!("{base}/channels/{channel}/turns"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let turn = opened["turn_id"].as_u64().unwrap();
    let key: mem_server::MessageKeyDto = client
        .post(format!("{base}/channels/{channel}/messages"))
        .json(&mem_server::AppendMessage {
            turn_id: turn,
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
