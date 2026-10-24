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
use crate::graph::{FactView, GraphStore, IngestError};
use std::collections::BTreeSet;

/// The corpus the head reads.
///
/// Defaults to 1 because a fresh scope starts at one and the door rarely knows
/// better; the NODE inside it is no longer a default — it is resolved from the
/// channel's members (ADR-0009 §1).
pub const DEFAULT_SCOPE: u64 = 1;

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
) -> Result<Projection, IngestError> {
    let view = channels
        .fetch_full(channel_id, member_id)
        .map_err(IngestError::Storage)?;
    // The head is the FROZEN set the checkpoint recorded (ADR-0008 §2): the
    // initialization memory is snapshotted as a set of edge ids at compression
    // time and re-rendered per request, so `?format=` still picks the
    // representation while the head's bytes stay stable between compressions.
    // Before the first compression there is no snapshot, so the live traversal
    // answers — over EVERY member of the channel (ADR-0009 §1).
    let head = match &view.head {
        Some(ids) => {
            let mut head = Vec::with_capacity(ids.len());
            for id in ids {
                // An id whose edge is gone is absence (the frozen set outlived
                // its row); a store that cannot answer is not, and it
                // propagates.
                if let Some(fact) = graph.fact_view(*id)? {
                    head.push(fact);
                }
            }
            head
        }
        None => member_memory(channels, graph, scope, channel_id)?,
    };
    Ok(Projection { head, view })
}

/// One turn as the LLM sees it (ADR-0009 §2): consecutive messages from the same
/// side collapse into ONE, and every line carries `[name(id=N)]` so the model can
/// name a speaker and look the member up. Tool rows are skipped for now.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub role: &'static str,
    /// Merged text, one `[name(id=N)] …` line per underlying record.
    pub text: String,
    pub start_ts: u64,
    /// When the turn ENDED. A reply keeps the clock it finished on, so a line a
    /// user typed mid-generation is visible as having arrived inside the span.
    pub end_ts: u64,
    /// The member ids this turn speaks for, in line order.
    pub senders: Vec<u64>,
}

/// The projection as conversation turns — the LLM's side of the channel.
///
/// Two rules, both from ADR-0009 §2: messages between two replies MERGE into one
/// user turn (a burst typed before the model answered is one thing said), and the
/// order is the log's own key order — the reply is keyed by its FIRST token's
/// clock, so a message typed during a generation sorts before that reply while
/// the reply's `end_ts` still says when it finished.
pub fn conversation(
    channels: &mut ChannelStore,
    channel_id: u64,
    member_id: u64,
) -> Result<Vec<Turn>, IngestError> {
    let view = channels
        .fetch_full(channel_id, member_id)
        .map_err(IngestError::Storage)?;
    turns_of(channels, channel_id, &view.increment)
}

/// The same turn building over rows a caller already holds — the door has the
/// increment from its own projection and must not scan twice.
pub fn turns_of(
    channels: &ChannelStore,
    channel_id: u64,
    rows: &[(crate::channel::MessageKey, crate::channel::MessageRow)],
) -> Result<Vec<Turn>, IngestError> {
    let mut turns: Vec<Turn> = Vec::new();
    for (key, row) in rows {
        if row.kind != crate::channel::kind::TEXT {
            continue; // tool calls / results are not in the conversation yet
        }
        let Some(speaker) = role_name(row.role) else {
            continue;
        };
        // An unregistered sender is absence, not a failure: the line falls
        // back to the id-derived name.
        let name = match channels
            .member(channel_id, row.sender)
            .map_err(IngestError::Storage)?
        {
            Some(member) => member.display_name,
            None => format!("member-{}", row.sender),
        };
        let line = format!("[{name}(id={})] {}", row.sender, row.content);
        let ended = row.end_ts_ms.max(key.ts_ms);
        match turns.last_mut() {
            // A run of user messages between two replies is ONE thing said.
            Some(turn) if turn.role == "user" && speaker == "user" => {
                turn.text.push('\n');
                turn.text.push_str(&line);
                turn.end_ts = turn.end_ts.max(ended);
                turn.senders.push(row.sender);
            }
            _ => turns.push(Turn {
                role: speaker,
                text: line,
                start_ts: key.ts_ms,
                end_ts: ended,
                senders: vec![row.sender],
            }),
        }
    }
    Ok(turns)
}

/// The facts incident to EVERY member of the channel, both directions, each edge
/// once — the head before the first compression.
///
/// ADR-0009 §1: a member's facts live on the node `member-{channel}-{id}`, and
/// the head is what that node touches, for all of them (the agent included: its
/// facts are as much initialization memory as the user's). Member order is the
/// store's (by id); the sort is weight-descending, ties by edge id, so repeated
/// reads of one corpus answer identically.
pub fn member_memory(
    channels: &mut ChannelStore,
    graph: &mut GraphStore,
    scope: u64,
    channel_id: u64,
) -> Result<Vec<FactView>, IngestError> {
    let members = channels
        .members(channel_id)
        .map_err(IngestError::Storage)?;
    let mut seen = BTreeSet::new();
    let mut head: Vec<FactView> = Vec::new();
    for (key, _) in members {
        let name = crate::channel::member_node(channel_id, key.member_id);
        let Some((node, _)) = graph.entity_by_name(scope, &name)? else {
            continue; // no facts hang on this member yet
        };
        let edges = graph.out_edges(scope, node)?;
        let incoming = graph.in_edges(scope, node)?;
        for edge_id in edges.into_iter().chain(incoming) {
            if !seen.insert(edge_id) {
                continue;
            }
            // An id whose edge is gone is absence, not a failure.
            if let Some(fact) = graph.fact_view(edge_id)? {
                head.push(fact);
            }
        }
    }
    head.sort_by(|a, b| {
        b.edge
            .weight
            .cmp(&a.edge.weight)
            .then_with(|| a.edge_id.cmp(&b.edge_id))
    });
    Ok(head)
}
