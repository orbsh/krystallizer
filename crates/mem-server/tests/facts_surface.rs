//! Integration tests for the fact graph's surface (ADR-0010 §1's write
//! side): a real axum server on an OS-picked port against a real Fjall
//! database, driven by reqwest. The tuple is the request BODY, verbatim.
//!
//! The load-bearing assertions: the caller's three-way outcome comes back
//! (linked / rewritten / dropped), a multi-valued relation accumulates
//! parallel edges while a functional one rewrites its incumbent IN PLACE
//! (which proves the incumbent scan read the stored edge through the
//! door), a structural bug is a 400 carrying the core's own reason, and
//! the graph shares the `memory` keyspace with flat memory without either
//! seeing the other.

use mem_server::{IngestOutcomeDto, StoreMemory};
use std::net::SocketAddr;
use tempfile::TempDir;

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

/// One ingest over the door: the body is the canonical 8-slot tuple.
async fn ingest(
    client: &reqwest::Client,
    base: &str,
    scope: u64,
    tuple: serde_json::Value,
) -> reqwest::Response {
    client
        .post(format!("{base}/facts/{scope}"))
        .json(&tuple)
        .send()
        .await
        .unwrap()
}

fn tuple(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

/// A fact lands and answers `linked` with the edge id; a relation that
/// declares nothing is multi-valued, so the next fact is a SECOND edge.
#[tokio::test]
async fn a_fact_round_trips_and_multi_valued_relations_accumulate() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let first: IngestOutcomeDto = ingest(
        &client,
        &base,
        1,
        tuple(r#"["user","person",{},"PREFERS",{},"iced_americano","coffee",{}]"#),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(first.outcome, "linked");
    assert_eq!(first.edge_id, 1, "the store assigns the edge id");

    let second: IngestOutcomeDto = ingest(
        &client,
        &base,
        1,
        tuple(r#"["user","person",{},"PREFERS",{},"cappuccino","coffee",{}]"#),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(
        (second.outcome.as_str(), second.edge_id),
        ("linked", 2),
        "no declaration = parallel edges are separate facts"
    );
}

/// A relation declared `functional` at one source has ONE incumbent: the
/// newer fact rewrites that edge and keeps its id. Reaching the INCUMBENT
/// is the assertion that the door really wrote and re-read the edge.
#[tokio::test]
async fn a_functional_relation_rewrites_its_incumbent() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let first: IngestOutcomeDto = ingest(
        &client,
        &base,
        1,
        tuple(
            r#"["k10r","system",{},"STORAGE",{"functional":true,"since":100},"fjall","system",{}]"#,
        ),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!((first.outcome.as_str(), first.edge_id), ("linked", 1));

    let second: IngestOutcomeDto = ingest(
        &client,
        &base,
        1,
        tuple(r#"["k10r","system",{},"STORAGE",{"functional":true,"since":200},"slatedb","system",{}]"#),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(
        (second.outcome.as_str(), second.edge_id),
        ("rewritten", first.edge_id),
        "the winner keeps the incumbent's edge id"
    );

    // A weaker, older fact is the loser: nothing is written.
    let third: IngestOutcomeDto = ingest(
        &client,
        &base,
        1,
        tuple(r#"["k10r","system",{},"STORAGE",{"functional":true,"since":50,"confidence":10},"fdb","system",{}]"#),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!((third.outcome.as_str(), third.edge_id), ("dropped", 1));
}

/// The scope is the path segment, and it is the ownership boundary: the
/// same names in another scope are another corpus, so a functional fact
/// there finds no incumbent to adjudicate against.
#[tokio::test]
async fn the_scope_rides_the_path() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let fact = || {
        tuple(
            r#"["k10r","system",{},"STORAGE",{"functional":true,"since":100},"fjall","system",{}]"#,
        )
    };

    let mine: IngestOutcomeDto = ingest(&client, &base, 1, fact())
        .await
        .json()
        .await
        .unwrap();
    let theirs: IngestOutcomeDto = ingest(&client, &base, 2, fact())
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(mine.outcome, "linked");
    assert_eq!(
        theirs.outcome, "linked",
        "scope 2's source is a different node — no incumbent crosses the boundary"
    );
    assert_eq!(theirs.edge_id, 2, "ids are global, the boundary is not");
}

/// A malformed tuple is the caller's bug: 400, with the core's own reason
/// passed through (no reworded summary, no half-stored fact).
#[tokio::test]
async fn a_malformed_tuple_is_refused_with_the_reason() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let short = ingest(&client, &base, 1, tuple(r#"["a","b",{},"REL"]"#)).await;
    assert_eq!(short.status(), 400);
    let body: serde_json::Value = short.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("8 slots"),
        "the arity is named: {body}"
    );

    let shaped = ingest(
        &client,
        &base,
        1,
        tuple(r#"["a","b",1,"REL",{},"c","d",{}]"#),
    )
    .await;
    assert_eq!(shaped.status(), 400);
    let body: serde_json::Value = shaped.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("slot 2"),
        "the offending slot is named: {body}"
    );

    let not_a_list = ingest(&client, &base, 1, tuple(r#"{"head":"a"}"#)).await;
    assert_eq!(not_a_list.status(), 400);
}

/// The graph and flat memory share ONE keyspace (the `memory` tree), so
/// the ns is the only separator: an entity's name is not a memory row and
/// a memory's text is not a graph node.
#[tokio::test]
async fn the_graph_and_flat_memory_share_a_keyspace_without_leaking() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    client
        .post(format!("{base}/memory"))
        .json(&StoreMemory {
            user_id: 1,
            text: "we chose Fjall".into(),
            created_at: 1000,
        })
        .send()
        .await
        .unwrap();

    ingest(
        &client,
        &base,
        1,
        tuple(r#"["galaxy_repo","repository",{},"PIVOTED_TO",{},"k10r","system",{}]"#),
    )
    .await;

    let hits: Vec<serde_json::Value> = client
        .get(format!("{base}/memory/1/search?q=galaxy_repo"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        hits.is_empty(),
        "the entity row does not show up in a memory search"
    );

    let still_there: Vec<serde_json::Value> = client
        .get(format!("{base}/memory/1/search?q=Fjall"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(still_there.len(), 1, "the memory row survives the ingest");
}
