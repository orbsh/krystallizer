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
use crate::graph::{FactView, GraphStore};
use std::collections::BTreeSet;

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
    channels: &mut ChannelStore,
    graph: &mut GraphStore,
    channel_id: u64,
    member_id: u64,
    scope: u64,
    user_node: u64,
) -> Projection {
    let view = channels.fetch_full(channel_id, member_id);
    // The head is the FROZEN set the checkpoint recorded (ADR-0008 §2): the
    // initialization memory is snapshotted as a set of edge ids at compression
    // time and re-rendered per request, so `?format=` still picks the
    // representation while the head's bytes stay stable between compressions.
    // Before the first compression there is no snapshot, so the live traversal
    // answers.
    let head = match &view.head {
        Some(ids) => ids.iter().filter_map(|id| graph.fact_view(*id)).collect(),
        None => initialization_memory(graph, scope, user_node),
    };
    Projection { head, view }
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
