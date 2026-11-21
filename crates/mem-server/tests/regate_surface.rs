//! Integration tests for the two RE-CUT doors (okm ADR-0070's verbs, reachable
//! from outside the process): a merge folds one entity into another, a split
//! hands a named set of edges to a new one. Both are one graph batch inside, so
//! the load-bearing assertions are that the edges really moved (not duplicated),
//! that the loser is gone, and that a refusal lands nothing.
//!
//! Everything is driven over HTTP against a real Fjall store: a verb that only
//! exists in theory is what these tests exist to prevent.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_server::{EntityDto, FactViewDto, IngestOutcomeDto, MergeAnswer, SplitAnswer};
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

fn tuple(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

/// One fact over the plural write door; the answer's `key` is the edge's hex.
async fn ingest(client: &reqwest::Client, base: &str, scope: u64, body: &str) -> IngestOutcomeDto {
    let response = client
        .post(format!("{base}/memories/{scope}"))
        .json(&serde_json::json!([tuple(body)]))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK, "ingest: {body}");
    let answers: Vec<IngestOutcomeDto> = response.json().await.unwrap();
    answers.into_iter().next().expect("one answer per tuple")
}

async fn entity(client: &reqwest::Client, base: &str, scope: u64, name: &str) -> Option<EntityDto> {
    let response = client
        .get(format!("{base}/memories/{scope}/entities/{name}"))
        .send()
        .await
        .unwrap();
    match response.status() {
        reqwest::StatusCode::OK => Some(response.json().await.unwrap()),
        reqwest::StatusCode::NOT_FOUND => None,
        other => panic!("entity {name}: {other}"),
    }
}

async fn edges(client: &reqwest::Client, base: &str, scope: u64, node_id: u64) -> Vec<FactViewDto> {
    let response = client
        .get(format!("{base}/graph/{scope}/entities/{node_id}/edges"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.unwrap()
}

/// The whole chain through the doors: write four facts, merge one entity into
/// another (one edge moves, one collapses into a loop, one is a duplicate of an
/// incumbent), then split the survivor by naming the edge that changes hands.
#[tokio::test]
async fn a_merge_and_a_split_are_drivable_over_the_wire() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let scope = 1;

    ingest(
        &client,
        &base,
        scope,
        r#"["alice","person",{},"KNOWS",{"confidence":42},"bob","person",{}]"#,
    )
    .await;
    ingest(
        &client,
        &base,
        scope,
        r#"["carol","person",{},"KNOWS",{},"bob","person",{}]"#,
    )
    .await;
    ingest(
        &client,
        &base,
        scope,
        r#"["alice","person",{},"KNOWS",{},"carol","person",{}]"#,
    )
    .await;
    ingest(
        &client,
        &base,
        scope,
        r#"["alice","person",{},"WORKS_AT",{"confidence":7},"acme","org",{}]"#,
    )
    .await;

    let alice = entity(&client, &base, scope, "alice")
        .await
        .expect("alice exists")
        .node_id
        .expect("an entity node");
    let carol = entity(&client, &base, scope, "carol")
        .await
        .expect("carol exists")
        .node_id
        .expect("an entity node");

    // ---- merge: alice folds into carol ----
    let response = client
        .post(format!("{base}/memories/{scope}/merge"))
        .json(&serde_json::json!({ "loser": alice, "survivor": carol }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let merged: MergeAnswer = response.json().await.unwrap();
    assert_eq!(merged.survivor, carol);
    assert_eq!(
        (merged.repointed, merged.collapsed, merged.absorbed),
        (1, 1, 1),
        "one moved, one collapsed into a loop, one was already stated"
    );

    assert!(
        entity(&client, &base, scope, "alice").await.is_none(),
        "the loser's row is gone"
    );
    let survivor_edges = edges(&client, &base, scope, carol).await;
    let works_at = survivor_edges
        .iter()
        .find(|fact| fact.kind == "WORKS_AT")
        .expect("the moved fact is on the survivor");
    assert_eq!(
        works_at.confidence, 7,
        "the payload rode along instead of landing as a default"
    );
    assert_eq!(
        survivor_edges
            .iter()
            .filter(|fact| fact.kind == "KNOWS")
            .count(),
        1,
        "the duplicate did not become a second row"
    );
    assert!(
        survivor_edges
            .iter()
            .all(|fact| fact.dst.node_id != Some(carol) || fact.kind != "KNOWS"),
        "the loop the fold would have made is not there"
    );

    // ---- split: carol gives the acme fact to a new entity ----
    let response = client
        .post(format!("{base}/memories/{scope}/split"))
        .json(&serde_json::json!({
            "node": carol,
            "name": "carol-org",
            "kind": "org",
            "moving": [works_at.key],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let split: SplitAnswer = response.json().await.unwrap();
    assert_eq!(split.node, carol);
    assert_eq!(split.repointed, 1, "one edge changed hands");

    let created = entity(&client, &base, scope, "carol-org")
        .await
        .expect("the split declared the new entity");
    assert_eq!(created.node_id, Some(split.created), "and answers its id");
    assert_eq!(created.kind, "org");
    let moved = edges(&client, &base, scope, split.created).await;
    assert_eq!(moved.len(), 1, "exactly the edge it was given");
    assert_eq!(moved[0].kind, "WORKS_AT");
    assert_eq!(moved[0].confidence, 7, "and its payload");
    assert!(
        edges(&client, &base, scope, carol)
            .await
            .iter()
            .all(|fact| fact.kind != "WORKS_AT"),
        "the source no longer has it"
    );

    // ---- a hex that names no fact lands nothing ----
    let response = client
        .post(format!("{base}/memories/{scope}/split"))
        .json(&serde_json::json!({
            "node": carol,
            "name": "never",
            "kind": "org",
            "moving": ["00ff"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "a key naming no fact is the caller's mistake"
    );
    assert!(
        entity(&client, &base, scope, "never").await.is_none(),
        "and nothing was written for it"
    );

    // A merge of an entity that does not exist is refused the same way.
    let response = client
        .post(format!("{base}/memories/{scope}/merge"))
        .json(&serde_json::json!({ "loser": 999_999, "survivor": carol }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
}
