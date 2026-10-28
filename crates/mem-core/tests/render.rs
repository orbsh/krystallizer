//! The read side's REPRESENTATION (ADR-0010 §5): one answer, one
//! representation.
//!
//! The three text forms carry the same content — which facts, which nodes,
//! which counters — and differ only in syntax; `struct` carries no text at
//! all, because there the answer IS the data. These tests lock that claim on
//! constructed slices, so they need no embedding engine and no storage.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_core::graph::{CommunitySlice, FactView, Facts, ResolvedNode};
use mem_core::{render_head, render_memory, RenderFormat};

fn node(node_id: u64, name: &str, kind: &str) -> ResolvedNode {
    ResolvedNode {
        id: mem_core::graph::NodeId::Entity { scope: 1, node_id },
        kind: kind.to_string(),
        literal_num: 0,
        name: name.to_string(),
    }
}

fn fact(edge_id: u64, kind: &str, src: ResolvedNode, dst: ResolvedNode) -> FactView {
    FactView {
        edge_id,
        kind: kind.to_string(),
        src,
        dst,
        edge: Facts {
            since: 1_700_000_000_000,
            confidence: 230,
            weight: 1_400,
            ..Facts::default()
        },
        refs: Vec::new(),
    }
}

/// The same fact, but carrying a reference link (PLAN 4c): the plaintext line
/// prints the target as an ADDRESS — `#41` — so a reader can see that there is
/// more without the answer inlining it. A fact with no refs prints no tail at
/// all: absence must not look like an empty pointer.
#[test]
fn a_reference_link_prints_as_an_address_and_nothing_else() {
    let plain = fact(
        34,
        "PREFERS",
        node(12, "Alice", "person"),
        node(15, "terse", "style"),
    );
    assert!(
        !mem_core::render::render_head(std::slice::from_ref(&plain), RenderFormat::Plaintext)
            .contains("refs="),
        "no refs, no tail"
    );

    let mut linked = plain.clone();
    linked.refs = vec![41, 57];
    let text = mem_core::render::render_head(&[linked], RenderFormat::Plaintext);
    assert!(
        text.contains("refs=#41,#57"),
        "the pointer is printed as addresses: {text}"
    );
}

/// One community of three members holding three facts, of which the answer
/// shows two (the budget elided one) — so every header states the elision.
fn elided_slice() -> CommunitySlice {
    let alice = node(12, "Alice", "person");
    let terse = node(15, "terse replies", "style");
    let rust = node(18, "Rust", "language");
    CommunitySlice {
        community: 7,
        members: 3,
        total: 3,
        facts: vec![
            fact(34, "PREFERS", alice.clone(), terse),
            fact(35, "WRITES", alice, rust),
        ],
    }
}

fn head() -> Vec<FactView> {
    vec![fact(
        34,
        "PREFERS",
        node(12, "Alice", "person"),
        node(15, "terse replies", "style"),
    )]
}

/// The claim the four formats exist for: the three text forms state the same
/// things, and none of them drops the community or the counters.
#[test]
fn every_text_format_carries_the_same_content() {
    let slice = elided_slice();
    for format in [
        RenderFormat::Plaintext,
        RenderFormat::Markdown,
        RenderFormat::Mermaid,
    ] {
        let text = render_memory(std::slice::from_ref(&slice), format);
        let name = format.name();
        assert!(text.contains("c7"), "{name}: the community id is named");
        assert!(text.contains("Alice"), "{name}: the source node is named");
        assert!(
            text.contains("terse replies"),
            "{name}: the destination node is named"
        );
        assert!(text.contains("PREFERS"), "{name}: the relation is named");
    }

    // The counters ride the LINE forms: a diagram's label carries the
    // relation alone.
    for format in [RenderFormat::Plaintext, RenderFormat::Markdown] {
        let text = render_memory(&[elided_slice()], format);
        let name = format.name();
        assert!(
            text.contains("w=1.400"),
            "{name}: the weight rides the line"
        );
        assert!(
            text.contains("c=0.90"),
            "{name}: the confidence rides the line"
        );
    }
}

/// The plaintext form is the dense prompt form, and its ids are the answer's
/// handles — every node and edge line carries the id it points at.
#[test]
fn plaintext_names_the_ids_the_caller_points_at() {
    let text = render_memory(&[elided_slice()], RenderFormat::Plaintext);
    assert!(
        text.contains("community c7 · 3 nodes / 3 facts · showing 2"),
        "{text}"
    );
    assert!(text.contains("n12 Alice [person]"), "{text}");
    assert!(text.contains("e34 n12 -[PREFERS]-> n15"), "{text}");
}

/// Markdown is the same content with headings and bullets.
#[test]
fn markdown_is_headings_and_bullets() {
    let text = render_memory(&[elided_slice()], RenderFormat::Markdown);
    assert!(
        text.starts_with("## community c7 · 3 nodes / 3 facts · showing 2"),
        "{text}"
    );
    assert!(text.contains("\n- `n12` Alice [person]"), "{text}");
    assert!(text.contains("- `e34` `n12` -[PREFERS]-> `n15`"), "{text}");
}

/// Mermaid is a diagram a renderer consumes: a directed graph, one subgraph
/// per community, the relation as the edge label.
#[test]
fn mermaid_is_a_directed_graph_with_a_subgraph() {
    let text = render_memory(&[elided_slice()], RenderFormat::Mermaid);
    assert!(text.starts_with("graph LR"), "{text}");
    assert!(
        text.contains("subgraph c7[\"community c7 · 3 nodes / 3 facts · showing 2\"]"),
        "{text}"
    );
    assert!(
        text.contains("n12[\"Alice\"] -->|PREFERS| n15[\"terse replies\"]"),
        "{text}"
    );
    assert!(text.trim_end().ends_with("end"), "{text}");
}

/// `struct` produces no text: the answer travels as data and the caller
/// assembles it.
#[test]
fn struct_renders_no_text() {
    assert_eq!(
        render_memory(&[elided_slice()], RenderFormat::Struct),
        "",
        "the memory answer renders nothing under `struct`"
    );
    assert_eq!(
        render_head(&head(), RenderFormat::Struct),
        "",
        "the head renders nothing under `struct`"
    );
    assert!(!RenderFormat::Struct.renders_text());
}

/// The head is injected at the top of the context, so its lines are read and
/// never pointed at: names and counters, no ids.
#[test]
fn head_renders_names_without_ids() {
    let text = render_head(&head(), RenderFormat::Plaintext);
    assert_eq!(
        text,
        "Alice (person) -[PREFERS]-> terse replies · w=1.400 · c=0.90"
    );
    assert!(!text.contains("n12"), "no node id in an injected block");

    let markdown = render_head(&head(), RenderFormat::Markdown);
    assert_eq!(
        markdown,
        "- Alice (person) -[PREFERS]-> terse replies · w=1.400 · c=0.90"
    );

    let mermaid = render_head(&head(), RenderFormat::Mermaid);
    assert_eq!(
        mermaid,
        "graph LR\n  n12[\"Alice\"] -->|PREFERS| n15[\"terse replies\"]"
    );
}

/// The wire value round-trips, and an unknown one is `None` — the door turns
/// that into a 400 rather than quietly serving another representation.
#[test]
fn format_parsing_round_trips_and_rejects_strangers() {
    for format in [
        RenderFormat::Plaintext,
        RenderFormat::Markdown,
        RenderFormat::Mermaid,
        RenderFormat::Struct,
    ] {
        assert_eq!(RenderFormat::parse(format.name()), Some(format));
    }
    assert_eq!(
        RenderFormat::parse("Plaintext"),
        None,
        "the wire is lowercase"
    );
    assert_eq!(RenderFormat::parse("json"), None, "no synonyms");
    assert_eq!(RenderFormat::default(), RenderFormat::Plaintext);
}
