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
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_core::channel::{kind, role};
use mem_core::graph::FactTuple;
use mem_core::view::{assemble, role_name, Projection, DEFAULT_SCOPE};
use mem_core::{open_service, ServiceStores};

/// A member id in these tests: the identity IS a uuid (ADR-0025 §4), so a small ordinal is spelled
/// as one — the same value the production path carries, not a test-only number.
fn m(n: u8) -> mem_core::Uuid7 {
    let mut bytes = [0u8; 16];
    bytes[15] = n;
    mem_core::Uuid7::from_bytes(bytes)
}

fn tuple(text: &str) -> FactTuple {
    FactTuple::from_json(text).expect("the tuple decodes")
}

fn open() -> (tempfile::TempDir, ServiceStores) {
    let dir = tempfile::tempdir().unwrap();
    let stores = open_service(dir.path()).unwrap();
    (dir, stores)
}

fn at(turn_id: u64, ts_ms: u64, sub: u16) -> mem_core::Pointer {
    mem_core::Pointer {
        turn_id,
        ts_ms,
        sub,
    }
}

/// One finished exchange: its own turn, one record, closed with that record
/// kept — what a completed round leaves in the log (ADR-0024).
#[allow(clippy::too_many_arguments)] // a log record IS this wide
fn say_from(
    stores: &mut ServiceStores,
    channel: u64,
    ts_ms: u64,
    sub: u16,
    sender: mem_core::Uuid7,
    end_ts_ms: u64,
    role: u8,
    kind: u8,
    text: &str,
) -> mem_core::MessageKey {
    let key = stores
        .channels
        .append_opening_from(channel, ts_ms, sub, sender, end_ts_ms, role, kind, text)
        .unwrap();
    let turn = key.turn_id;
    stores
        .channels
        .close_turn(channel, turn, ts_ms, sub + 1, role, "", &[(ts_ms, sub)])
        .unwrap();
    key
}

fn say(
    stores: &mut ServiceStores,
    channel: u64,
    ts_ms: u64,
    sub: u16,
    role: u8,
    kind: u8,
    text: &str,
) -> mem_core::MessageKey {
    say_from(stores, channel, ts_ms, sub, m(1), 0, role, kind, text)
}

fn projection(stores: &mut ServiceStores) -> Projection {
    assemble(
        &mut stores.channels,
        &mut stores.graph,
        7,
        m(1),
        DEFAULT_SCOPE,
    )
    .expect("the projection assembles")
}

/// An empty corpus is a legal answer, not an error: no head, no summary,
/// no increment (ADR-0008 §2's "nothing landed" posture).
#[test]
fn an_empty_projection_is_empty_not_wrong() {
    let (_d, mut stores) = open();
    let p = projection(&mut stores);
    assert!(p.head.is_empty(), "no facts in the scope yet");
    assert_eq!(p.view.coverage, mem_core::Pointer::NONE);
    assert!(p.view.summary.is_none());
    assert!(p.view.increment.is_empty());
}

/// The head is the user node's incident facts in BOTH directions, each
/// edge once — a node is useful through the facts pointing AT it too, and
/// a fact between the node and itself must not appear twice.
#[test]
fn head_is_both_directions_deduplicated() {
    let (_d, mut stores) = open();
    // The reference spells the member id as the uuid it is (ADR-0025 §4).
    let user_ref = format!("member-{}", m(1));
    let scope = DEFAULT_SCOPE;
    // "{user_ref}" resolves first → node 1 (the default user node).
    stores
        .graph
        .ingest(
            scope,
            &tuple(&format!(
                r#"["{user_ref}","person",{{}},"PREFERS",{{}},"简洁","style",{{}}]"#
            )),
        )
        .unwrap();
    // Same node as the TAIL: an incoming edge.
    stores
        .graph
        .ingest(
            scope,
            &tuple(&format!(
                r#"["k10r","system",{{}},"OWNED_BY",{{}},"{user_ref}","person",{{}}]"#
            )),
        )
        .unwrap();
    // A second outgoing edge.
    stores
        .graph
        .ingest(
            scope,
            &tuple(&format!(
                r#"["{user_ref}","person",{{}},"WRITES",{{}},"Rust","language",{{}}]"#
            )),
        )
        .unwrap();

    let p = projection(&mut stores);
    assert_eq!(p.head.len(), 3, "three edges, one direction each");
    let user = 1u64;
    assert!(
        p.head.iter().any(|f| f.dst.id.entity_id() == Some(user)),
        "the incoming fact must be in the head"
    );
    assert_eq!(
        p.head
            .iter()
            .filter(|f| f.src.id.entity_id() == Some(user))
            .count(),
        2,
        "both outgoing facts are in the head"
    );
    // Weights are fold-equal here, so the tiebreak (the head's own bytes,
    // ascending) decides — repeated reads answer identically.
    let keys: Vec<_> = p.head.iter().map(|f| f.key.clone()).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted, "stable order: equal weights keep head order");
    let weights: Vec<u32> = p.head.iter().map(|f| f.edge.weight).collect();
    assert!(weights.is_sorted_by(|a, b| a >= b), "weight-descending");
}

/// The order is the field order and the checkpoint still governs the
/// increment: everything at or before `coverage_ts` is the summary's, not
/// the increment's.
#[test]
fn increment_starts_after_the_compression_point() {
    let (_d, mut stores) = open();
    let channel = 7u64;
    let _member = 1u64;
    say(&mut stores, channel, 1000, 0, role::USER, kind::TEXT, "hi");
    say(
        &mut stores,
        channel,
        1001,
        0,
        role::ASSISTANT,
        kind::TEXT,
        "hello",
    );
    stores
        .channels
        .append_checkpoint(channel, at(1, 1000, 0), "greeting done", &[])
        .unwrap();

    let p = projection(&mut stores);
    assert_eq!(p.view.coverage, at(1, 1000, 0));
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
    let user_ref = format!("member-{}", m(1));
    let (_d, mut stores) = open();
    let scope = DEFAULT_SCOPE;
    stores
        .graph
        .ingest(
            scope,
            &tuple(&format!(
                r#"["{user_ref}","person",{{}},"PREFERS",{{}},"简洁","style",{{}}]"#
            )),
        )
        .unwrap();

    let live = projection(&mut stores);
    assert!(
        !live.head.is_empty(),
        "before any compression the live traversal answers"
    );
    let head_ids: Vec<Vec<u8>> = live.head.iter().map(|f| f.key.wire()).collect();

    let verdict = mem_core::compress::Verdict {
        checkpoint: at(1, 1000, 0),
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
        .open_checkpoint(7, verdict.checkpoint, &head_ids)
        .unwrap();
    stores
        .channels
        .complete_checkpoint(7, "the summary", None)
        .expect("the node is open")
        .expect("a node is open");

    // The graph gains a preference AFTER the compression: a live traversal
    // would now include it, the frozen head must not.
    stores
        .graph
        .ingest(
            scope,
            &tuple(&format!(
                r#"["{user_ref}","person",{{}},"PREFERS",{{}},"浓咖啡","drink",{{}}]"#
            )),
        )
        .unwrap();

    let frozen = projection(&mut stores);
    let frozen_ids: Vec<Vec<u8>> = frozen.head.iter().map(|f| f.key.wire()).collect();
    assert_eq!(
        frozen_ids, head_ids,
        "the head is the set frozen at the compression, re-rendered — not the live traversal"
    );
}
/// The LLM's side of the channel (ADR-0009 §2): a burst of user messages
/// between two replies merges into ONE turn, each line carries `[name(id=N)]`,
/// and a message typed DURING a generation sorts before that reply (which keeps
/// the clock it finished on).
#[test]
fn conversation_merges_a_user_burst_and_keeps_the_replys_end() {
    let (_d, mut stores) = open();
    let ch = 7u64;
    stores
        .channels
        .put_member(m(10), "小王", mem_core::member_role::HUMAN)
        .unwrap();
    stores
        .channels
        .put_member(m(11), "小李", mem_core::member_role::HUMAN)
        .unwrap();
    stores
        .channels
        .put_member(m(2), "k10r", mem_core::member_role::AGENT)
        .unwrap();

    say_from(
        &mut stores,
        ch,
        1_000,
        0,
        m(10),
        0,
        role::USER,
        kind::TEXT,
        "在吗",
    );
    say_from(
        &mut stores,
        ch,
        1_050,
        0,
        m(11),
        0,
        role::USER,
        kind::TEXT,
        "问个事",
    );
    // The reply is keyed by its FIRST token and ends 4 s later.
    say_from(
        &mut stores,
        ch,
        1_100,
        0,
        m(2),
        5_000,
        role::ASSISTANT,
        kind::TEXT,
        "在，你说",
    );
    // Typed while that reply was still streaming.
    say_from(
        &mut stores,
        ch,
        1_200,
        0,
        m(10),
        0,
        role::USER,
        kind::TEXT,
        "急",
    );

    let turns = mem_core::view::conversation(&mut stores.channels, ch, m(1)).unwrap();
    assert_eq!(
        turns.len(),
        3,
        "burst → one user turn, reply, then the late one"
    );
    assert_eq!(turns[0].role, "user");
    assert_eq!(
        turns[0].text,
        format!("[小王(id={})] 在吗\n[小李(id={})] 问个事", m(10), m(11)),
        "one turn, attributed per line, ids for the model to look up"
    );
    assert_eq!(turns[0].senders, vec![m(10), m(11)]);
    assert_eq!((turns[0].start_ts, turns[0].end_ts), (1_000, 1_050));

    assert_eq!(turns[1].role, "assistant");
    assert_eq!(turns[1].text, format!("[k10r(id={})] 在，你说", m(2)));
    assert_eq!(
        (turns[1].start_ts, turns[1].end_ts),
        (1_100, 5_000),
        "ordered by first token, but the end of the generation is kept"
    );

    assert_eq!(turns[2].role, "user");
    assert_eq!(turns[2].text, format!("[小王(id={})] 急", m(10)));
    assert!(
        turns[2].start_ts < turns[1].end_ts,
        "it arrived inside the reply's span — visible, not reordered away"
    );
}

/// The single-party shape needs no setup: member 1 exists from the first write,
/// named from the one constant, so nothing is ever unattributed.
#[test]
fn the_default_member_is_seeded_on_first_touch() {
    let (_d, mut stores) = open();
    let ch = 9u64;
    assert!(
        stores.channels.member(m(1)).unwrap().is_none(),
        "nothing yet"
    );
    say(&mut stores, ch, 10, 0, role::USER, kind::TEXT, "hi");
    let seeded = stores
        .channels
        .member(m(1))
        .expect("no store failure")
        .expect("member 1 seeded");
    assert_eq!(seeded.display_name, mem_core::channel::DEFAULT_MEMBER_NAME);
    assert_eq!(seeded.role, mem_core::member_role::HUMAN);

    let turns = mem_core::view::conversation(&mut stores.channels, ch, m(1)).unwrap();
    assert_eq!(
        turns[0].text,
        format!(
            "[{}(id={})] hi",
            mem_core::channel::DEFAULT_MEMBER_NAME,
            m(1)
        )
    );
}

/// The head is EVERY member's facts, not one user node's (ADR-0009 §1): a
/// member's facts hang on `member-{id}`, so two members bring two sets,
/// deduplicated by edge id and sorted by weight.
#[test]
fn the_head_comes_from_every_member_of_the_channel() {
    let (_d, mut stores) = open();
    let scope = DEFAULT_SCOPE;
    for (member, name, drink) in [(m(10), "小王", "美式"), (m(11), "小李", "拿铁")] {
        // The head asks the RELATION who is in the channel (ADR-0051): a person needs both an
        // identity row and a membership row, which `join` writes in one commit.
        stores
            .channels
            .join(
                7, // the channel `projection()` assembles
                member,
                name,
                mem_core::member_role::HUMAN,
                0,
            )
            .unwrap();
        let node = mem_core::channel::member_node(member);
        stores
            .graph
            .ingest(
                scope,
                &tuple(&format!(
                    r#"["{node}","person",{{}},"PREFERS",{{}},"{drink}","drink",{{}}]"#
                )),
            )
            .unwrap();
    }

    let p = projection(&mut stores);
    let names: Vec<String> = p.head.iter().map(|f| f.src.name.clone()).collect();
    let expected = vec![format!("member-{}", m(10)), format!("member-{}", m(11))];
    assert_eq!(
        names, expected,
        "both members' facts, in member-id order at equal weight"
    );
    assert_eq!(p.head[0].dst.name, "美式");
    assert_eq!(p.head[1].dst.name, "拿铁");
}
