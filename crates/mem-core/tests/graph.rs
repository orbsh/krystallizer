//! The fact graph's contract (ADR-0010 §1 write side): the 8-slot tuple's
//! decode, name/scope resolution, the parallel-vs-functional write path,
//! literal nodes, and the two layout locks (the entity key's bytes and the
//! fact edge's declared-attribute payload).
//!
//! The round trips are locked, not just the bytes: a byte-pass with an
//! empty query is the silent-corruption shape this file exists to catch.

use mem_core::graph::{FactTuple, GraphStore, IngestError, IngestOutcome};
use mem_core::{EntityKey, EntityRow, FactEdge, LITERAL_KIND};
use okm_core::obj_dynamic::DynamicValue;
use okm_core::{Document, KeyEncode, NodeRef};

fn tuple(text: &str) -> FactTuple {
    FactTuple::from_json(text).expect("the tuple decodes")
}

/// The canonical example from PROMPTS §1 (the paste-source of the form).
const PIVOT: &str = r#"["galaxy_repo","repository",{},"PIVOTED_TO",{"reason":"network_graph_too_heavy"},"k10r_core","system",{}]"#;

/// Slots are positional and the arity is fixed: head value / head type /
/// head props, rel name / rel props, tail value / tail type / tail props
/// (ADR-0004). A malformed tuple is an error, never a half-stored fact.
#[test]
fn fact_tuple_decode_locks_the_slots() {
    let fact = tuple(PIVOT);
    assert_eq!(fact.head.value, "galaxy_repo");
    assert_eq!(fact.head.type_label, "repository");
    assert_eq!(fact.rel.name, "PIVOTED_TO");
    assert_eq!(
        fact.rel.props.get("reason"),
        Some(&DynamicValue::Str("network_graph_too_heavy".into()))
    );
    assert_eq!(fact.tail.value, "k10r_core");
    assert_eq!(fact.tail.type_label, "system");
    assert!(fact.tail.props.is_empty());

    assert_eq!(FactTuple::from_json("[]"), Err(IngestError::Arity(0)));
    assert!(matches!(
        FactTuple::from_json(r#"["a","b",{},1,{},"c","d",{}]"#),
        Err(IngestError::Slot { slot: 3, .. })
    ));
    assert!(matches!(
        FactTuple::from_json(r#"["a","b","c","R",{},"d","e",{}]"#),
        Err(IngestError::Slot { slot: 2, .. })
    ));
    assert!(matches!(
        FactTuple::from_json("not json"),
        Err(IngestError::Json(_))
    ));
}

/// The entity key is `[scope][node_id]` — fixed width, BE, no name in it
/// (the name is an attribute; the exact entrance is the `by_name` face).
#[test]
fn entity_key_layout_is_hex_locked() {
    let buf = EntityKey {
        scope: 7,
        node_id: 9,
    }
    .encode();
    assert_eq!(buf.len(), 16, "scope 8 + node_id 8");
    assert_eq!(&buf[0..8], &7u64.to_be_bytes(), "scope");
    assert_eq!(&buf[8..16], &9u64.to_be_bytes(), "node_id");
}

/// The fact edge's payload is an ordinary obj payload: `[version u8]
/// [hot_len u16 BE][since u64][confidence u8][weight u32][read u32]
/// [write u32][tool u32][last_read u64][last_write u64]` — the derive's own
/// field order, contiguous big-endian. The round trip is what keeps the
/// reader honest against the writer (both are the declaration's).
#[test]
fn edge_payload_layout_is_locked() {
    let edge = FactEdge {
        since: 7,
        confidence: 200,
        weight: 3,
        read_count: 5,
        write_count: 2,
        tool_count: 9,
        last_read: 1_000,
        last_write: 900,
    };
    let payload = edge.encode_payload();
    assert_eq!(
        payload.len(),
        3 + 8 + 1 + 4 + 4 + 4 + 4 + 8 + 8,
        "header + the fixed fields"
    );
    assert_eq!(payload[0], 2, "layout version");
    assert_eq!(&payload[1..3], &41u16.to_be_bytes(), "hot_len");
    assert_eq!(&payload[3..11], &7u64.to_be_bytes(), "since");
    assert_eq!(payload[11], 200, "confidence");
    assert_eq!(&payload[12..16], &3u32.to_be_bytes(), "weight");
    assert_eq!(&payload[16..20], &5u32.to_be_bytes(), "read_count");
    assert_eq!(&payload[20..24], &2u32.to_be_bytes(), "write_count");
    assert_eq!(&payload[24..28], &9u32.to_be_bytes(), "tool_count");
    assert_eq!(&payload[28..36], &1_000u64.to_be_bytes(), "last_read");
    assert_eq!(&payload[36..44], &900u64.to_be_bytes(), "last_write");
    assert_eq!(FactEdge::decode_payload(&payload), edge);
}

/// One name = one node inside one scope; the same name in another scope is
/// a different corpus (the ownership boundary flat memory shares).
#[test]
fn entities_resolve_by_name_within_a_scope() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    let fact = tuple(PIVOT);

    graph.ingest(1, &fact).unwrap();
    let (head_id, head) = graph.entity_by_name(1, "galaxy_repo").unwrap();
    assert_eq!(head.name, "galaxy_repo");
    assert_eq!(head_id, 1, "the first node of a fresh table");
    let (tail_id, _) = graph.entity_by_name(1, "k10r_core").unwrap();

    // The kind token came back out of the collection's own dictionary.
    assert_eq!(graph.entity_kind(head.kind_id).unwrap(), "repository");

    // Re-ingesting the same fact resolves the SAME nodes (no duplicate
    // entity for a name already in the scope).
    graph.ingest(1, &fact).unwrap();
    assert_eq!(graph.entity_by_name(1, "galaxy_repo").unwrap().0, head_id);

    // Another scope: a separate node, not a shared one.
    assert!(graph.entity_by_name(2, "galaxy_repo").is_none());
    graph.ingest(2, &fact).unwrap();
    assert_ne!(graph.entity_by_name(2, "galaxy_repo").unwrap().0, head_id);
    assert_ne!(graph.entity_by_name(2, "k10r_core").unwrap().0, tail_id);
}

/// Head/tail props are the ENTITY's attributes (ADR-0004's slot rule): they
/// land on the node row and MERGE across facts, because the node is shared
/// by every fact about it.
#[test]
fn entity_props_land_on_the_node_and_merge() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    graph
        .ingest(
            1,
            &tuple(
                r#"["galaxy_repo","repository",{"vcs":"gitea"},"PIVOTED_TO",{},"k10r_core","system",{}]"#,
            ),
        )
        .unwrap();
    let (head_id, _) = graph.entity_by_name(1, "galaxy_repo").unwrap();

    graph
        .ingest(
            1,
            &tuple(
                r#"["galaxy_repo","repository",{"stage":"prototype"},"USES",{},"fjall_engine","system",{}]"#,
            ),
        )
        .unwrap();

    let props = graph.entity_props(1, head_id).expect("the node has props");
    assert_eq!(
        props.get("vcs"),
        Some(&DynamicValue::Str("gitea".into())),
        "the earlier fact's attribute survives the later one"
    );
    assert_eq!(
        props.get("stage"),
        Some(&DynamicValue::Str("prototype".into()))
    );
}

/// A relation that does NOT declare `functional` is multi-valued: two facts
/// with the same (source, kind) are two edges (parallel edges are separate
/// facts, okm ADR-0017).
#[test]
fn multi_valued_relation_links_parallel_edges() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    let first = tuple(r#"["user","person",{},"PREFERS",{},"iced_americano","coffee",{}]"#);
    let second = tuple(r#"["user","person",{},"PREFERS",{},"cappuccino","coffee",{}]"#);
    assert_eq!(
        graph.ingest(1, &first).unwrap(),
        IngestOutcome::Linked { edge_id: 1 }
    );
    assert_eq!(
        graph.ingest(1, &second).unwrap(),
        IngestOutcome::Linked { edge_id: 2 }
    );

    let (user_id, _) = graph.entity_by_name(1, "user").unwrap();
    assert_eq!(graph.typed_out(1, user_id, "PREFERS").len(), 2);
    assert_eq!(graph.out_edges(1, user_id).len(), 2);
}

/// A relation declaring `functional` at one source has ONE incumbent: a
/// newer fact rewrites that edge in place (same edge_id, new endpoints and
/// attributes) instead of accumulating a second.
#[test]
fn functional_relation_rewrites_the_incumbent() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    let first = tuple(
        r#"["k10r_core","system",{},"STORAGE_ENGINE",{"functional":true,"since":100,"reason":"stale"},"fjall_engine","system",{}]"#,
    );
    let second = tuple(
        r#"["k10r_core","system",{},"STORAGE_ENGINE",{"functional":true,"since":200},"slatedb_engine","system",{}]"#,
    );
    let IngestOutcome::Linked { edge_id } = graph.ingest(1, &first).unwrap() else {
        panic!("the first fact links");
    };
    assert_eq!(
        graph.ingest(1, &second).unwrap(),
        IngestOutcome::Rewritten { edge_id },
        "the winner keeps the incumbent's edge id"
    );

    let (core_id, _) = graph.entity_by_name(1, "k10r_core").unwrap();
    assert_eq!(graph.typed_out(1, core_id, "STORAGE_ENGINE").len(), 1);

    let fact = graph.fact(edge_id).unwrap();
    assert_eq!(fact.kind, "STORAGE_ENGINE");
    assert_eq!(fact.edge.since, 200);
    let (slatedb_id, _) = graph.entity_by_name(1, "slatedb_engine").unwrap();
    let expected = NodeRef::new(
        <EntityRow as Document>::NS_PREFIX,
        &EntityKey {
            scope: 1,
            node_id: slatedb_id,
        }
        .encode(),
    );
    assert_eq!(fact.dst, expected, "the endpoint moved to the new object");
    assert_eq!(fact.src.ns_id(), mem_core::ENTITY_NS);

    // The old object keeps no stale in-edge: the traversal faces follow the
    // rewrite.
    let (fjall_id, _) = graph.entity_by_name(1, "fjall_engine").unwrap();
    assert!(graph.typed_out(1, fjall_id, "STORAGE_ENGINE").is_empty());

    // The rewrite is ONE batch (unlink_into + link_into), and the dynamic
    // segment is k10r's entry: the old assertion's props must not survive
    // into the new one.
    let props = graph.fact_props(edge_id).expect("the assertion's own record");
    assert!(
        !props.contains_key("reason"),
        "the old assertion's prop does not survive the rewrite"
    );
    assert_eq!(
        props.get("functional"),
        Some(&DynamicValue::Bool(true)),
        "the new assertion's own declaration is what the entry holds"
    );
    assert_eq!(graph.fact(edge_id).unwrap().edge.since, 200);
    assert_eq!(graph.out_edges(1, core_id), vec![edge_id], "one edge, one commit");
}

/// The loser of the adjudication is DROPPED: an older, less confident fact
/// never overwrites the incumbent (a wrong overwrite is irreversible).
#[test]
fn functional_relation_drops_an_older_weaker_fact() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    let strong = tuple(
        r#"["k10r_core","system",{},"STORAGE_ENGINE",{"functional":true,"since":200,"confidence":255},"fjall_engine","system",{}]"#,
    );
    let weak = tuple(
        r#"["k10r_core","system",{},"STORAGE_ENGINE",{"functional":true,"since":100,"confidence":10},"slatedb_engine","system",{}]"#,
    );
    let IngestOutcome::Linked { edge_id } = graph.ingest(1, &strong).unwrap() else {
        panic!("the first fact links");
    };
    assert_eq!(
        graph.ingest(1, &weak).unwrap(),
        IngestOutcome::Dropped { edge_id }
    );

    let fact = graph.fact(edge_id).unwrap();
    assert_eq!(fact.edge.since, 200, "the incumbent is untouched");
    let (fjall_id, _) = graph.entity_by_name(1, "fjall_engine").unwrap();
    let expected = NodeRef::new(
        <EntityRow as Document>::NS_PREFIX,
        &EntityKey {
            scope: 1,
            node_id: fjall_id,
        }
        .encode(),
    );
    assert_eq!(fact.dst, expected);
}

/// A literal object (a date, a number, a short string) has no node identity,
/// and still becomes a node — kind `literal`, its exact text in `name` (the
/// `by_name` entrance) and its integer projection in the declared
/// `literal_num` (so numeric reads stay ordered key scans).
#[test]
fn literal_object_becomes_a_node() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    graph
        .ingest(
            1,
            &tuple(r#"["user","person",{},"BORN_IN",{},"1990","literal",{}]"#),
        )
        .unwrap();

    let (node_id, row) = graph
        .entity_by_name(1, "1990")
        .expect("the literal is reachable by its text");
    assert_eq!(row.literal_num, 1990);
    assert_eq!(graph.entity_kind(row.kind_id).unwrap(), LITERAL_KIND);

    // A non-numeric literal keeps its text and projects to 0 (a date
    // format's canonical projection is an owed ruling, not a guess).
    graph
        .ingest(
            1,
            &tuple(r#"["user","person",{},"BORN_ON",{},"1990-01-01","literal",{}]"#),
        )
        .unwrap();
    let (_, row) = graph.entity_by_name(1, "1990-01-01").unwrap();
    assert_eq!(row.literal_num, 0);
    assert_ne!(node_id, 0);
}

/// The relation's open props ride the edge's dynamic segment — and the
/// three declared fields are PROJECTIONS of their props, not stored twice
/// (one datum, one home).
#[test]
fn fact_open_props_ride_the_edge_dynamic_segment() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    let IngestOutcome::Linked { edge_id } = graph
        .ingest(
            1,
            &tuple(
                r#"["galaxy_repo","repository",{},"PIVOTED_TO",{"functional":true,"since":5,"reason":"network_graph_too_heavy"},"k10r_core","system",{}]"#,
            ),
        )
        .unwrap()
    else {
        panic!("the first fact links");
    };

    let props = graph.fact_props(edge_id).expect("the edge has props");
    assert_eq!(
        props.get("reason"),
        Some(&DynamicValue::Str("network_graph_too_heavy".into()))
    );
    assert_eq!(
        props.get("functional"),
        Some(&DynamicValue::Bool(true)),
        "the caller's own declaration is kept on the assertion"
    );
    assert!(
        !props.contains_key("since"),
        "a declared field is not also a dynamic one"
    );

    let fact = graph.fact(edge_id).unwrap();
    assert_eq!(fact.edge.since, 5, "the declared field carries the value");
    assert_eq!(fact.edge.confidence, 255, "absent confidence is full");
}

/// The endpoints are self-describing refs: `[ns 2B][len varint][pkey]`, the
/// pkey being the entity key's own bytes — no registry, no shared shape.
#[test]
fn node_refs_are_self_describing() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    let text = r#"["ADG","doc",{},"CITES",{},"RFC_2119","doc",{}]"#;
    let IngestOutcome::Linked { edge_id } = graph.ingest(1, &tuple(text)).unwrap() else {
        panic!("links");
    };

    let fact = graph.fact(edge_id).unwrap();
    let (decoded, taken) = NodeRef::decode(&fact.src.encode()).expect("ref decodes");
    assert_eq!(taken, fact.src.encode().len());
    assert_eq!(decoded, fact.src);
    assert_eq!(decoded.ns_id(), mem_core::ENTITY_NS);

    let (adg_id, _) = graph.entity_by_name(1, "ADG").unwrap();
    assert_eq!(
        EntityKey::decode(&decoded.pkey),
        EntityKey {
            scope: 1,
            node_id: adg_id
        },
        "the ref's pkey IS the entity key"
    );
}

/// Ids survive a reopen: the counters are recovered from the tables (the
/// last key in key order), so a fresh process never hands out an id that
/// collides with a stored row.
#[test]
fn ids_are_recovered_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut graph = GraphStore::open(dir.path()).unwrap();
        graph.ingest(1, &tuple(PIVOT)).unwrap();
        graph
            .ingest(
                1,
                &tuple(r#"["user","person",{},"PREFERS",{},"iced_americano","coffee",{}]"#),
            )
            .unwrap();
    }
    let mut graph = GraphStore::open(dir.path()).unwrap();
    let (existing, _) = graph.entity_by_name(1, "galaxy_repo").unwrap();
    assert_eq!(existing, 1, "the stored node is still the first");

    let IngestOutcome::Linked { edge_id } = graph
        .ingest(
            1,
            &tuple(r#"["user","person",{},"PREFERS",{},"cappuccino","coffee",{}]"#),
        )
        .unwrap()
    else {
        panic!("links");
    };
    assert_eq!(edge_id, 3, "the edge counter continues past the stored ids");
    let (user_id, _) = graph.entity_by_name(1, "user").unwrap();
    assert_eq!(user_id, 3, "the node counter continues too");
}

// ---- retrieval (ADR-0003): vector landing ------------------------------

/// The embedding artifact is ~96 MB and deliberately NOT in the repo, so these
/// tests read the repo config's own model directory and skip when the machine
/// has none — the default gate stays runnable anywhere, and a machine that HAS
/// the model exercises the real inference path (the discipline `embedding.rs`
/// uses).
fn local_embedder() -> Option<std::sync::Arc<mem_core::Embedder>> {
    let cfg_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/krystallizer.kdl");
    let cfg = mem_core::KrystallizerConfig::load(&cfg_path).ok()?;
    let dir = std::path::Path::new(&cfg.embedding.model_dir);
    if !dir.join("model.safetensors").exists() {
        eprintln!("skipping: no embedding model in {}", cfg.embedding.model_dir);
        return None;
    }
    Some(std::sync::Arc::new(
        mem_core::Embedder::load(&cfg.embedding.model_dir).expect("model loads"),
    ))
}

/// Vector landing: an exact-name query lands on that node with score 1.0 (the
/// stored vector IS that text's embedding, and both are unit-length), ahead of
/// every other node. This is the deterministic core of ADR-0003's landing; the
/// semantic ranking quality is PLAN 1d's exit gate, not a unit test.
#[test]
fn retrieval_lands_on_the_exact_node() {
    let Some(engine) = local_embedder() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap().with_embedder(engine);

    graph
        .ingest(
            1,
            &tuple(r#"["fjall_engine","system",{},"STORAGE_ENGINE",{},"k10r_core","system",{}]"#),
        )
        .unwrap();
    graph
        .ingest(
            1,
            &tuple(
                r#"["slatedb_engine","system",{},"STORAGE_ENGINE",{},"k10r_core","system",{}]"#,
            ),
        )
        .unwrap();

    let seeds = graph.retrieve(1, "fjall_engine", 5).unwrap();
    assert!(!seeds.is_empty(), "an exact-name query lands");
    assert_eq!(seeds[0].name, "fjall_engine", "the exact node ranks first");
    assert!(
        (seeds[0].score - 1.0).abs() < 1e-5,
        "same text, same vector: score 1.0, got {}",
        seeds[0].score
    );
    assert_eq!(seeds[0].kind, "system", "the kind token rides the seed");
    // k bounds the answer.
    assert_eq!(graph.retrieve(1, "fjall_engine", 1).unwrap().len(), 1);
}

/// The table is DERIVED state: a write drops it, and the next retrieval sees
/// the new node. A cached table that survived a write would silently answer
/// from the old corpus.
#[test]
fn retrieval_table_follows_writes() {
    let Some(engine) = local_embedder() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap().with_embedder(engine);

    graph
        .ingest(
            1,
            &tuple(r#"["alpha","thing",{},"IS",{},"beta","thing",{}]"#),
        )
        .unwrap();
    let first = graph.retrieve(1, "alpha", 5).unwrap();
    assert_eq!(first[0].name, "alpha");

    // A node created AFTER the table was built must be reachable.
    graph
        .ingest(
            1,
            &tuple(r#"["gamma","thing",{},"IS",{},"beta","thing",{}]"#),
        )
        .unwrap();
    let seeds = graph.retrieve(1, "gamma", 5).unwrap();
    assert_eq!(seeds[0].name, "gamma", "the table was rebuilt, not stale");
}

/// No engine attached: nothing can be embedded, so retrieval answers the legal
/// empty list rather than an error, and the row is legal but vectorless.
#[test]
fn retrieval_without_an_engine_lands_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    graph
        .ingest(
            1,
            &tuple(r#"["alpha","thing",{},"IS",{},"beta","thing",{}]"#),
        )
        .unwrap();
    assert!(graph.retrieve(1, "alpha", 5).unwrap().is_empty());
    let (node_id, _) = graph.entity_by_name(1, "alpha").unwrap();
    assert!(graph.entity(1, node_id).unwrap().embedding.is_empty());
}

/// The read shape (ADR-0010 §5): the query lands, the answer is the landed
/// COMMUNITY's facts rendered as id-bearing text. A query landing on the
/// tail reaches the same answer as one landing on a head — the community is
/// the unit, not a hop count.
#[test]
fn recall_returns_the_landed_community_as_text() {
    let Some(engine) = local_embedder() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap().with_embedder(engine);
    for fact in [
        r#"["fjall_engine","system",{},"IS_A",{},"storage_engine","system",{}]"#,
        r#"["slatedb_engine","system",{},"IS_A",{},"storage_engine","system",{}]"#,
    ] {
        graph.ingest(1, &tuple(fact)).unwrap();
    }

    for query in ["fjall_engine", "storage_engine"] {
        let recall = graph.recall(1, query, 1, None, 0).unwrap();
        assert_eq!(recall.seeds.len(), 1, "one seed for `{query}`");
        assert_eq!(recall.communities.len(), 1, "both names share one community");
        assert_eq!(recall.facts.len(), 2, "the community's two facts");
        assert_eq!((recall.shown, recall.total), (2, 2));

        // Every line that names something carries the id it points at: the
        // ids ARE the answer's handles.
        let text = &recall.text;
        assert!(text.contains("community c"), "{text}");
        for fact in &recall.facts {
            assert!(
                text.contains(&format!("e{} ", fact.edge_id)),
                "the edge line carries its id: {text}"
            );
            assert!(
                text.contains(&format!("n{} {}", fact.src.node_id, fact.src.name)),
                "the node line carries its id: {text}"
            );
        }
    }

    // An empty query is the legal "nothing landed": no seeds, no facts, no
    // text — never an error.
    let nothing = graph.recall(1, "   ", 5, None, 0).unwrap();
    assert!(nothing.seeds.is_empty() && nothing.facts.is_empty());
    assert!(nothing.text.is_empty());
    assert_eq!((nothing.shown, nothing.total), (0, 0));
}

// ---- the weight system (ADR-0005) -----------------------------------

/// One assertion is one write: the edge lands with `write_count = 1`, the
/// caller's `since` as its clock, and the formula folded to the index's
/// bucket — `ln(2) × 0.5 = 0.3466` → 347 milli-units. No clock (absent
/// `since`) invents no decay.
#[test]
fn a_landed_fact_carries_one_write_and_the_folded_weight() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    graph
        .ingest(
            1,
            &tuple(
                r#"["alpha","thing",{},"IS",{"since":1000000},"beta","thing",{}]"#,
            ),
        )
        .unwrap();

    let edge = graph.fact(1).unwrap().edge;
    assert_eq!(edge.write_count, 1);
    assert_eq!(edge.read_count, 0);
    assert_eq!(edge.last_write, 1_000_000);
    assert_eq!(edge.weight, 347, "ln(2) × 0.5 × 1000, rounded");
    // The score is the formula before decay; the bucket is it times 1000.
    assert!((edge.score() - 0.346_573_6).abs() < 1e-6);
}

/// A served recall is one read per fact, folded at the caller's clock: the
/// weight rises by `ln(2)` and the served set is ordered by it — the fact
/// a tool has consulted outranks one merely served.
#[test]
fn recall_counts_reads_and_orders_the_served_facts_by_weight() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    graph
        .ingest(
            1,
            &tuple(
                r#"["alpha","thing",{},"IS",{"since":1000},"beta","thing",{}]"#,
            ),
        )
        .unwrap();
    graph
        .ingest(
            1,
            &tuple(
                r#"["gamma","thing",{},"IS",{"since":1000},"beta","thing",{}]"#,
            ),
        )
        .unwrap();
    // No engine attached: land explicitly on the tail by its node id.
    let (beta, beta_row) = graph.entity_by_name(1, "beta").unwrap();
    let alpha = graph.entity_by_name(1, "alpha").unwrap().0;
    let gamma = graph.entity_by_name(1, "gamma").unwrap().0;
    let edge_a = graph.out_facts(1, alpha)[0].edge_id;
    let edge_b = graph.out_facts(1, gamma)[0].edge_id;

    // A boosted fact is reached and it is the one ranked first.
    graph.touch(1, edge_b, mem_core::Touch::Tool, 2000).unwrap();
    let seed = mem_core::Seed {
        node_id: beta,
        score: 1.0,
        name: beta_row.name,
        kind: "thing".into(),
    };
    let facts = graph.expand(1, &[seed], Some(3000)).unwrap();
    assert_eq!(facts.len(), 2);
    assert_eq!(
        facts[0].edge_id, edge_b,
        "the tool-consulted fact outranks the merely-served one"
    );
    // Both were served, so both counted one read.
    for fact in &facts {
        assert_eq!(fact.edge.read_count, 1);
        assert_eq!(fact.edge.last_read, 3000);
    }
    // The boosted edge: tool ln(2)×1.5 + read ln(2) + write ln(2)×0.5.
    let boosted = facts[0].edge.weight;
    let read_once = facts[1].edge.weight;
    assert!(
        boosted > read_once,
        "tool weight {boosted} vs plain read {read_once}"
    );
    assert_eq!(read_once, 1040, "ln(2) + ln(2)×0.5");
    assert_eq!(boosted, 2079, "plus ln(2)×1.5 for the tool consult");
    assert_eq!(edge_a, edge_b - 1, "the two facts are separate edges");
}

/// The deep-water quadrant (read many, write few) earns the rarity bonus:
/// past the read floor with the write count under the ceiling, the weight
/// jumps by a whole unit — expert knowledge is not punished for being
/// written once.
#[test]
fn the_deep_water_quadrant_earns_the_rarity_bonus() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    graph
        .ingest(
            1,
            &tuple(r#"["alpha","thing",{},"IS",{"since":1000},"beta","thing",{}]"#),
        )
        .unwrap();

    let mut edge = FactEdge::default();
    for _ in 0..4 {
        edge = graph.touch(1, 1, mem_core::Touch::Read, 2000).unwrap().unwrap().edge;
    }
    assert_eq!(edge.read_count, 4);
    assert_eq!(edge.write_count, 1);
    // ln(5) + ln(2)×0.5 + 1.0 (bonus) = 1.6094 + 0.3466 + 1
    assert_eq!(edge.weight, 2956);

    // A second assertion keeps the bonus (write 2 is still under the
    // ceiling); the third forfeits it.
    for since in [5000u64, 6000] {
        graph
            .ingest(
                1,
                &tuple(&format!(
                    r#"["alpha","thing",{{}},"IS",{{"since":{since},"functional":true}},"gamma","thing",{{}}]"#
                )),
            )
            .unwrap();
    }
    let rewritten = graph
        .touch(1, 1, mem_core::Touch::Read, 7000)
        .unwrap()
        .unwrap()
        .edge;
    assert_eq!(rewritten.write_count, 3);
    assert!(rewritten.score() < edge.score(), "the bonus is gone");
}

/// Decay is charged AT THE FOLD, from the gap since the last recorded
/// access: two idle days cost 0.02, and the fold re-bases the clock. An
/// edge whose caller never stamped a time decays by nothing — an absent
/// timestamp is not an old one.
#[test]
fn time_decay_is_charged_at_the_fold_it_elapsed_for() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    graph
        .ingest(
            1,
            &tuple(
                r#"["alpha","thing",{},"IS",{"since":1000000},"beta","thing",{}]"#,
            ),
        )
        .unwrap();

    let two_days = 2 * mem_core::graph::DAY_MS;
    let edge = graph
        .touch(1, 1, mem_core::Touch::Read, 1_000_000 + two_days)
        .unwrap()
        .unwrap()
        .edge;
    assert_eq!(edge.last_read, 1_000_000 + two_days);
    // ln(2) + ln(2)×0.5 − 2 × 0.01 = 1.0197 → 1020.
    assert_eq!(edge.weight, 1020);
}

/// A touch only counts for a real edge inside the named scope — edges carry
/// no scope segment, so the check decodes the source ref (the door turns a
/// `None` into 404).
#[test]
fn a_touch_answers_none_for_an_unknown_edge_or_another_scope() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    graph
        .ingest(
            1,
            &tuple(r#"["alpha","thing",{},"IS",{"since":1000},"beta","thing",{}]"#),
        )
        .unwrap();
    assert!(graph
        .touch(1, 99, mem_core::Touch::Tool, 2000)
        .unwrap()
        .is_none());
    assert!(graph
        .touch(2, 1, mem_core::Touch::Tool, 2000)
        .unwrap()
        .is_none());
    assert!(graph
        .touch(1, 1, mem_core::Touch::Tool, 2000)
        .unwrap()
        .is_some());
}

// ---- ADR-0010 steps 3–4: binning, the read budget, the id issuer ----

/// A landed fact bins BOTH endpoints into one community: the node that
/// moved leaves its seed community behind, and membership is read through
/// the `by_community` face.
#[test]
fn a_landed_fact_bins_both_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    graph
        .ingest(
            1,
            &tuple(r#"["alpha","thing",{},"IS",{},"beta","thing",{}]"#),
        )
        .unwrap();

    let (alpha, _) = graph.entity_by_name(1, "alpha").unwrap();
    let (beta, _) = graph.entity_by_name(1, "beta").unwrap();
    let community = graph.entity(1, alpha).unwrap().community_id;
    assert_eq!(community, graph.entity(1, beta).unwrap().community_id);
    assert!(community != 0, "every node is seeded with a real community");

    let members = graph.community_members(1, community);
    let ids: Vec<u64> = members.iter().map(|(node, _)| *node).collect();
    assert_eq!(members.len(), 2);
    assert!(ids.contains(&alpha) && ids.contains(&beta));

    // The abandoned seed (the lower id, absorbed by the higher one) has no
    // member left under it.
    let (orphan, _) = graph.entity_by_name(1, "alpha").unwrap();
    let _ = orphan;
    assert!(
        graph.community_members(1, 1).is_empty(),
        "the stale by_community entry is gone"
    );
}

/// The answer's size is a BUDGET on fact lines (ADR-0010 §5: a few hundred
/// tokens), and the header states what the budget elided — never a half
/// line, never a silent truncation.
#[test]
fn the_answer_is_budgeted_and_states_the_elision() {
    let Some(engine) = local_embedder() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap().with_embedder(engine);
    // A triangle: three facts, one community.
    for fact in [
        r#"["alpha","thing",{},"IS",{},"beta","thing",{}]"#,
        r#"["beta","thing",{},"IS",{},"gamma","thing",{}]"#,
        r#"["gamma","thing",{},"IS",{},"alpha","thing",{}]"#,
    ] {
        graph.ingest(1, &tuple(fact)).unwrap();
    }

    let capped = graph.recall(1, "alpha", 1, None, 2).unwrap();
    assert_eq!(capped.shown, 2, "the budget caps the shown facts");
    assert_eq!(capped.total, 3, "the community still holds all three");
    assert_eq!(capped.facts.len(), 2);
    assert!(
        capped.text.contains("showing 2"),
        "the header states the elision: {}",
        capped.text
    );

    // The elided lines are the LOWEST-weight ones, and lifting the budget
    // shows the rest: the same corpus answers the same way.
    let full = graph.recall(1, "alpha", 1, None, 0).unwrap();
    assert_eq!(full.shown, 3);
    assert_eq!(full.total, 3);
    assert!(!full.text.contains("showing"));
    for fact in &capped.facts {
        assert!(
            full.facts.iter().any(|f| f.edge_id == fact.edge_id),
            "the capped answer is a prefix of the full one"
        );
    }
}

/// Community ids come from a sequence row, so they only ever grow: a reopen
/// never hands out an id that was already issued (the previous issuer read
/// the maximum off live rows, which a departed community would lower).
#[test]
fn community_ids_are_never_reused_across_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let first = {
        let mut graph = GraphStore::open(dir.path()).unwrap();
        graph
            .ingest(
                1,
                &tuple(r#"["alpha","thing",{},"IS",{},"beta","thing",{}]"#),
            )
            .unwrap();
        let (alpha, _) = graph.entity_by_name(1, "alpha").unwrap();
        graph.entity(1, alpha).unwrap().community_id
    };

    let mut graph = GraphStore::open(dir.path()).unwrap();
    graph
        .ingest(
            1,
            &tuple(r#"["gamma","thing",{},"IS",{},"delta","thing",{}]"#),
        )
        .unwrap();
    let (gamma, _) = graph.entity_by_name(1, "gamma").unwrap();
    let later = graph.entity(1, gamma).unwrap().community_id;
    assert!(
        later > first,
        "the sequence survives the reopen: {later} > {first}"
    );
}

/// Communities are per scope: the same names in another scope form their own
/// community, and neither scope's members leak into the other.
#[test]
fn communities_are_per_scope() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    let fact = tuple(r#"["alpha","thing",{},"IS",{},"beta","thing",{}]"#);
    graph.ingest(1, &fact).unwrap();
    graph.ingest(2, &fact).unwrap();

    let (alpha_one, _) = graph.entity_by_name(1, "alpha").unwrap();
    let (alpha_two, _) = graph.entity_by_name(2, "alpha").unwrap();
    let one = graph.entity(1, alpha_one).unwrap().community_id;
    let two = graph.entity(2, alpha_two).unwrap().community_id;

    let members_one: Vec<u64> = graph
        .community_members(1, one)
        .into_iter()
        .map(|(node, _)| node)
        .collect();
    let members_two: Vec<u64> = graph
        .community_members(2, two)
        .into_iter()
        .map(|(node, _)| node)
        .collect();
    assert_eq!(members_one.len(), 2);
    assert_eq!(members_two.len(), 2);
    assert!(members_one.contains(&alpha_one) && !members_one.contains(&alpha_two));
    assert!(members_two.contains(&alpha_two) && !members_two.contains(&alpha_one));
}
