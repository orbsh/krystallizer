//! The service's store assembly (mem-core `service`): ONE Fjall database
//! per process (one directory, one lock file) holding two keyspaces —
//! `channel` (its own tree: the log is queue-like and takes the store's
//! highest write rate) and `memory` (flat memory + code + the fact graph,
//! ns-discriminated; the skills will join it).
//!
//! Two properties are locked here: every group comes off that one
//! assembly and works, and a second process-level open of the same
//! directory fails (one writer per directory, by construction — no silent
//! second writer).

use mem_core::{open_service, CodeRow, EntityRow, FactTuple, MemoryRow};
use okm_core::{Document, KeyEncode};

/// All four groups come off one call and serve reads/writes.
#[test]
fn the_service_assembly_serves_every_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut stores = open_service(dir.path()).unwrap();

    stores.channels.append(7, 1000, 0, 0, 0, "hi");
    let code_id = stores
        .code
        .put("nu", "log into a host", "{}", "ssh $args.host");
    let memory_id = stores.memory.store(1, "a fact", 1_000);
    let fact = FactTuple::from_json(
        r#"["galaxy_repo","repository",{},"PIVOTED_TO",{},"k10r_core","system",{}]"#,
    )
    .unwrap();
    let outcome = stores.graph.ingest(1, &fact).unwrap();

    assert_eq!(
        stores
            .channels
            .fetch_full(7, mem_core::DEFAULT_MEMBER)
            .increment
            .len(),
        1
    );
    assert_eq!(stores.code.get(code_id).unwrap().language, "nu");
    assert_eq!(stores.memory.search(1, "fact").len(), 1);
    assert_eq!(memory_id, 1, "the memory table's own id counter works");
    assert_eq!(
        outcome,
        mem_core::IngestOutcome::Linked { edge_id: 1 },
        "the graph's id counters work off the same assembly"
    );
    assert_eq!(stores.graph.entity_by_name(1, "galaxy_repo").unwrap().0, 1);
}

/// The premise that makes ONE keyspace legal for several tables: their ns
/// differ (32 flat memory, 35 code, 36 entities, 37 facts), so the address
/// header the Collection joins can never collide — and the bare key
/// payloads carry no ns themselves (`[user_id 8B][id 8B]` = 16 bytes vs
/// `[code_id 8B]` = 8, `[scope 8B][node_id 8B]` = 16).
#[test]
fn the_memory_keyspace_tables_declare_different_ns() {
    let ns_of = |prefix: &[u8]| u16::from_be_bytes(prefix[0..2].try_into().unwrap());
    assert_eq!(ns_of(MemoryRow::NS_PREFIX), 32);
    assert_eq!(ns_of(CodeRow::NS_PREFIX), 35);
    assert_eq!(ns_of(EntityRow::NS_PREFIX), mem_core::ENTITY_NS);
    assert_eq!(
        ns_of(<mem_core::FactEdge as okm_core::KvGraph>::NS_PREFIX),
        mem_core::FACT_NS
    );

    assert_eq!(mem_core::MemoryKey { user_id: 1, id: 1 }.encode().len(), 16);
    assert_eq!(mem_core::CodeKey { code_id: 1 }.encode().len(), 8);
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
