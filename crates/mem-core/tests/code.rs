//! Code rows are vertices with a versioned sheet (k10r ADR-0023): the id is
//! still a surrogate (no content hash — gravity ADR-0003's reason stands: the
//! caller re-reads the row on every call and the source travels as a call
//! argument, so there is no cache to stale), the row sits on a NODE so a fact
//! can point at it (`task --finishes_on--> code`), and its payload is a CHAIN of
//! immutable versions.
//!
//! Why a chain: a script gets corrected, and what ran must stay readable
//! afterwards. The current version is therefore the last key of the script's
//! range, not a pointer — nothing can fall out of step with the rows.
//!
//! Four properties are locked: the sheet's key layout, the write/read round
//! trip with ids recovered across a reopen, the revision chain (append-only, and
//! "latest" resolved by key order), and the vertex itself — a code row facts
//! cannot reach would leave the binding with nowhere to land.
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

/// Layout lock: the BARE key payload encodes as
/// `[scope 8B][code_id 8B][rev 8B]` — a scoped surrogate identity with a version
/// axis. The ns (35) + primary slot live in the ADDRESS header the Collection
/// joins at entry time, not in the key struct. Changing the layout breaks this
/// test first, NOT stored data.
#[test]
fn key_layout_is_hex_locked() {
    let key = mem_core::CodeKey {
        scope: 3,
        code_id: 7,
        rev: 2,
    };
    let buf = key.encode();
    assert_eq!(buf.len(), 24, "scope + code_id + rev");
    assert_eq!(&buf[0..8], &3u64.to_be_bytes(), "scope first");
    assert_eq!(&buf[8..16], &7u64.to_be_bytes(), "then code_id");
    assert_eq!(&buf[16..24], &2u64.to_be_bytes(), "then the revision");
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

    let (rev, row) = store.code(1, 1).expect("the read works").expect("row 1");
    assert_eq!(rev, 1, "a new script starts at its first version");
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

/// The chain: a revision appends and never overwrites, so the version that ran
/// yesterday still reads back today — while the ordinary read answers the newest
/// one, resolved by key order rather than by a pointer somebody maintains.
#[test]
fn a_revision_appends_and_the_latest_is_the_last_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = GraphStore::open(dir.path()).unwrap();
    let code_id = store
        .put_code(1, "python", "count lines", "{}", "print(1)")
        .unwrap();

    assert_eq!(
        store
            .put_code_revision(1, code_id, "python", "count lines", "{}", "print(2)")
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .put_code_revision(1, code_id, "python", "count lines", "{}", "print(3)")
            .unwrap(),
        3
    );

    let (rev, row) = store.code(1, code_id).unwrap().expect("the script reads");
    assert_eq!((rev, row.source.as_str()), (3, "print(3)"), "latest wins");

    for (rev, source) in [(1, "print(1)"), (2, "print(2)"), (3, "print(3)")] {
        assert_eq!(
            store
                .code_at(1, code_id, rev)
                .unwrap()
                .expect("every version stays readable")
                .source,
            source,
            "version {rev} is immutable"
        );
    }
    assert!(
        store.code_at(1, code_id, 4).unwrap().is_none(),
        "a version nobody wrote is absent, not the latest"
    );

    // A second script's version 1 is not "the latest of the first": the range is
    // per script, and its end is the next script's first key.
    let other = store
        .put_code(1, "python", "other", "{}", "print('other')")
        .unwrap();
    assert_eq!(store.code(1, other).unwrap().expect("reads").0, 1);
    assert_eq!(store.code(1, code_id).unwrap().expect("still").0, 3);
}

/// The revision counter is derived from the keys, not kept in memory: a fresh
/// process must continue the chain instead of restating version 2 over a version
/// that may already have run.
#[test]
fn revisions_continue_across_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let code_id;
    {
        let mut store = GraphStore::open(dir.path()).unwrap();
        code_id = store.put_code(1, "nu", "x", "{}", "one").unwrap();
        store
            .put_code_revision(1, code_id, "nu", "x", "{}", "two")
            .unwrap();
    }
    let mut store = GraphStore::open(dir.path()).unwrap();
    assert_eq!(
        store
            .put_code_revision(1, code_id, "nu", "x", "{}", "three")
            .unwrap(),
        3,
        "continues past the stored versions"
    );
    assert_eq!(store.code_at(1, code_id, 2).unwrap().unwrap().source, "two");
}

/// A revision of an id the store never wrote is REFUSED by id: a phantom script
/// would answer calls with a body nobody delivered.
#[test]
fn a_revision_of_a_script_that_does_not_exist_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = GraphStore::open(dir.path()).unwrap();
    let err = store
        .put_code_revision(1, 99, "nu", "x", "{}", "print(1)")
        .expect_err("no script 99");
    assert!(
        err.to_string().contains("99"),
        "the refusal names the id: {err}"
    );
    assert!(
        store.code(1, 99).unwrap().is_none(),
        "and nothing was created"
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
        assert_eq!(
            store.put_code(1, "nu", "second", "{}", "second").unwrap(),
            2
        );
    }
    let mut store = GraphStore::open(dir.path()).unwrap();
    assert_eq!(
        store.put_code(1, "python", "third", "{}", "third").unwrap(),
        3,
        "continues past the max"
    );
    assert_eq!(
        store
            .code(1, 1)
            .expect("the row reads")
            .expect("row")
            .1
            .source,
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

    // A revision does not move the address: the binding a task wrote before the
    // correction still lands on the same script.
    store
        .put_code_revision(1, code_id, "python", "says hi", "{}", "print('hi!')")
        .unwrap();
    assert_eq!(
        store.code_vertex(1, code_id).unwrap(),
        Some(vertex),
        "the vertex survives a revision"
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

/// The use counters ride the VERTEX, on the same three declared fields skills
/// carry (ADR-0018): an attempt is one call, a success is one call and one ok, so
/// `calls > ok` is the visible state of "ran and broke". They count the SCRIPT —
/// the row several skills and callers meet on — and nothing here counts a caller.
#[test]
fn an_invocation_counts_the_script_on_its_vertex() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = GraphStore::open(dir.path()).unwrap();
    let code_id = store
        .put_code(1, "python", "generate a pdf", "{}", "print(1)")
        .unwrap();

    let attempt = store
        .touch_code(
            1,
            code_id,
            mem_core::skills::TouchPhase::Attempt,
            1_700_000_000_000,
        )
        .unwrap();
    let mem_core::code::CodeTouchOutcome::Counted(attempt) = attempt else {
        panic!("a written script is countable: {attempt:?}");
    };
    assert_eq!(
        (attempt.calls, attempt.ok),
        (1, 0),
        "an attempt is not a success"
    );

    let success = store
        .touch_code(
            1,
            code_id,
            mem_core::skills::TouchPhase::Success,
            1_700_000_002_000,
        )
        .unwrap();
    let mem_core::code::CodeTouchOutcome::Counted(success) = success else {
        panic!("the second record counts too: {success:?}");
    };
    assert_eq!((success.calls, success.ok), (1, 1));

    // Two scripts, one caller: the counters do not bleed across rows.
    let other = store
        .put_code(1, "python", "log in", "{}", "print(2)")
        .unwrap();
    store
        .touch_code(
            1,
            code_id,
            mem_core::skills::TouchPhase::Attempt,
            1_700_000_003_000,
        )
        .unwrap();
    let fresh = store
        .touch_code(
            1,
            other,
            mem_core::skills::TouchPhase::Attempt,
            1_700_000_004_000,
        )
        .unwrap();
    let mem_core::code::CodeTouchOutcome::Counted(fresh) = fresh else {
        panic!("the new row counts from zero: {fresh:?}");
    };
    assert_eq!((fresh.calls, fresh.ok), (1, 0));
}

/// An id nobody wrote is absent, and an id that is a skill (or any other kind) is
/// refused WITH the kind it is: a caller's wrong id and a wrong kind are
/// different mistakes, and a counter must never invent the row it counts.
#[test]
fn a_touch_names_what_is_missing_or_mistyped() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = GraphStore::open(dir.path()).unwrap();

    let absent = store
        .touch_code(1, 99, mem_core::skills::TouchPhase::Attempt, 1)
        .unwrap();
    assert!(
        matches!(absent, mem_core::code::CodeTouchOutcome::Absent),
        "{absent:?}"
    );

    // The vertex is addressed BY NAME (`code-{id}`), so a collision is possible in
    // exactly one way: somebody declares a skill whose name is `code-7`. Then the
    // node exists and is not code — the guard says which kind it met instead of
    // bumping a stranger's counters.
    store
        .put_skill(
            1,
            &mem_core::skills::SkillDecl {
                name: "code-7".into(),
                title: "A skill that happens to be named like code".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let mismatch = store
        .touch_code(1, 7, mem_core::skills::TouchPhase::Attempt, 1)
        .unwrap();
    match mismatch {
        mem_core::code::CodeTouchOutcome::NotACode { kind } => assert_eq!(kind, "skill"),
        other => panic!("a skill is not a code row: {other:?}"),
    }
}
