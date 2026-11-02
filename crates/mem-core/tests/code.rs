//! Code rows are vertices with a sheet (k10r ADR-0023): the id is still a bare
//! surrogate (no name, no content hash — gravity ADR-0003's reason stands: the
//! caller re-reads the row on every call and the source travels as a call
//! argument, so there is no cache to stale), but the row now sits on a NODE so a
//! fact can point at it (`task --finishes_on--> code`).
//!
//! Three properties are locked: the sheet's key layout, the write/read round
//! trip with ids recovered across a reopen, and the vertex itself — a code row
//! facts cannot reach would leave the binding with nowhere to land.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_core::code::{code_name, CODE_KIND};
use mem_core::{CodeRow, GraphStore};
use okm_core::KeyEncode;

/// Layout lock: the BARE key payload encodes as `[scope 8B][code_id 8B]` — a
/// scoped surrogate identity. The ns (35) + primary slot live in the ADDRESS
/// header the Collection joins at entry time, not in the key struct. Changing
/// the layout breaks this test first, NOT stored data.
#[test]
fn key_layout_is_hex_locked() {
    let key = mem_core::CodeKey {
        scope: 3,
        code_id: 7,
    };
    let buf = key.encode();
    assert_eq!(buf.len(), 16, "scope + code_id");
    assert_eq!(&buf[0..8], &3u64.to_be_bytes(), "scope first");
    assert_eq!(&buf[8..16], &7u64.to_be_bytes(), "then code_id");
}

/// The write/read round trip: ids are assigned in order and the row comes back
/// field for field — language, description, parameter definition and source. The
/// source is re-read on every call (fidelity is the contract); the description
/// and the parameters are what a link is decided from, so they must survive too.
#[test]
fn put_assigns_ids_and_get_round_trips_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = GraphStore::open(dir.path()).unwrap();

    assert_eq!(
        store
            .put_code(
                1,
                "nu",
                "log into a host via ssh",
                r#"{"host": "string", "user": "string"}"#,
                "ssh $args.host",
            )
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .put_code(1, "python", "sum two ints", r#"{"n": "int"}"#, "print(1)")
            .unwrap(),
        2
    );

    let row = store.code(1, 1).expect("the read works").expect("row 1");
    assert_eq!(
        row,
        CodeRow {
            language: "nu".into(),
            description: "log into a host via ssh".into(),
            parameters: r#"{"host": "string", "user": "string"}"#.into(),
            source: "ssh $args.host".into(),
        }
    );
    assert!(
        store.code(1, 3).unwrap().is_none(),
        "unknown id = absent, not an error"
    );
    assert!(
        store.code(2, 1).unwrap().is_none(),
        "a code id is scoped: another corpus does not see it"
    );
}

/// Ids are recovered from storage: a fresh process must not reuse an id (that
/// would silently overwrite stored code).
#[test]
fn ids_are_recovered_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store = GraphStore::open(dir.path()).unwrap();
        assert_eq!(store.put_code(1, "nu", "first", "{}", "first").unwrap(), 1);
        assert_eq!(store.put_code(1, "nu", "second", "{}", "second").unwrap(), 2);
    }
    let mut store = GraphStore::open(dir.path()).unwrap();
    assert_eq!(
        store.put_code(1, "python", "third", "{}", "third").unwrap(),
        3,
        "continues past the max"
    );
    assert_eq!(
        store.code(1, 1).expect("the row reads").expect("row").source,
        "first",
        "id 1 was not overwritten"
    );
}

/// The vertex is the point of the change: a code row has to be a node, or
/// nothing in the graph can name it.
#[test]
fn a_code_row_is_a_vertex_a_fact_can_reach() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = GraphStore::open(dir.path()).unwrap();
    let code_id = store
        .put_code(1, "python", "says hi", "{}", "print('hi')")
        .unwrap();

    let vertex = store
        .code_vertex(1, code_id)
        .unwrap()
        .expect("a code id lives on a vertex");
    assert_eq!(
        store.code_id_of(1, vertex).unwrap(),
        Some(code_id),
        "the id comes back out of the vertex"
    );
    assert_eq!(
        store
            .entity(1, vertex)
            .unwrap()
            .expect("the vertex row")
            .kind
            .as_str(),
        Some(CODE_KIND)
    );

    // An edge from an ordinary task to that vertex — the binding gravity closes.
    let fact = mem_core::FactTuple::from_json(&format!(
        r#"["run it","task",{{}},"finishes_on",{{}},"{}","code",{{}}]"#,
        code_name(code_id)
    ))
    .unwrap();
    store.ingest(1, &fact).unwrap();
    let (task, _) = store
        .entity_by_name(1, "run it")
        .unwrap()
        .expect("the fact opened the task");
    let targets = store
        .targets_of(1, task, mem_core::tasks::FINISHES_ON)
        .unwrap();
    assert_eq!(targets, vec![vertex], "the edge lands on the code vertex");
    assert_eq!(
        store.code_id_of(1, targets[0]).unwrap(),
        Some(code_id),
        "and the number comes back for the caller"
    );
}