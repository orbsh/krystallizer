//! Phase 2.5's view layer: the projection as an ORDERED structure.
//!
//! ADR-0008 §2 declares the order, ADR-0006 §2 owns its placement (view
//! side, never write side). Head first:
//!
//!   1. the head — initialization memory (ADR-0001): the facts incident
//!      to the user node, weight-descending, injected at the head of the
//!      context;
//!   2. the compression point — the checkpoint's `coverage_ts` and the
//!      summary that covers everything at or before it;
//!   3. the increment — every log row strictly after `coverage_ts`, in key
//!      order (BE bytes == time order, no sort).
//!
//! Storage stays full; the view only orders and names (the role code
//! becomes the LLM role name here — ADR-0008 §2 puts that mapping on this
//! side of the wire). One representation: the caller receives structure
//! and renders it, and every message entry keeps its key so a delta
//! record can be written from it.
//!
//! The view runs NO graph algorithm: the head is one node's incident
//! edges in both directions, deduplicated by edge id — not a traversal
//! with a visited policy. Deeper walks are the caller's composition over
//! the graph's own reads (ADR-0003).

use crate::channel::{role, ChannelStore, SessionView};
use crate::graph::{CommunitySlice, FactView, GraphStore, WEIGHT_SCALE};
use std::collections::{BTreeMap, BTreeSet};

/// The corpus the head reads, and which node inside it is the user.
///
/// Both default to 1 for the same reason [`DEFAULT_MEMBER`] does: the
/// single-member shape carries no member→node mapping yet (ADR-0008 §5).
/// Phase 2.7's members layer replaces them with a resolution; until then
/// the door passes these defaults and a caller that knows better states
/// its own values.
///
/// [`DEFAULT_MEMBER`]: crate::channel::DEFAULT_MEMBER
pub const DEFAULT_SCOPE: u64 = 1;
pub const DEFAULT_USER_NODE: u64 = 1;

/// The assembled projection. The order is the field order: head, then the
/// checkpoint (coverage point + summary), then the increment.
#[derive(Clone, Debug, PartialEq)]
pub struct Projection {
    /// Initialization memory: the facts touching the user node, both
    /// directions, deduplicated by edge id, weight-descending (ties by
    /// edge id, so repeated reads of one corpus answer identically).
    pub head: Vec<FactView>,
    /// The per-member projection the checkpoint + increment come from.
    pub view: SessionView,
}

/// The LLM role name for a stored role code (ADR-0008 §2's mapping), or
/// `None` for a code the store never polices — the caller's bug, surfaced
/// rather than guessed at.
pub fn role_name(code: u8) -> Option<&'static str> {
    match code {
        role::USER => Some("user"),
        role::ASSISTANT => Some("assistant"),
        role::TOOL => Some("tool"),
        role::SYSTEM => Some("system"),
        _ => None,
    }
}

/// Assemble one member's projection for one channel: the head from the
/// fact graph, the checkpoint + increment from the log.
///
/// `scope` / `user_node` are stated rather than inferred — the mapping
/// from a member to a graph node is Phase 2.7's, and inventing one here
/// would put a policy in the storage layer.
pub fn assemble(
    channels: &ChannelStore,
    graph: &mut GraphStore,
    channel_id: u64,
    member_id: u64,
    scope: u64,
    user_node: u64,
) -> Projection {
    Projection {
        head: initialization_memory(graph, scope, user_node),
        view: channels.fetch_full(channel_id, member_id),
    }
}

/// The facts incident to `user_node`, both directions, each edge once.
fn initialization_memory(graph: &mut GraphStore, scope: u64, user_node: u64) -> Vec<FactView> {
    let out = graph.out_edges(scope, user_node);
    let incoming = graph.in_edges(scope, user_node);
    let mut seen = BTreeSet::new();
    let mut head: Vec<FactView> = out
        .into_iter()
        .chain(incoming)
        .filter(|edge_id| seen.insert(*edge_id))
        .filter_map(|edge_id| graph.fact_view(edge_id))
        .collect();
    head.sort_by(|a, b| {
        b.edge
            .weight
            .cmp(&a.edge.weight)
            .then_with(|| a.edge_id.cmp(&b.edge_id))
    });
    head
}

/// Render one answer's community slices as the id-bearing memory text
/// (ADR-0010 §5).
///
/// One block per community. The header states the slice's size and, when the
/// budget elided anything, how much of it the answer shows; every node line
/// carries its node id and every fact line its edge id, because an id IS the
/// caller's handle — it is what a later precise read or edit points at.
/// The text is deterministic: same corpus, same budget, same answer.
pub fn render_memory(slices: &[CommunitySlice]) -> String {
    let mut out = String::new();
    for slice in slices {
        let shown = slice.facts.len();
        out.push_str(&format!(
            "community c{} · {} nodes / {} facts",
            slice.community, slice.members, slice.total
        ));
        if shown < slice.total {
            out.push_str(&format!(" · showing {shown}"));
        }
        out.push('\n');

        // Only the nodes the shown facts mention: a node line with no edge
        // is a bare name the caller cannot use.
        let mut nodes: BTreeMap<u64, (String, String)> = BTreeMap::new();
        for fact in &slice.facts {
            nodes
                .entry(fact.src.node_id)
                .or_insert_with(|| (fact.src.name.clone(), fact.src.kind.clone()));
            nodes
                .entry(fact.dst.node_id)
                .or_insert_with(|| (fact.dst.name.clone(), fact.dst.kind.clone()));
        }
        for (node_id, (name, kind)) in &nodes {
            out.push_str(&format!("n{node_id} {name} [{kind}]\n"));
        }
        for fact in &slice.facts {
            out.push_str(&format!(
                "e{} n{} -[{}]-> n{} · w={:.3} · c={:.2} · since={}\n",
                fact.edge_id,
                fact.src.node_id,
                fact.kind,
                fact.dst.node_id,
                fact.edge.weight as f64 / WEIGHT_SCALE,
                fact.edge.confidence as f64 / 255.0,
                fact.edge.since
            ));
        }
    }
    out.trim_end().to_string()
}