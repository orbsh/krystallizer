//! Integration tests for the fact graph's surface (ADR-0010 §1's write
//! side): a real axum server on an OS-picked port against a real Fjall
//! database, driven by reqwest. The tuple is the request BODY, verbatim.
//!
//! The load-bearing assertions: the caller's three-way outcome comes back
//! (linked / rewritten / dropped / bumped), a multi-valued relation accumulates
//! parallel edges while a functional one rewrites its incumbent IN PLACE
//! (which proves the incumbent scan read the stored edge through the
//! door), a structural bug is a 400 carrying the core's own reason, and
//! the graph shares the `memory` keyspace with the code store without
//! either seeing the other.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_server::{EntityDto, FactViewDto, GraphDto, IngestOutcomeDto};
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

/// One ingest over the door. The door is PLURAL — the body is an array of
/// tuples — so one fact is a ONE-ELEMENT array, and the answer is an array in
/// the same order. [`Answer`] unwraps that single element, so a test can go on
/// talking about "the outcome" without pretending the door is singular.
async fn ingest(
    client: &reqwest::Client,
    base: &str,
    scope: u64,
    tuple: serde_json::Value,
) -> Answer {
    let response = client
        .post(format!("{base}/memories/{scope}"))
        .json(&serde_json::json!([tuple]))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap();
    Answer { status, body }
}

/// One request's answer, with the plural write door's wrapping taken off.
struct Answer {
    status: reqwest::StatusCode,
    body: serde_json::Value,
}

impl Answer {
    fn status(&self) -> reqwest::StatusCode {
        self.status
    }

    /// The single element of a one-element write (a REQUEST-level refusal has
    /// no array at all and is handed over as it came).
    async fn json<T: serde::de::DeserializeOwned>(self) -> Result<T, serde_json::Error> {
        let value = match self.body {
            serde_json::Value::Array(mut answers) => {
                if answers.len() == 1 {
                    answers.remove(0)
                } else {
                    serde_json::Value::Array(answers)
                }
            }
            other => other,
        };
        serde_json::from_value(value)
    }
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

    // Element-level refusals: the write door answers 200 and names the bad
    // element, because the OTHERS in the same request still land.
    let short = ingest(&client, &base, 1, tuple(r#"["a","b",{},"REL"]"#)).await;
    assert_eq!(short.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = short.json().await.unwrap();
    assert_eq!(body["outcome"], "refused");
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
    let body: serde_json::Value = shaped.json().await.unwrap();
    assert_eq!(body["outcome"], "refused");
    assert!(
        body["error"].as_str().unwrap().contains("slot 2"),
        "the offending slot is named: {body}"
    );

    // A body that is not an array is not a fact list at all: that IS a 400.
    let not_a_list = client
        .post(format!("{base}/memories/1"))
        .json(&tuple(r#"{"head":"a"}"#))
        .send()
        .await
        .unwrap();
    assert_eq!(not_a_list.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = not_a_list.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("array"),
        "the body shape is named: {body}"
    );
}

/// The graph and the code store share ONE keyspace (the `memory` tree),
/// so the ns is the only separator: an entity and a piece of code can both
/// be the "first row" of their table (id 1) without either seeing the
/// other.
#[tokio::test]
async fn the_graph_and_the_code_store_share_a_keyspace_without_leaking() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let code: serde_json::Value = client
        .post(format!("{base}/code"))
        .json(&serde_json::json!({
            "language": "nu",
            "description": "log into a host",
            "parameters": "{}",
            "source": "ssh $args.host",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(code["code_id"], 1, "the code table's first id");

    ingest(
        &client,
        &base,
        1,
        tuple(r#"["galaxy_repo","repository",{},"PIVOTED_TO",{},"k10r","system",{}]"#),
    )
    .await;

    // Same numeric id (1) in both tables — different ns, no collision.
    let entity: EntityDto = client
        .get(format!("{base}/memories/1/entities/galaxy_repo"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(entity.node_id, Some(1));
    assert_eq!(entity.user, None);

    let stored: serde_json::Value = client
        .get(format!("{base}/code/1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stored["source"], "ssh $args.host", "the code row survives");

    // The entity wrote nothing into the code table: the only id there is the
    // one the door stored, and the next id is empty.
    let ghost = client.get(format!("{base}/code/2")).send().await.unwrap();
    assert_eq!(
        ghost.status(),
        reqwest::StatusCode::NOT_FOUND,
        "the entity is not a code row"
    );
}

/// The exact name entrance: the human identity reaches the graph here
/// (ADR-0003). An unknown name is a 404 — not an empty result, which
/// would be a different statement.
#[tokio::test]
async fn the_exact_entrance_finds_a_node_and_404s_when_absent() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    ingest(
        &client,
        &base,
        1,
        tuple(r#"["galaxy_repo","repository",{},"PIVOTED_TO",{},"k10r","system",{}]"#),
    )
    .await;

    let found: EntityDto = client
        .get(format!("{base}/memories/1/entities/galaxy_repo"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        found,
        EntityDto {
            node_id: Some(1),
            user: None,
            kind: "repository".into(),
            literal_num: 0,
            name: "galaxy_repo".into(),
        }
    );

    let absent = client
        .get(format!("{base}/memories/1/entities/nothing-here"))
        .send()
        .await
        .unwrap();
    assert_eq!(absent.status(), 404);

    let other_scope = client
        .get(format!("{base}/memories/2/entities/galaxy_repo"))
        .send()
        .await
        .unwrap();
    assert_eq!(other_scope.status(), 404, "the name lives inside its scope");
}

/// One hop of the traversal: the node's outgoing facts come back with
/// BOTH endpoints resolved, so a caller (or a renderer) never has to walk
/// the refs itself — and a literal object arrives as the node it is.
#[tokio::test]
async fn one_hop_traversal_resolves_both_ends() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    ingest(
        &client,
        &base,
        1,
        tuple(r#"["user","person",{},"PREFERS",{},"iced_americano","coffee",{}]"#),
    )
    .await;
    ingest(
        &client,
        &base,
        1,
        tuple(r#"["user","person",{},"BORN_IN",{},"1990","literal",{}]"#),
    )
    .await;

    let edges: Vec<FactViewDto> = client
        .get(format!("{base}/graph/1/entities/1/edges"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(edges.len(), 2, "both facts leave the same source");
    assert_eq!(edges[0].kind, "PREFERS");
    assert_eq!(edges[0].src.name, "user");
    assert_eq!(edges[0].dst.name, "iced_americano");
    assert_eq!(edges[0].dst.kind, "coffee");
    assert_eq!(edges[1].kind, "BORN_IN");
    assert_eq!(edges[1].dst.kind, "literal");
    assert_eq!(edges[1].dst.literal_num, 1990, "the literal's projection");
    assert_eq!(
        (edges[0].src.node_id, edges[0].src.name.as_str()),
        (Some(1), "user"),
        "the endpoints arrive resolved — nothing was passed in"
    );

    let none: Vec<FactViewDto> = client
        .get(format!("{base}/graph/1/entities/999/edges"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        none.is_empty(),
        "a node with no facts is a legal empty list"
    );
}

/// The renderer's door: one scope's graph as nodes + links — what the served
/// `/viz` page reads instead of hand-written packets. Nothing
/// from another scope rides it, and an empty scope is a legal empty graph.
#[tokio::test]
async fn the_visualization_door_serves_one_scope_as_nodes_and_links() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    ingest(
        &client,
        &base,
        1,
        tuple(r#"["user","person",{},"PREFERS",{},"iced_americano","coffee",{}]"#),
    )
    .await;
    ingest(
        &client,
        &base,
        1,
        tuple(r#"["user","person",{},"BORN_IN",{},"1990","literal",{}]"#),
    )
    .await;
    ingest(
        &client,
        &base,
        2,
        tuple(r#"["other","person",{},"PREFERS",{},"tea","drink",{}]"#),
    )
    .await;

    let graph: GraphDto = client
        .get(format!("{base}/graph/1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(graph.nodes.len(), 3, "user + iced_americano + the literal");
    assert_eq!(graph.links.len(), 2);
    let names: Vec<&str> = graph.nodes.iter().map(|n| n.name.as_str()).collect();
    assert!(names.contains(&"user") && names.contains(&"1990"));
    assert!(
        !names.contains(&"other"),
        "another scope's nodes never ride this read"
    );
    assert_eq!(graph.links[0].source, Some(1), "links point by node id");
    assert_eq!(
        (graph.links[0].kind.as_str(), graph.links[0].target),
        ("PREFERS", Some(2))
    );

    let empty: GraphDto = client
        .get(format!("{base}/graph/9"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        empty.nodes.is_empty() && empty.links.is_empty(),
        "an empty scope is a legal empty graph"
    );
}

/// `GET /graph/{scope}/search` — vector landing (ADR-0003). The server built
/// without an engine cannot embed the query, so the door answers the legal
/// EMPTY list (200 + `[]`), never an error: "nothing landed" is a valid
/// retrieval outcome, and the caller injects no memory. The ranked path is
/// covered in mem-core (which owns the engine), so this test pins the door's
/// shape and its no-engine contract.
#[tokio::test]
async fn retrieval_door_answers_an_empty_landing_without_an_engine() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/graph/1/search"))
        .json(&serde_json::json!(["fjall_engine"]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "empty landing is not an error status"
    );
    let answers: Vec<Vec<serde_json::Value>> = response.json().await.unwrap();
    assert_eq!(answers.len(), 1, "one answer per query");
    assert!(answers[0].is_empty(), "no engine = nothing lands");
}

/// `GET /memories/{scope}/recall` — landing + one hop in one answer. Without an
/// engine nothing lands, so the door answers empty seeds AND empty facts
/// (200, never an error). The ranked path lives in mem-core, which owns the
/// engine; this pins the door's shape.
#[tokio::test]
async fn recall_door_answers_an_empty_landing_without_an_engine() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/memories/1/recall"))
        .json(&serde_json::json!(["fjall_engine"]))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let answers: Vec<serde_json::Value> = response.json().await.unwrap();
    let recall = answers[0].clone();
    assert_eq!(recall["format"], "plaintext");
    assert_eq!(recall["text"], "", "nothing landed: an empty text answer");
    assert!(
        recall.get("seeds").is_none() && recall.get("facts").is_none(),
        "the text representation carries no data: {recall}"
    );
}

/// `POST /memories/{scope}/edges/{id}/touch` — the caller reports a TOOL
/// consult (k10r cannot see one from inside), and the fact's new view
/// comes back with the counter bumped and the weight re-folded. An unknown
/// edge, or one from another scope, is a 404: the edge has no scope
/// segment, its endpoints do.
#[tokio::test]
async fn the_touch_door_counts_a_tool_consult_and_404s_for_a_foreign_edge() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let outcome: IngestOutcomeDto = ingest(
        &client,
        &base,
        1,
        tuple(r#"["user","person",{},"PREFERS",{"since":1000},"iced_americano","coffee",{}]"#),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(outcome.outcome, "linked");

    let touched: FactViewDto = client
        .post(format!("{base}/memories/1/edges/1/touch"))
        .json(&serde_json::json!({ "ts_ms": 2000 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(touched.tool_count, 1);
    assert_eq!(
        touched.write_count, 1,
        "the assertion is still the one write"
    );
    assert_eq!(touched.last_read, 2000, "the caller's clock, verbatim");
    assert_eq!(
        touched.weight, 1386,
        "ln(2)×0.5 (one write) + ln(2)×1.5 (the tool consult) — the folded value"
    );

    let unknown = client
        .post(format!("{base}/memories/1/edges/99/touch"))
        .json(&serde_json::json!({ "ts_ms": 2000 }))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);

    let foreign = client
        .post(format!("{base}/memories/2/edges/1/touch"))
        .json(&serde_json::json!({ "ts_ms": 2000 }))
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 404, "another scope's edge is not ours");
}

/// The read door's contract: the query lands, the answer is the landed
/// community's facts plus the RENDERED, id-bearing text — and when nothing
/// can land (no embedding engine attached here), that is a legal EMPTY
/// answer, never an error. The rendered text itself is locked in mem-core's
/// graph suite, which needs the model artifact.
#[tokio::test]
async fn recall_answers_the_rendered_memory_and_the_consolidation_doors_are_gone() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    ingest(
        &client,
        &base,
        1,
        tuple(r#"["alpha","thing",{},"IS",{},"beta","thing",{}]"#),
    )
    .await;

    let answers: Vec<serde_json::Value> = client
        .post(format!("{base}/memories/1/recall?k=3&budget=5"))
        .json(&serde_json::json!(["alpha"]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let answer = answers[0].clone();
    assert_eq!(answer["format"], "plaintext");
    assert_eq!(answer["text"], "", "nothing landed: no text at all");
    assert!(
        answer.get("seeds").is_none() && answer.get("facts").is_none(),
        "a text answer carries the text, not the data: {answer}"
    );
    assert_eq!(answer["shown"], 0);
    assert_eq!(answer["total"], 0);

    // The same question in the data representation: the structure, no text.
    let answers: Vec<serde_json::Value> = client
        .post(format!("{base}/memories/1/recall?k=3&format=struct"))
        .json(&serde_json::json!(["alpha"]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let structured = answers[0].clone();
    assert_eq!(structured["format"], "struct");
    assert_eq!(structured["seeds"].as_array().unwrap().len(), 0);
    assert_eq!(structured["facts"].as_array().unwrap().len(), 0);
    assert!(
        structured.get("text").is_none(),
        "a data answer carries no text: {structured}"
    );

    // The consolidation machinery retired with the capsules: the pull doors
    // answer 404 because they no longer exist (ADR-0010 §5 — the read side
    // renders the community, nothing summarizes it).
    for suffix in [
        "/facts/1/communities",
        "/facts/1/communities/1",
        "/facts/1/communities/1/packet",
    ] {
        let status = client
            .get(format!("{base}{suffix}"))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 404, "{suffix} is not a door any more");
    }
}
