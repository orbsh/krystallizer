//! The task node's contract (ADR-0021): a task is an ORDINARY vertex in the fact
//! graph with a sheet beside it, so what this file pins is that the two halves
//! stay one thing — the vertex is what edges reach, the sheet is what changes,
//! and neither write disturbs the other.
//!
//! The three cases that make the design worth its extra table: a task created by
//! the task door, a task created by a FACT (the resolver opens a sheet too, so
//! "mentioned in a fact" is "exists"), and the state face answering an
//! enumeration without reading every task.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_core::graph::{FactTuple, GraphStore, IngestOutcome};
use mem_core::tasks::{TaskDecl, TaskRead, TASK_KIND};

fn decl(name: &str) -> TaskDecl {
    TaskDecl {
        name: name.to_string(),
        next: None,
        state: None,
    }
}

fn open_store() -> (tempfile::TempDir, GraphStore) {
    let dir = tempfile::tempdir().unwrap();
    let graph = GraphStore::open(dir.path()).unwrap();
    (dir, graph)
}

fn fact(json: &str) -> FactTuple {
    FactTuple::from_json(json).unwrap()
}

#[test]
fn a_task_is_a_vertex_with_a_sheet_beside_it() {
    let (_dir, mut graph) = open_store();
    let landed = graph
        .open_task(1, &decl("ship the face change"), 1_000)
        .unwrap();
    assert!(landed.created);

    // The vertex is an ordinary entity of kind `task` — the same collection as
    // every other node, which is what makes edges to it ordinary edges.
    let row = graph.entity(1, landed.node_id).unwrap().unwrap();
    assert_eq!(row.kind.as_str(), Some(TASK_KIND));
    assert_eq!(row.name, "ship the face change");

    // The sheet is where the mutable half lives.
    let TaskRead::Found(task) = graph.task(1, landed.node_id).unwrap() else {
        panic!("the task reads back");
    };
    assert_eq!(task.state, "open");
    assert_eq!(task.created_at, 1_000);
    assert_eq!(task.next, "", "nothing said yet");
}

#[test]
fn opening_the_same_name_twice_is_one_task() {
    let (_dir, mut graph) = open_store();
    let first = graph.open_task(1, &decl("wire the review"), 1_000).unwrap();
    let again = graph.open_task(1, &decl("wire the review"), 2_000).unwrap();
    assert_eq!(first.node_id, again.node_id);
    assert!(!again.created, "the name is the identity");
    assert_eq!(graph.list_tasks(1, None).unwrap().len(), 1);
    // Re-opening RESTATES the task, it does not move it: the clock stays the one
    // the work was opened with. A retried tool call must not be able to reset a
    // task's birthday.
    let TaskRead::Found(task) = graph.task(1, first.node_id).unwrap() else {
        panic!("the task reads back");
    };
    assert_eq!(task.created_at, 1_000);
}

#[test]
fn reopening_a_task_does_not_drag_it_back_to_open() {
    let (_dir, mut graph) = open_store();
    let landed = graph.open_task(1, &decl("ship the face change"), 1_000).unwrap();
    graph
        .set_task_state(1, landed.node_id, Some("doing"), Some("read the diff"), 2_000)
        .unwrap();

    // The model retries the same open (or a second turn says "this work exists"
    // again). Nothing about the sheet may move.
    let again = graph
        .open_task(
            1,
            &TaskDecl {
                name: "ship the face change".to_string(),
                next: None,
                state: None,
            },
            3_000,
        )
        .unwrap();
    assert!(!again.created);
    let TaskRead::Found(task) = graph.task(1, landed.node_id).unwrap() else {
        panic!("the task reads back");
    };
    assert_eq!(task.state, "doing", "the work in progress stays in progress");
    assert_eq!(task.next, "read the diff", "and keeps its next action");
    assert_eq!(task.updated_at, 2_000, "a restatement is not a move");
}

#[test]
fn a_name_that_is_not_a_task_cannot_be_opened_as_one() {
    let (_dir, mut graph) = open_store();
    graph
        .put_entity(
            1,
            &mem_core::graph::Term {
                value: "alice".to_string(),
                type_label: "person".to_string(),
                props: Default::default(),
            },
        )
        .unwrap();
    let err = graph.open_task(1, &decl("alice"), 1_000).unwrap_err();
    assert!(
        format!("{err}").contains("not a task"),
        "a declaration must not re-label a node: {err}"
    );
    assert!(graph.list_tasks(1, None).unwrap().is_empty());
}

#[test]
fn a_state_change_moves_the_task_and_leaves_the_vertex_alone() {
    let (_dir, mut graph) = open_store();
    let landed = graph
        .open_task(1, &decl("review the kernel"), 1_000)
        .unwrap();
    let before = graph.entity(1, landed.node_id).unwrap().unwrap();

    let TaskRead::Found(task) = graph
        .set_task_state(
            1,
            landed.node_id,
            Some("doing"),
            Some("read the diff"),
            2_000,
        )
        .unwrap()
    else {
        panic!("the move answers with the row");
    };
    assert_eq!(task.state, "doing");
    assert_eq!(task.next, "read the diff");
    assert_eq!(task.updated_at, 2_000);

    // The vertex did not move, and neither did its counters.
    let after = graph.entity(1, landed.node_id).unwrap().unwrap();
    assert_eq!(after, before, "a state change is not a vertex write");

    // The face followed: `open` no longer answers with it, `doing` does.
    assert!(graph.list_tasks(1, Some("open")).unwrap().is_empty());
    let doing = graph.list_tasks(1, Some("doing")).unwrap();
    assert_eq!(doing.len(), 1);
    assert_eq!(doing[0].node_id, landed.node_id);
}

#[test]
fn a_task_mentioned_in_a_fact_exists_and_lists_as_open() {
    let (_dir, mut graph) = open_store();
    // Neither name is known yet: the resolver creates both nodes, and BOTH come
    // back as open tasks — being named as a `task` is being a task, whether the
    // task door or a fact said so.
    let outcome = graph
        .ingest(
            1,
            &fact(r#"["task-12","task",{},"depends_on",{},"task-9","task",{}]"#),
        )
        .unwrap();
    assert!(matches!(outcome, IngestOutcome::Linked { .. }));

    let open = graph.list_tasks(1, Some("open")).unwrap();
    let names: Vec<&str> = open.iter().map(|task| task.name.as_str()).collect();
    assert_eq!(names, vec!["task-12", "task-9"], "both endpoints are tasks");
    for task in &open {
        assert_eq!(task.created_at, 0, "no clock was stated for them");
    }
}

#[test]
fn a_dependency_is_an_ordinary_edge() {
    let (_dir, mut graph) = open_store();
    let later = graph.open_task(1, &decl("task-12"), 1_000).unwrap();
    let earlier = graph.open_task(1, &decl("task-9"), 1_000).unwrap();
    graph
        .ingest(
            1,
            &fact(r#"["task-12","task",{},"depends_on",{},"task-9","task",{}]"#),
        )
        .unwrap();

    // The edge resolves to the SAME vertices the door created — a fact and the
    // task door are two spellings of one node, not two nodes.
    assert_eq!(
        graph.entity_by_name(1, "task-12").unwrap().unwrap().0,
        later.node_id
    );
    // `typed_out` answers EDGE ids; the endpoints come off the row, which is
    // where a dependency lives.
    let out = graph.typed_out(1, later.node_id, "depends_on").unwrap();
    assert_eq!(out.len(), 1);
    let edge = graph.fact_view(out[0]).unwrap().unwrap();
    assert_eq!(
        (edge.src.id.entity_id(), edge.dst.id.entity_id()),
        (Some(later.node_id), Some(earlier.node_id))
    );
    assert_eq!(edge.kind, "depends_on");
    assert_eq!(
        graph.list_tasks(1, None).unwrap().len(),
        2,
        "still two tasks"
    );
}

#[test]
fn a_state_that_is_not_a_state_is_refused() {
    let (_dir, mut graph) = open_store();
    let landed = graph.open_task(1, &decl("typo guard"), 1_000).unwrap();
    let Err(error) = graph.set_task_state(1, landed.node_id, Some("openn"), None, 2_000) else {
        panic!("a typo must not become a state no listing can find");
    };
    assert!(error.to_string().contains("not a task state"), "{error}");

    // The sheet is exactly as it was.
    let TaskRead::Found(task) = graph.task(1, landed.node_id).unwrap() else {
        panic!("the task reads back");
    };
    assert_eq!(task.state, "open");
    assert_eq!(task.updated_at, 1_000);
}

#[test]
fn a_person_is_not_a_task() {
    let (_dir, mut graph) = open_store();
    // A fact names a person; the node exists and has no sheet.
    graph
        .ingest(
            1,
            &fact(r#"["ada","person",{},"OWNS",{},"notes","thing",{}]"#),
        )
        .unwrap();
    let (person, _) = graph.entity_by_name(1, "ada").unwrap().unwrap();
    assert!(matches!(
        graph
            .set_task_state(1, person, Some("done"), None, 1_000)
            .unwrap(),
        TaskRead::NotATask { .. }
    ));
    assert!(matches!(
        graph.task(1, person).unwrap(),
        TaskRead::NotATask { .. }
    ));
    assert!(graph.list_tasks(1, None).unwrap().is_empty());
}

#[test]
fn listing_a_scope_does_not_leak_across_scopes() {
    let (_dir, mut graph) = open_store();
    let mine = graph.open_task(1, &decl("mine"), 1_000).unwrap();
    let theirs = graph.open_task(2, &decl("theirs"), 1_000).unwrap();
    let listed = graph.list_tasks(1, Some("open")).unwrap();
    assert_eq!(
        listed.len(),
        1,
        "the face decodes a scope; it must filter by it"
    );
    assert_eq!(listed[0].node_id, mine.node_id);
    assert_ne!(mine.node_id, theirs.node_id);
}
