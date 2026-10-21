//! The read side's rendering: ONE answer, ONE representation.
//!
//! A consumer of memory need not know how the graph is assembled — a
//! third-party agent has no idea what a community slice is and should never
//! have to reconstruct one. So k10r renders the memory content itself, and
//! the REPRESENTATION is the caller's choice through [`RenderFormat`]:
//!
//! - `plaintext` — the line form, dense, for a prompt (ADR-0010 §5);
//! - `markdown` — the same content with headings and bullets;
//! - `mermaid` — `graph LR`, one subgraph per community, for a diagram
//!   consumer;
//! - `struct` — no text at all: the answer travels as its own data and the
//!   caller assembles it.
//!
//! The three text forms carry the SAME facts, nodes and community and differ
//! only in syntax — with one deliberate exception: a diagram's edge label
//! carries the relation alone, because the counters (weight, confidence) are
//! read off a LINE, where a picture's label is noise. Ids are the caller's
//! handles in a recall answer (a later precise read or edit points at `n12` /
//! `e34`); the head carries no ids because it is injected, not cited.

use crate::graph::{CommunitySlice, FactView, WEIGHT_SCALE};

/// How one memory answer is represented on the wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RenderFormat {
    /// The line form (the default): compact, for a prompt.
    #[default]
    Plaintext,
    /// Headings and bullets over the same lines.
    Markdown,
    /// A `graph LR` diagram, one subgraph per community.
    Mermaid,
    /// The answer AS DATA — the caller assembles it, nothing is rendered.
    Struct,
}

impl RenderFormat {
    /// Every accepted value, in the order the docs list them.
    pub const NAMES: [&'static str; 4] = ["plaintext", "markdown", "mermaid", "struct"];

    /// Parse a wire value; an unknown one is the caller's bug (400), never
    /// a silent fallback to a different representation.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "plaintext" => Some(Self::Plaintext),
            "markdown" => Some(Self::Markdown),
            "mermaid" => Some(Self::Mermaid),
            "struct" => Some(Self::Struct),
            _ => None,
        }
    }

    /// The wire value (echoed beside the answer).
    pub fn name(self) -> &'static str {
        match self {
            Self::Plaintext => "plaintext",
            Self::Markdown => "markdown",
            Self::Mermaid => "mermaid",
            Self::Struct => "struct",
        }
    }

    /// Whether this format produces text at all.
    pub fn renders_text(self) -> bool {
        !matches!(self, Self::Struct)
    }
}

/// Render one recall answer's community slices.
///
/// One block per community. The header states the slice's size and, when the
/// budget elided anything, how much of it the answer shows; every node line
/// carries its node id and every fact line its edge id, because an id IS the
/// caller's handle. The text is deterministic: same corpus, same budget, same
/// answer.
pub fn render_memory(slices: &[CommunitySlice], format: RenderFormat) -> String {
    match format {
        RenderFormat::Plaintext => render_memory_plaintext(slices),
        RenderFormat::Markdown => render_memory_markdown(slices),
        RenderFormat::Mermaid => render_memory_mermaid(slices),
        // No text under `struct`; the caller asked for the data instead.
        RenderFormat::Struct => String::new(),
    }
}

/// Render the head — initialization memory (ADR-0001/0008): the facts
/// incident to the user node, weight-descending.
///
/// Names only: the head is injected at the top of the context, so its lines
/// are read, not pointed at (unlike a recall answer's `n12` / `e34`).
pub fn render_head(facts: &[FactView], format: RenderFormat) -> String {
    match format {
        RenderFormat::Plaintext => facts
            .iter()
            .map(|f| {
                format!(
                    "{} -[{}]-> {} · {}{}",
                    head_end(&f.src.name, &f.src.kind),
                    f.kind,
                    f.dst.name,
                    counters(f),
                    refs_tail(f)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        RenderFormat::Markdown => facts
            .iter()
            .map(|f| {
                format!(
                    "- {} -[{}]-> {} · {}{}",
                    head_end(&f.src.name, &f.src.kind),
                    f.kind,
                    f.dst.name,
                    counters(f),
                    refs_tail(f)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        RenderFormat::Mermaid => {
            let mut out = String::from("graph LR\n");
            for fact in facts {
                out.push_str(&format!(
                    "  n{}[\"{}\"] -->|{}| n{}[\"{}\"]\n",
                    fact.src.node_id,
                    mermaid_text(&fact.src.name),
                    mermaid_text(&fact.kind),
                    fact.dst.node_id,
                    mermaid_text(&fact.dst.name),
                ));
            }
            out.trim_end().to_string()
        }
        RenderFormat::Struct => String::new(),
    }
}

fn render_memory_plaintext(slices: &[CommunitySlice]) -> String {
    let mut out = String::new();
    for slice in slices {
        out.push_str(&header(slice));
        out.push('\n');
        for (node_id, (name, kind)) in nodes_of(slice) {
            out.push_str(&format!("n{node_id} {name} [{kind}]\n"));
        }
        for fact in &slice.facts {
            out.push_str(&fact_line_plaintext(fact));
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

fn render_memory_markdown(slices: &[CommunitySlice]) -> String {
    let mut out = String::new();
    for slice in slices {
        out.push_str(&format!("## {}\n\n", header(slice)));
        for (node_id, (name, kind)) in nodes_of(slice) {
            out.push_str(&format!("- `n{node_id}` {name} [{kind}]\n"));
        }
        if !slice.facts.is_empty() {
            out.push('\n');
        }
        for fact in &slice.facts {
            out.push_str(&format!(
                "- `e{}` `n{}` -[{}]-> `n{}` · {}\n",
                fact.edge_id,
                fact.src.node_id,
                fact.kind,
                fact.dst.node_id,
                counters(fact),
            ));
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn render_memory_mermaid(slices: &[CommunitySlice]) -> String {
    let mut out = String::from("graph LR\n");
    for slice in slices {
        out.push_str(&format!(
            "  subgraph c{}[\"{}\"]\n",
            slice.community,
            mermaid_text(&header(slice))
        ));
        for fact in &slice.facts {
            out.push_str(&format!(
                "    n{}[\"{}\"] -->|{}| n{}[\"{}\"]\n",
                fact.src.node_id,
                mermaid_text(&fact.src.name),
                mermaid_text(&fact.kind),
                fact.dst.node_id,
                mermaid_text(&fact.dst.name),
            ));
        }
        out.push_str("  end\n");
    }
    out.trim_end().to_string()
}

/// The block header both text forms share.
fn header(slice: &CommunitySlice) -> String {
    let mut out = format!(
        "community c{} · {} nodes / {} facts",
        slice.community, slice.members, slice.total
    );
    if slice.facts.len() < slice.total {
        out.push_str(&format!(" · showing {}", slice.facts.len()));
    }
    out
}

/// `e34 n12 -[PREFERS]-> n15 · w=1.400 · c=0.90 · since=… · refs=#41,#57`
///
/// The `refs` tail is the reference link (PLAN 4c): the ids this fact points at,
/// printed so the reader can SEE there is more and decide for itself. Each id is
/// an address — `GET /graph/{scope}/facts/{id}` — not an inlined subtree.
fn fact_line_plaintext(fact: &FactView) -> String {
    let base = format!(
        "e{} n{} -[{}]-> n{} · {} · since={}",
        fact.edge_id,
        fact.src.node_id,
        fact.kind,
        fact.dst.node_id,
        counters(fact),
        fact.edge.since,
    );
    format!("{base}{}", refs_tail(fact))
}

/// The counters every fact line carries: the folded weight (ADR-0005,
/// milli-units) and the confidence (0..=255 on the row, shown 0..=1).
fn counters(fact: &FactView) -> String {
    format!(
        "w={:.3} · c={:.2}",
        fact.edge.weight as f64 / WEIGHT_SCALE,
        fact.edge.confidence as f64 / 255.0,
    )
}

/// The reference-link tail (PLAN 4c): ` · refs=#41,#57`, or empty when the fact
/// points at nothing. An id here is an ADDRESS the reader may fetch —
/// `GET /graph/{scope}/facts/{id}` — so the head keeps its "names, not ids" rule
/// for identity while still telling the reader where a pointer leads.
fn refs_tail(fact: &FactView) -> String {
    if fact.refs.is_empty() {
        return String::new();
    }
    let refs = fact
        .refs
        .iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(" · refs={refs}")
}

/// `Alice (person)` — the entity with its kind, for a head line.
fn head_end(name: &str, kind: &str) -> String {
    format!("{name} ({kind})")
}

/// Only the nodes the shown facts mention, id-ordered: a node line with no
/// edge is a bare name the caller cannot use.
fn nodes_of(slice: &CommunitySlice) -> std::collections::BTreeMap<u64, (String, String)> {
    let mut nodes = std::collections::BTreeMap::new();
    for fact in &slice.facts {
        nodes
            .entry(fact.src.node_id)
            .or_insert_with(|| (fact.src.name.clone(), fact.src.kind.clone()));
        nodes
            .entry(fact.dst.node_id)
            .or_insert_with(|| (fact.dst.name.clone(), fact.dst.kind.clone()));
    }
    nodes
}

/// A label safe inside a mermaid `["…"]`: quotes and newlines would break
/// the diagram, and nothing else can.
fn mermaid_text(raw: &str) -> String {
    raw.replace('"', "'").replace('\n', " ")
}
