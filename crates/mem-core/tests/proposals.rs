//! Proposals (ADR-0016): the write side of the skill loop. A walk of a
//! composition is counted, never promoted — promotion is a human's declaration,
//! and its absence from the model's view is what makes the distinction real.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_core::graph::GraphStore;
use mem_core::proposals::{ProposalDecl, ProposalRead, PROPOSAL_KIND};
use mem_core::skills::{SkillDecl, SkillRead, DEFAULT_LIMIT};

fn walk(key: &str, ts_ms: u64) -> ProposalDecl {
    ProposalDecl {
        key: key.to_string(),
        steps: r#"[{"code_id": 4, "note": "fetch"}, {"code_id": 7, "note": "format"}]"#.to_string(),
        ts_ms,
    }
}

#[test]
fn walking_the_same_composition_counts_and_never_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    let first = graph.propose(1, &walk("4+7", 1_000)).unwrap();
    assert!(first.created, "the first walk creates the row");
    assert_eq!(first.count, 1, "seen once");

    let second = graph.propose(1, &walk("4+7", 2_000)).unwrap();
    assert!(
        !second.created,
        "the same composition, not a second proposal"
    );
    assert_eq!(second.node_id, first.node_id, "the same row");
    assert_eq!(second.count, 2, "counted, not duplicated");

    // A different order is a DIFFERENT composition: `7+4` is not `4+7`.
    let other = graph.propose(1, &walk("7+4", 3_000)).unwrap();
    assert_ne!(other.node_id, first.node_id);
    assert_eq!(other.count, 1);
}

#[test]
fn a_proposal_without_a_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    let error = graph.propose(1, &walk("  ", 1_000)).unwrap_err();
    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("needs a key"),
        "the refusal names what is missing: {rendered}"
    );
}

#[test]
fn the_queue_is_ordered_by_repetition_and_drops_promoted_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    for ts in [1_000, 2_000, 3_000] {
        graph.propose(1, &walk("4+7", ts)).unwrap();
    }
    graph.propose(1, &walk("5+6", 4_000)).unwrap();

    let queue = graph.proposals(1, 1).unwrap();
    assert_eq!(queue.len(), 2);
    assert_eq!(queue[0].key, "4+7", "most-walked first");
    assert_eq!(queue[0].count, 3);
    assert_eq!(queue[0].last_seen, 3_000, "the caller's clock, as given");
    assert_eq!(queue[1].key, "5+6");

    // A threshold is the human's filter, not the store's opinion.
    let repeated = graph.proposals(1, 2).unwrap();
    assert_eq!(repeated.len(), 1, "only what repeated");
    assert_eq!(repeated[0].key, "4+7");

    // Promotion takes the row out of the queue without erasing it.
    let promoted = graph
        .promote(
            1,
            queue[0].node_id,
            "fetch-then-format",
            "Fetch and format",
            "",
        )
        .unwrap();
    assert!(promoted.created);

    let after = graph.proposals(1, 1).unwrap();
    assert_eq!(after.len(), 1, "promoted rows leave the queue");
    assert_eq!(after[0].key, "5+6");

    match graph.proposal(1, queue[0].node_id).unwrap() {
        ProposalRead::Found(row) => {
            assert_eq!(
                row.promoted_to, "fetch-then-format",
                "the record of what it became"
            );
            assert_eq!(row.count, 3, "the count survives promotion");
            assert!(!row.steps.is_empty(), "and so do the steps it suggested");
        }
        other => panic!("the proposal must still be readable: {other:?}"),
    }
}

#[test]
fn promotion_copies_the_steps_into_a_real_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    let landed = graph.propose(1, &walk("4+7", 1_000)).unwrap();
    let promoted = graph
        .promote(
            1,
            landed.node_id,
            "fetch-then-format",
            "Fetch, then format",
            "Two steps.",
        )
        .unwrap();

    match graph.skill(1, promoted.skill_node_id).unwrap() {
        SkillRead::Found(skill) => {
            assert_eq!(skill.name, "fetch-then-format");
            assert_eq!(skill.title, "Fetch, then format");
            assert_eq!(
                skill.steps,
                walk("4+7", 0).steps,
                "the steps are the run that was counted, copied rather than retyped"
            );
        }
        other => panic!("the promoted skill must read back as a skill: {other:?}"),
    }

    // And the resolver now answers it — the proposal became visible only by the
    // human's act (kind face, no engine needed).
    let resolved = graph
        .resolve_skills(1, "fetch and format", DEFAULT_LIMIT)
        .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name, "fetch-then-format");
}

#[test]
fn a_proposal_is_invisible_to_the_resolver_until_it_is_promoted() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    graph.propose(1, &walk("4+7", 1_000)).unwrap();
    let before = graph
        .resolve_skills(1, "fetch and format", DEFAULT_LIMIT)
        .unwrap();
    assert!(
        before.is_empty(),
        "a draft is not a skill: the resolver keeps one kind, and this is not it"
    );

    // The node is a real row in the graph, with its own kind.
    let queue = graph.proposals(1, 1).unwrap();
    let row = graph.entity(1, queue[0].node_id).unwrap().unwrap();
    assert_eq!(row.kind.as_str(), Some(PROPOSAL_KIND));
}

#[test]
fn promoting_is_once_and_nothing_else_qualifies() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    let landed = graph.propose(1, &walk("4+7", 1_000)).unwrap();
    graph
        .promote(1, landed.node_id, "fetch-then-format", "", "")
        .unwrap();

    let again = graph
        .promote(1, landed.node_id, "fetch-then-format-again", "", "")
        .unwrap_err();
    assert!(
        format!("{again:?}").contains("already a skill"),
        "a second promotion would declare a skill nobody reviewed: {again:?}"
    );

    // A skill node is not a proposal, and an absent id is absent — the two are
    // told apart, because a caller's mistake differs from a wrong id.
    let skill = graph
        .put_skill(
            1,
            &SkillDecl {
                name: "hand-written".to_string(),
                ..SkillDecl::default()
            },
        )
        .unwrap();
    match graph.proposal(1, skill.node_id).unwrap() {
        ProposalRead::NotAProposal { kind } => assert_eq!(kind, "skill"),
        other => panic!("a skill must not read as a proposal: {other:?}"),
    }
    match graph.proposal(1, 9_999).unwrap() {
        ProposalRead::Absent => {}
        other => panic!("an unknown id is absent: {other:?}"),
    }
}
