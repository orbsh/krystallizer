//! Integration tests for the flat-memory surface (PLAN 1e — the memory
//! family's tool face): a real axum server on an OS-picked port against a
//! real Fjall database, driven by reqwest. Covers the write/read round
//! trip (the store assigns the id), the user-scoped scan (one user's rows
//! never leak into another's), the legal empty result, and — the
//! load-bearing one — that flat memory and the code store share ONE
//! keyspace without seeing each other (same tree, different ns).

use mem_server::{MemoryHitDto, MemoryIdDto, PutCode, StoreMemory};
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

/// One memory with the caller's clock (`created_at` is the caller's to
/// stamp — the server never assigns one, ADR-0008 §3).
fn memory(user_id: u64, text: &str, at: u64) -> StoreMemory {
    StoreMemory {
        user_id,
        text: text.into(),
        created_at: at,
    }
}

async fn store(client: &reqwest::Client, base: &str, body: StoreMemory) -> MemoryIdDto {
    client
        .post(format!("{base}/memory"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn search(client: &reqwest::Client, base: &str, user_id: u64, q: &str) -> Vec<MemoryHitDto> {
    client
        .get(format!("{base}/memory/{user_id}/search?q={q}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// The add then the recall: ids come from the store (1, 2, …) and the
/// text comes back verbatim, in key order.
#[tokio::test]
async fn a_memory_round_trips_by_id() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let first = store(&client, &base, memory(1, "we chose Rust + Fjall", 1000)).await;
    let second = store(&client, &base, memory(1, "no milk in coffee", 2000)).await;
    assert_eq!(first.memory_id, 1, "the store assigns ids from 1");
    assert_eq!(second.memory_id, 2);

    let hits = search(&client, &base, 1, "Fjall").await;
    assert_eq!(
        hits,
        vec![MemoryHitDto {
            memory_id: 1,
            text: "we chose Rust + Fjall".into(),
        }],
        "a hit carries its identity and its text"
    );
}

/// The scan is user-prefixed: two users storing the same word never see
/// each other's rows.
#[tokio::test]
async fn the_scan_is_user_scoped() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    store(&client, &base, memory(1, "engine picked", 1000)).await;
    store(&client, &base, memory(2, "engine rejected", 1000)).await;

    let mine = search(&client, &base, 1, "engine").await;
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].text, "engine picked");

    let theirs = search(&client, &base, 2, "engine").await;
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0].text, "engine rejected");

    assert!(
        search(&client, &base, 3, "engine").await.is_empty(),
        "a user with no rows has nothing to return"
    );
}

/// An empty query and a miss are both an empty list — a legal answer, not
/// an error: the caller injects no memory for that turn.
#[tokio::test]
async fn an_empty_result_is_legal() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    store(&client, &base, memory(1, "something", 1000)).await;

    assert!(search(&client, &base, 1, "").await.is_empty());
    assert!(search(&client, &base, 1, "nothing-like-this")
        .await
        .is_empty());
}

/// Flat memory and the code store live in the SAME keyspace (the `memory`
/// tree), so the 2-byte ns is the only thing keeping them apart: a memory
/// must not show up in the code pool, and code must not show up in a
/// memory search.
#[tokio::test]
async fn memory_and_code_share_one_keyspace_without_seeing_each_other() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    store(&client, &base, memory(1, "login", 1000)).await;
    client
        .post(format!("{base}/code"))
        .json(&PutCode {
            language: "nu".into(),
            description: "log into a host via ssh".into(),
            parameters: r#"{"host": "string"}"#.into(),
            source: "ssh $args.host".into(),
        })
        .send()
        .await
        .unwrap();

    let hits = search(&client, &base, 1, "login").await;
    assert_eq!(hits.len(), 1, "the code row does not leak into memory");
    assert_eq!(hits[0].text, "login");

    let pool: Vec<serde_json::Value> = client
        .get(format!("{base}/code"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pool.len(), 1, "the memory row does not leak into the pool");
}
