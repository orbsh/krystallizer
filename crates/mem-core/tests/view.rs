//! Phase 2.5's view layer (ADR-0008 §2, ADR-0006 §2): the projection is
//! assembled in ONE order — head (initialization memory) → the compression
//! point (checkpoint) → the increment — and the role CODE becomes the LLM
//! role NAME at this boundary. Storage stays full; the head is one node's
//! incident facts, not a graph algorithm.
//!
//! The head reads the user node, which today means "node 1 of scope 1"
//! (`DEFAULT_USER_NODE` / `DEFAULT_SCOPE`): the first entity a scope ever
//! resolves takes id 1, so a test can pin the convention without a members
//! table. Phase 2.7 replaces the constants with a member→node mapping.

use mem_core::channel::{kind, role};
use mem_core::graph::FactTuple;
use mem_core::view::{assemble, role_name, DEFAULT_SCOPE, DEFAULT_USER_NODE, Projection};
use mem_core::{open_service, ServiceStores};

fn tuple(text: &str) -> FactTuple {
    FactTuple::from_json(text).expect("the tuple decodes")
}

fn open() -> (tempfile::TempDir, ServiceStores) {
    let dir = tempfile::tempdir().unwrap();
    let stores = open_service(dir.path()).unwrap();
    (dir, stores)
}

fn projection(stores: &mut ServiceStores) -> Projection {
    assemble(
        &mut stores.channels,
        &mut stores.graph,
        7,
        1,
        DEFAULT_SCOPE,
        DEFAULT_USER_NODE,
    )
}

/// An empty corpus is a legal answer, not an error: no head, no summary,
/// no increment (ADR-0008 §2's "nothing landed" posture).
#[test]
fn an_empty_projection_is_empty_not_wrong() {
    let (_d, mut stores) = open();
    let p = projection(&mut stores);
    assert!(p.head.is_empty(), "no facts in the scope yet");
    assert_eq!(p.view.coverage_ts, 0);
    assert!(p.view.summary.is_none());
    assert!(p.view.increment.is_empty());
}

/// The head is the user node's incident facts in BOTH directions, each
/// edge once — a node is useful through the facts pointing AT it too, and
/// a fact between the node and itself must not appear twice.
#[test]
fn head_is_both_directions_deduplicated() {
    let (_d, mut stores) = open();
    let scope = DEFAULT_SCOPE;
    // "用户" resolves first → node 1 (the default user node).
    stores
        .graph
        .ingest(scope, &tuple(r#"["用户","person",{},"PREFERS",{},"简洁","style",{}]"#))
        .unwrap();
    // Same node as the TAIL: an incoming edge.
    stores
        .graph
        .ingest(scope, &tuple(r#"["k10r","system",{},"OWNED_BY",{},"用户","person",{}]"#))
        .unwrap();
    // A second outgoing edge.
    stores
        .graph
        .ingest(scope, &tuple(r#"["用户","person",{},"WRITES",{},"Rust","language",{}]"#))
        .unwrap();

    let p = projection(&mut stores);
    assert_eq!(p.head.len(), 3, "three edges, one direction each");
    let user = 1u64;
    assert!(
        p.head.iter().any(|f| f.dst.node_id == user),
        "the incoming fact must be in the head"
    );
    assert_eq!(
        p.head.iter().filter(|f| f.src.node_id == user).count(),
        2,
        "both outgoing facts are in the head"
    );
    // Weights are fold-equal here, so the tiebreak (edge id, ascending)
    // decides — repeated reads answer identically.
    let ids: Vec<u64> = p.head.iter().map(|f| f.edge_id).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "stable order: equal weights keep edge id order");
    let weights: Vec<u32> = p.head.iter().map(|f| f.edge.weight).collect();
    assert!(
        weights.is_sorted_by(|a, b| a >= b),
        "weight-descending"
    );
}

/// The order is the field order and the checkpoint still governs the
/// increment: everything at or before `coverage_ts` is the summary's, not
/// the increment's.
#[test]
fn increment_starts_after_the_compression_point() {
    let (_d, mut stores) = open();
    let channel = 7u64;
    let _member = 1u64;
    stores.channels.append(channel, 1000, 0, role::USER, kind::TEXT, "hi");
    stores
        .channels
        .append(channel, 1001, 0, role::ASSISTANT, kind::TEXT, "hello");
    stores
        .channels
        .append_checkpoint(channel, (1000, 0), "greeting done", &[]);

    let p = projection(&mut stores);
    assert_eq!(p.view.coverage_ts, 1000);
    assert_eq!(p.view.summary.as_deref(), Some("greeting done"));
    assert_eq!(p.view.increment.len(), 1, "only strictly-after rows");
    assert_eq!(p.view.increment[0].0.ts_ms, 1001);
}

/// The role mapping lives on this side of the wire (ADR-0008 §2), and an
/// unknown code is surfaced as `None` rather than guessed at.
#[test]
fn role_codes_map_to_llm_roles() {
    assert_eq!(role_name(role::USER), Some("user"));
    assert_eq!(role_name(role::ASSISTANT), Some("assistant"));
    assert_eq!(role_name(role::TOOL), Some("tool"));
    assert_eq!(role_name(role::SYSTEM), Some("system"));
    assert_eq!(role_name(9), None, "the store never polices the code");
}

/// The head is FROZEN at the compression (ADR-0008 §2, ruled 2026-10-20: the
/// snapshot is the FACT SET, re-rendered per request — lossless, and `struct`
/// keeps meaning what it says). Before the first compression the live
/// traversal answers; after one, the graph may move and the head must not.
#[test]
fn the_head_is_frozen_at_the_compression() {
    let (_d, mut stores) = open();
    let scope = DEFAULT_SCOPE;
    stores
        .graph
        .ingest(
            scope,
            &tuple(r#"["用户","person",{},"PREFERS",{},"简洁","style",{}]"#),
        )
        .unwrap();

    let live = projection(&mut stores);
    assert!(
        !live.head.is_empty(),
        "before any compression the live traversal answers"
    );
    let head_ids: Vec<u64> = live.head.iter().map(|f| f.edge_id).collect();

    let verdict = mem_core::compress::Verdict {
        checkpoint_ts: 1000,
        checkpoint_sub: 0,
        trigger: mem_core::compress::Trigger::Budget,
        assembled_bytes: 60_000,
        context_bytes: 131_072,
        budget_percent: 50,
        cache_ttl_ms: 300_000,
        idle_ms: Some(1_000),
        max_gap_ms: Some(700),
    };
    stores
        .channels
        .open_checkpoint(7, (verdict.checkpoint_ts, verdict.checkpoint_sub), &head_ids);
    stores
        .channels
        .complete_checkpoint(7, "the summary")
        .expect("the node is open");

    // The graph gains a preference AFTER the compression: a live traversal
    // would now include it, the frozen head must not.
    stores
        .graph
        .ingest(
            scope,
            &tuple(r#"["用户","person",{},"PREFERS",{},"浓咖啡","drink",{}]"#),
        )
        .unwrap();

    let frozen = projection(&mut stores);
    let frozen_ids: Vec<u64> = frozen.head.iter().map(|f| f.edge_id).collect();
    assert_eq!(
        frozen_ids, head_ids,
        "the head is the set frozen at the compression, re-rendered — not the live traversal"
    );
}