//! The service's store assembly (mem-core `service`): ONE Fjall database
//! per process (one directory, one lock file) holding two keyspaces —
//! `channel` (its own tree: the log is queue-like and takes the store's
//! highest write rate) and `memory` (code + the fact graph,
//! ns-discriminated; the skills will join it).
//!
//! Two properties are locked here: every group comes off that one
//! assembly and works, and a second process-level open of the same
//! directory fails (one writer per directory, by construction — no silent
//! second writer).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_core::{open_service, CodeRow, EntityRow, FactTuple};
use okm_core::{Document, KeyEncode};

/// A member id in these tests: the identity IS a uuid (ADR-0025 §4), so a small ordinal is spelled
/// as one — the same value the production path carries, not a test-only number.
fn m(n: u8) -> mem_core::Uuid7 {
    let mut bytes = [0u8; 16];
    bytes[15] = n;
    mem_core::Uuid7::from_bytes(bytes)
}

/// Every group comes off one call and serves reads/writes.
#[test]
fn the_service_assembly_serves_every_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut stores = open_service(dir.path()).unwrap();

    // One finished turn: the log's rows arrive under a turn id (ADR-0024).
    let turn = stores
        .channels
        .append_opening(7, 1000, 0, 0, 0, "hi")
        .unwrap()
        .turn_id;
    stores
        .channels
        .close_turn(7, turn, 1000, 1, 0, "", &[(1000, 0)])
        .unwrap();
    // Code is a VERTEX with a sheet now (k10r ADR-0023), so the write also
    // interns a node — the graph's node counter and the code watermark are two
    // numbers, and this test is where that stops being implicit.
    let code_id = stores
        .graph
        .put_code(1, "nu", "log into a host", "{}", "ssh $args.host")
        .unwrap();
    let fact = FactTuple::from_json(
        r#"["galaxy_repo","repository",{},"PIVOTED_TO",{},"k10r_core","system",{}]"#,
    )
    .unwrap();
    let outcome = stores.graph.ingest(1, &fact).unwrap();

    assert_eq!(
        stores.channels.fetch_full(7, m(1)).unwrap().increment.len(),
        1
    );
    assert_eq!(
        stores
            .graph
            .code(1, code_id)
            .expect("the row reads")
            .expect("the row is there")
            .1
            .language,
        "nu"
    );
    // The answer is the fact's HEAD (okm ADR-0067), and that head is what the
    // same assembly reads back: one address, no id counters to agree on.
    assert!(
        matches!(outcome, mem_core::IngestOutcome::Linked { .. }),
        "a fresh fact is a new edge: {outcome:?}"
    );
    assert!(
        stores.graph.fact(outcome.key()).unwrap().is_some(),
        "the head the write answered with is the row the read finds"
    );
    let (code_vertex, _) = stores
        .graph
        .entity_by_name(1, &mem_core::code::code_name(code_id))
        .unwrap()
        .expect("the code rows are reachable as vertices");
    let (entity, _) = stores
        .graph
        .entity_by_name(1, "galaxy_repo")
        .unwrap()
        .unwrap();
    assert_ne!(
        code_vertex, entity,
        "one node per thing: the code vertex is not the fact's entity"
    );
}

/// The premise that makes ONE keyspace legal for several tables: their ns
/// differ (35 code, 36 entities, 37 facts), so the address header the
/// Collection joins can never collide — and the bare key payloads carry no
/// ns themselves (`[scope 8B][code_id 8B]` = 16 vs `[scope 8B][node_id 8B]` = 16).
#[test]
fn the_memory_keyspace_tables_declare_different_ns() {
    let ns_of = |prefix: &[u8]| u16::from_be_bytes(prefix[0..2].try_into().unwrap());
    assert_eq!(ns_of(CodeRow::NS_PREFIX), 35);
    assert_eq!(ns_of(EntityRow::NS_PREFIX), mem_core::ENTITY_NS);
    assert_eq!(
        ns_of(<mem_core::Facts as okm_core::Document>::NS_PREFIX),
        mem_core::FACT_NS
    );

    assert_eq!(
        mem_core::CodeKey {
            scope: 1,
            code_id: 1,
            rev: 1
        }
        .encode()
        .len(),
        24
    );
    assert_eq!(
        mem_core::EntityKey {
            scope: 1,
            node_id: 1
        }
        .encode()
        .len(),
        16
    );
    assert_eq!((mem_core::ENTITY_NS, mem_core::FACT_NS), (36, 37));
}

/// One writer per directory: a second process-level open of the same
/// directory fails on Fjall's lock (after its bounded retries) — the
/// service never silently gains a second writer.
#[test]
fn a_second_open_of_the_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let _first = open_service(dir.path()).unwrap();
    let second = open_service(dir.path());
    assert!(
        second.is_err(),
        "the directory's lock must refuse a second opener, got: {:?}",
        second.map(|_| ())
    );
}
