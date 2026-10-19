//! The fact graph (ADR-0010 §1, write side): entities (ns 36) and facts
//! (ns 37), plus the deterministic `ingest(fact)` path that lands one
//! 8-slot fact tuple (ADR-0004).
//!
//! Shape, in okm's terms (ADR-0010 §1 — the retired `edge:{src}:{label}:{dst}`
//! prefix layouts were never reimplemented here):
//!
//! - **Entities** = a node collection (ns 36), ordinary document layout:
//!   `[scope][node_id]` primary, `kind_id` + `literal_num` as declared
//!   fields, `name` as the cold segment, and two declared 0x1 faces —
//!   `by_name` (the exact name/alias entrance) and `by_kind` (the
//!   node-kind face). The kind TOKEN allocates through the collection's own
//!   KIND vocabulary (okm's fixed faces 0x04/0x05), never through the
//!   field-name dictionary (0x02/0x03) its props use — two id spaces, so a
//!   token can never read back as a prop name. A literal's numeric form is a
//!   declared FIELD, so a numeric read stays an ordered key scan rather than
//!   vector distance; the face over it lands with the read that scans it.
//! - **Facts** = an okm edge collection (ns 37, one ns per graph): the
//!   declaration is `FactEdge` (`GraphEncode`, ns 37, member set =
//!   `FactNodes::Entities`), the edge's payload is an ordinary obj payload
//!   carrying `since` / `confidence` / `weight` as declared fields
//!   (ADR-0010 §1's three algorithm-queried fields, one 0x1 face each), and
//!   the endpoints stay self-describing `NodeRef`s — a ref carries its own
//!   pkey width, so endpoints need no declaration. The relation NAME is the
//!   edge's PREDICATE: it allocates in the graph's own kind vocabulary
//!   (0x04/0x05) and is listed by one prefix scan of it.
//! - **The relation's open props** (condition / cause / strength / the
//!   per-assertion `functional` flag) ride the edge's dynamic segment
//!   (okm's fixed face 0x01, one entry per edge), built by
//!   `put_frame_named`; their names allocate in the field-name dictionary
//!   (0x02/0x03), separate from the predicate vocabulary.
//!
//! Write-time resolution (ADR-0010 §1): a fact whose relation declares
//! `functional` (the caller states it per assertion — a kind's cardinality
//! is the caller's knowledge, never a store guess) is adjudicated against
//! the incumbent edge of the same kind out of the same source; the
//! declaration absent means the relation is multi-valued and the fact
//! links a parallel edge (the safe default: a dropped fact is lost, an
//! extra edge is recoverable). A literal object (a date, a number, a short
//! string — no node identity) becomes a node of kind `literal`, its exact
//! text in `name` and its integer projection in `literal_num`.
//!
//! Not here yet: ADR-0010's steps 3–4 (incremental LPA, community
//! binning, capsules — with them the node row's `community_id` and its
//! `by_community` face).
//!
//! The weight system (ADR-0005) is the edge's own metadata layer: the
//! counters ride the edge row (`read_count` / `write_count` /
//! `tool_count` / `last_read` / `last_write`), the folded `weight` is the
//! row's declared field, and its `by_weight` face follows the row — so a
//! weight change is one row rewrite, never a second index write plus a
//! tombstone. `recall` orders what it serves by that weight; the counters
//! it bumps are the caller's clock riding the call (`ts_ms`), because a
//! read without a clock is served but not counted.

use crate::embedding::Embedder;
use okm_core::index::DYNAMIC_SLOT;
use okm_core::obj_dict::{DictCache, FIELD_DICT, KIND_VOCAB};
use okm_core::obj_dynamic::{put_frame_named, DynamicValue};
use okm_core::{
    Collection, Document, DocumentEncode, EdgeHead, FjallStore, Graph, GraphEncode, KeyEncode,
    MemBatch, NodeRef, NodeSet, Vector, VectorElem, VirtualStorage, PRIMARY_SLOT, write_slot,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// Entity node collection (ADR-0010 §1's ns 36 — allocated at landing,
/// next free after the code store's 35).
pub const ENTITY_NS: u16 = 36;
/// The facts' edge collection (ns 37, one ns per graph).
pub const FACT_NS: u16 = 37;
/// The reserved node kind of a literal object — a value with no node
/// identity of its own. The caller marks it in the tuple's type slot;
/// k10r recognizes exactly this one token (an open type vocabulary
/// otherwise, so no domain term is second-guessed).
pub const LITERAL_KIND: &str = "literal";
/// The relation prop whose presence (`true`) declares the kind functional
/// at one source — one value per entity, so a new fact may overwrite the
/// incumbent. Absent/false = multi-valued, parallel edges.
pub const FUNCTIONAL_PROP: &str = "functional";

// ---- the weight system's constants (ADR-0005) -----------------------
//
// The stored `weight` is the formula's value in MILLI-units: the formula
// yields a small float, the index needs a stable ordered integer, and
// 0.001 is finer than the per-day decay step (0.01), so the bucket never
// reorders two facts the formula would order differently.
/// Values per formula unit (`weight = formula × 1000`, rounded).
pub const WEIGHT_SCALE: f64 = 1000.0;
/// `read_score = ln(1 + read_count) × 1`
pub const READ_SCORE_COEFF: f64 = 1.0;
/// `write_score = ln(1 + write_count) × 0.5`
pub const WRITE_SCORE_COEFF: f64 = 0.5;
/// `tool_score = ln(1 + tool_count) × 1.5` (a tool consult outranks a look)
pub const TOOL_SCORE_COEFF: f64 = 1.5;
/// The deep-water bonus: high read, low write is expert knowledge, and a
/// write-side penalty would structurally punish it.
pub const RARITY_BONUS: f64 = 1.0;
/// `read_count > RARITY_READ_FLOOR` …
pub const RARITY_READ_FLOOR: u32 = 3;
/// … and `write_count < RARITY_WRITE_CEIL` define the deep-water quadrant.
pub const RARITY_WRITE_CEIL: u32 = 3;
/// `time_decay = days_since_last_access × 0.01`
pub const DECAY_PER_DAY: f64 = 0.01;
/// One day in the caller's clock unit (milliseconds).
pub const DAY_MS: u64 = 86_400_000;

/// Which counter one access bumps. `Write` is the fact's own assertion
/// (ingest); `Read` is a served recall; `Tool` is a caller-reported tool
/// consult (its own door — k10r cannot see a tool call from inside).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touch {
    Read,
    Write,
    Tool,
}

/// Entity identity: `[scope][node_id]`. `scope` leads because the corpus
/// is per-user (ADR-0003) — the ownership boundary flat memory shares.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(36)]
pub struct EntityKey {
    pub scope: u64,
    pub node_id: u64,
}

/// One entity node. `kind_id` allocates through this collection's own KIND
/// vocabulary (okm's fixed faces 0x04/0x05); the entity's prop names
/// allocate through the field-name dictionary (0x02/0x03). The two never
/// share an id space, so a token cannot read back under a prop's name.
///
/// `community_id` is the write-side binning result (ADR-0010 §2): a node
/// is seeded with a fresh id the moment it lands and re-binned by the
/// incremental LPA pass, which is why the `by_community` face exists —
/// the bucket read is a face scan, never a full-table filter.
///
/// `name` is an ATTRIBUTE (never key material): the human identity reaches
/// the exact entrance through the `by_name` face. `literal_num` is a
/// literal node's integer projection (0 for an ordinary entity) — a
/// declared, fixed-width, ordered field, so an interval or nearest read
/// stays a `scan_range` over a face rather than a vector-distance guess.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(EntityKey)]
#[ok_ns(36)]
#[ok_layout(version = 2)]
#[ok_index(
    // Payload fields only: the key tail appends the primary key, so the
    // entries land as `[kind_id][scope][node_id]`,
    // `[name][scope][node_id]` and `[community_id][scope][node_id]`.
    // Text-first regime: an exact match re-verifies the row's name and the
    // decoded key's scope — a prefix scan for `alpha` also reaches
    // `alphabet`, which is what dictionary order means.
    by_kind { fields(kind_id) },
    by_name { fields(name) },
    by_community { fields(community_id) },
)]
pub struct EntityRow {
    pub kind_id: u16,
    /// A literal node's integer projection (0 for an ordinary entity): a
    /// declared, fixed-width, ordered field, so a numeric read later stays
    /// a range scan rather than a vector-distance guess. No face is
    /// declared over it yet — the face lands with the read that scans it.
    #[ok_default(0)]
    pub literal_num: i64,
    /// The node's community (ADR-0010 §2): a fresh seed when the node
    /// lands, then whatever the local LPA vote assigns. 0 means "not yet
    /// seeded" — a row that predates this field decodes to it and the next
    /// ingest re-bins it.
    #[ok_default(0)]
    pub community_id: u64,
    pub name: String,
    /// The node's vector (ADR-0003): the built-in engine's output over the
    /// entity's name, a cold frame of ANY length, L2-normalized at the
    /// source so the contract's `cosine` is an inner product. EMPTY is a
    /// legal row (no engine attached when it was written) and the retrieval
    /// side skips it — never a zero vector.
    pub embedding: Vector<f32>,
}

use __OkmIndex_EntityRow_by_community as ByCommunity;
use __OkmIndex_EntityRow_by_name as ByName;

/// A fact: an edge of the graph. The three declared fields are ADR-0010
/// §1's algorithm-queried ones — `since` (the caller's clock, the
/// adjudication input), `confidence` (the write-side's, defaulting to full
/// confidence so a fact that states none is not penalized) and `weight`
/// (the weight system's fold, ADR-0005).
///
/// The four counters plus two clocks after `weight` are the weight
/// system's raw inputs (ADR-0005's `weight_meta`): read / write / tool
/// counts and the last access in each direction. They ride the SAME row
/// as the weight — okm's declared faces are derived from the row, so a
/// counter bump rewrites one row and the `by_weight` entry follows it.
/// The delta-append table the ADR sketches is the multi-writer escape; it
/// folds into these same fields, so switching to it changes no schema.
#[derive(Clone, Debug, Default, PartialEq, GraphEncode)]
#[ok_ns(37)]
#[ok_nodes(FactNodes)]
// The fields appended after `weight` raise the payload's hot width, hence
// the layout version (okm's schema-evolution rule: new fields at the tail
// of their segment, version recorded in the header).
#[ok_layout(version = 2)]
// The three declared faces ADR-0010 §1 names: each algorithm-queried field
// gets its own counting-family 0x1 index, declared here (a face is a
// declaration, never implied by the field's existence). `by_weight` is also
// ADR-0005's `W:{weight}:{edge_id}` index — one face, not two.
#[ok_index(
    by_since { fields(since) },
    by_confidence { fields(confidence) },
    by_weight { fields(weight) },
)]
pub struct FactEdge {
    #[ok_default(0)]
    pub since: u64,
    #[ok_default(255)]
    pub confidence: u8,
    #[ok_default(0)]
    pub weight: u32,
    #[ok_default(0)]
    pub read_count: u32,
    #[ok_default(0)]
    pub write_count: u32,
    #[ok_default(0)]
    pub tool_count: u32,
    #[ok_default(0)]
    pub last_read: u64,
    #[ok_default(0)]
    pub last_write: u64,
}

impl FactEdge {
    /// The formula's value BEFORE decay, in weight units:
    /// `ln(1+read) + ln(1+write)×0.5 + ln(1+tool)×1.5 + rarity_bonus`.
    /// Kept separate from the stored bucket so a test (or a future
    /// re-fold) can see the exact quantity.
    pub fn score(&self) -> f64 {
        let read = (1.0 + self.read_count as f64).ln() * READ_SCORE_COEFF;
        let write = (1.0 + self.write_count as f64).ln() * WRITE_SCORE_COEFF;
        let tool = (1.0 + self.tool_count as f64).ln() * TOOL_SCORE_COEFF;
        let rarity = if self.read_count > RARITY_READ_FLOOR && self.write_count < RARITY_WRITE_CEIL
        {
            RARITY_BONUS
        } else {
            0.0
        };
        read + write + tool + rarity
    }

    /// The elapsed decay from the last recorded access to `now`, in weight
    /// units. No recorded access (a caller that stamps no clock) or a
    /// clock that did not move costs nothing — decay is never invented
    /// from an absent timestamp.
    pub fn decay_at(&self, now: u64) -> f64 {
        let last = self.last_read.max(self.last_write);
        if last == 0 || now <= last {
            return 0.0;
        }
        (now - last) as f64 / DAY_MS as f64 * DECAY_PER_DAY
    }

    /// Fold the weight from the current counters and an already-measured
    /// decay. Called at every fold point (a write, a served read, a tool
    /// touch), so the indexed value is the formula's value at the
    /// precision the index needs (ADR-0005: "the indexed value is the same
    /// value the formula produces at fold time").
    fn fold_weight(&mut self, elapsed_decay: f64) {
        let value = (self.score() - elapsed_decay).max(0.0) * WEIGHT_SCALE;
        self.weight = value.round() as u32;
    }

    /// This edge after one access of `kind` at the caller's `now` (ms):
    /// the counter bumps, the clock advances (never backwards), and the
    /// weight is folded with the decay the gap had accrued. The decay is
    /// measured against the PREVIOUS access and charged once here; the
    /// same fold re-bases the clock, so it is not charged twice.
    pub fn touched(&self, kind: Touch, now: u64) -> FactEdge {
        let elapsed_decay = self.decay_at(now);
        let mut next = self.clone();
        match kind {
            Touch::Read | Touch::Tool => {
                next.last_read = next.last_read.max(now);
            }
            Touch::Write => {
                next.last_write = next.last_write.max(now);
            }
        }
        match kind {
            Touch::Read => next.read_count = next.read_count.saturating_add(1),
            Touch::Write => next.write_count = next.write_count.saturating_add(1),
            Touch::Tool => next.tool_count = next.tool_count.saturating_add(1),
        }
        next.fold_weight(elapsed_decay);
        next
    }
}

/// The member set: the graph admits entity nodes (ns 36) and nothing else.
/// A compile-time fact in code — checked by okm at write AND at decode.
pub enum FactNodes {
    Entities,
}

impl NodeSet for FactNodes {
    const NS_VALUES: &'static [u16] = &[ENTITY_NS];
}

// ---- ADR-0010 steps 3–4: binning and capsules ------------------------

/// The capsule collection (ADR-0010 §4): one row per community — a cache
/// of the community's latest semantic summary. Its own ns, next free after
/// the facts' 37.
pub const COMMUNITY_NS: u16 = 38;
/// How many propagation rounds one LPA pass runs (ADR-0010 §2: "3 rounds
/// converge in the common case"; the cap keeps a pass bounded).
pub const LPA_ROUNDS: usize = 3;
/// The neighbourhood radius the local subgraph is loaded at, in hops.
pub const LPA_RADIUS: usize = 2;
/// A community is worth summarizing when its internal density crosses this
/// (ADR-0010 §3's density water line: `2|E| / (|V|(|V|-1))`).
pub const DENSITY_THRESHOLD: f64 = 0.35;
/// …but the density reads only once the community has this many members: a
/// two-node community is trivially a clique (density 1.0 after one fact),
/// and a "summary" of one assertion is not what the water line is for.
/// Below it, the uncompressed watermark is the only reason to summarize.
pub const DENSITY_MIN_VERTICES: usize = 3;
/// …or when this many assertions have landed in it since the last
/// consolidation (ADR-0010 §3's uncompressed-fact watermark).
pub const UNCOMPRESSED_WATERMARK: u32 = 15;
/// Capsule states: `PENDING` = the summary is stale, the caller may pull
/// the packet; `DONE` = the summary is current.
pub const STATE_PENDING: u8 = 0;
pub const STATE_DONE: u8 = 1;

/// Capsule identity: `[scope][community_id]`. Per-scope, like every other
/// row of this corpus: community ids are issued per scope, so two users'
/// corpora never share one.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(38)]
pub struct CommunityKey {
    pub scope: u64,
    pub community_id: u64,
}

/// One community's capsule (ADR-0010 §4): the summary the read side injects
/// without running an algorithm, plus the write side's bookkeeping —
/// `uncompressed` counts assertions that landed since the last summary, and
/// `state` says whether that summary is stale.
///
/// The overwrite IS the design: a capsule is a cache, the latest summary is
/// the only one worth reading, and the history is what the facts already
/// are. One row per community keeps the key count bounded by the number of
/// communities instead of growing per re-summarization.
#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(CommunityKey)]
#[ok_ns(38)]
pub struct CommunityRow {
    /// The caller's clock of the last write to this row (ms).
    #[ok_default(0)]
    pub updated_at: u64,
    /// `STATE_PENDING` / `STATE_DONE`.
    #[ok_default(0)]
    pub state: u8,
    /// Assertions landed since the last consolidation (the watermark's
    /// input); reset by the write-back.
    #[ok_default(0)]
    pub uncompressed: u32,
    pub title: String,
    pub summary: String,
}

/// What one consolidation pull returns for one community (ADR-0010 §4):
/// the packet is the canonical 8-slot tuples of the community's internal
/// facts, one JSON array per line — exactly the form the summarizer prompt
/// states, so nothing restates the tuple.
#[derive(Clone, Debug, PartialEq)]
pub struct CommunityPacket {
    pub community_id: u64,
    pub state: u8,
    pub uncompressed: u32,
    pub member_count: u32,
    pub updated_at: u64,
    pub title: String,
    pub summary: String,
    pub facts: Vec<String>,
}

/// One community as a recall hands it back: the layer-3 context the caller
/// injects at the prompt head without a second round trip (ADR-0010 §5).
#[derive(Clone, Debug, PartialEq)]
pub struct Capsule {
    pub community_id: u64,
    pub state: u8,
    pub updated_at: u64,
    /// Assertions landed since the last summary (the write side's watermark
    /// input); the caller may use it to judge how stale the text is.
    pub uncompressed: u32,
    pub title: String,
    pub summary: String,
}

/// One end of a fact: a value, its type label, and its props (the
/// entity's attributes — they land on the node row, shared by every fact
/// about that entity, ADR-0004/ADR-0010 §1).
#[derive(Clone, Debug, PartialEq)]
pub struct Term {
    pub value: String,
    pub type_label: String,
    pub props: BTreeMap<String, DynamicValue>,
}

/// The relation: its name IS the edge's PREDICATE (the graph's kind
/// vocabulary allocates it), so it carries no type label; its props are the
/// assertion's modifiers.
#[derive(Clone, Debug, PartialEq)]
pub struct Relation {
    pub name: String,
    pub props: BTreeMap<String, DynamicValue>,
}

/// The 8-slot fact tuple, slots 0–7 in order: head value / head type /
/// head props, rel name / rel props, tail value / tail type / tail props
/// (ADR-0004 — the form the wiki's serialization section, the extraction
/// skill and PROMPTS all state).
#[derive(Clone, Debug, PartialEq)]
pub struct FactTuple {
    pub head: Term,
    pub rel: Relation,
    pub tail: Term,
}

impl FactTuple {
    /// Decode the canonical JSON list form. Structural failures are
    /// errors, never silent defaults: a tuple of the wrong arity or a
    /// slot of the wrong JSON shape is a caller bug, and the store
    /// reports it instead of storing a half-fact.
    pub fn from_json(text: &str) -> Result<Self, IngestError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| IngestError::Json(e.to_string()))?;
        Self::from_value(&value)
    }

    /// The same decode for an already-parsed JSON value (the HTTP door
    /// hands its body straight in — the tuple travels as JSON either way).
    pub fn from_value(value: &serde_json::Value) -> Result<Self, IngestError> {
        let slots = value.as_array().ok_or(IngestError::Slot {
            slot: 0,
            why: "the tuple must be a JSON list",
        })?;
        if slots.len() != 8 {
            return Err(IngestError::Arity(slots.len()));
        }
        Ok(Self {
            head: decode_term(&slots[0], &slots[1], &slots[2])?,
            rel: Relation {
                name: text_slot(&slots[3], 3)?,
                props: props_slot(&slots[4], 4)?,
            },
            tail: decode_term(&slots[5], &slots[6], &slots[7])?,
        })
    }
}

fn text_slot(value: &serde_json::Value, slot: usize) -> Result<String, IngestError> {
    value.as_str().map(str::to_owned).ok_or(IngestError::Slot {
        slot,
        why: "expected a string",
    })
}

fn props_slot(
    value: &serde_json::Value,
    slot: usize,
) -> Result<BTreeMap<String, DynamicValue>, IngestError> {
    if !value.is_object() {
        return Err(IngestError::Slot {
            slot,
            why: "expected an object (empty props are `{}`)",
        });
    }
    // The value form carries its own serde (okm `schema-serde`), so the
    // slot decoder is a deserialize — no hand-written recursion per shape.
    serde_json::from_value(value.clone()).map_err(|_| IngestError::Slot {
        slot,
        why: "expected an object of dynamic values",
    })
}

fn decode_term(
    value: &serde_json::Value,
    type_label: &serde_json::Value,
    props: &serde_json::Value,
) -> Result<Term, IngestError> {
    Ok(Term {
        value: text_slot(value, 0)?,
        type_label: text_slot(type_label, 1)?,
        props: props_slot(props, 2)?,
    })
}

/// What one ingest did — ADR-0010 §1's three-way outcome, reported so the
/// caller can tell a landed fact from an adjudicated loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    /// No incumbent: the fact is a new edge.
    Linked { edge_id: u64 },
    /// The relation is functional at this source and this fact won the
    /// adjudication: the incumbent's row was rewritten (same `edge_id`).
    Rewritten { edge_id: u64 },
    /// Functional relation, incumbent kept: nothing was written.
    Dropped { edge_id: u64 },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IngestError {
    #[error("fact tuple is not valid JSON: {0}")]
    Json(String),
    #[error("fact tuple must be a JSON list of 8 slots, got {0}")]
    Arity(usize),
    #[error("slot {slot}: {why}")]
    Slot { slot: usize, why: &'static str },
    #[error("graph storage: {0}")]
    Storage(String),
    #[error("embedding: {0}")]
    Embedding(String),
}

/// A decoded fact, read back from storage.
#[derive(Clone, Debug, PartialEq)]
pub struct FactBody {
    pub edge_id: u64,
    pub src: NodeRef,
    pub dst: NodeRef,
    pub kind: String,
    pub edge: FactEdge,
}

/// One node as a fact read returns it: its identity, its name, its kind
/// token and (for a literal) its numeric projection — the refs carry the
/// scope, so a caller never passes one in.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedNode {
    pub scope: u64,
    pub node_id: u64,
    pub kind: String,
    pub literal_num: i64,
    pub name: String,
}

/// One fact with BOTH endpoints resolved — what a wire face or a
/// renderer needs (an edge id alone is not renderable).
#[derive(Clone, Debug, PartialEq)]
pub struct FactView {
    pub edge_id: u64,
    pub kind: String,
    pub src: ResolvedNode,
    pub dst: ResolvedNode,
    pub edge: FactEdge,
}

/// One landing seed (ADR-0003): the node the query landed on, its similarity
/// (inner product of unit vectors = cosine), and the kind token. Seeds are
/// what the neighbourhood expansion consumes; the caller needs nothing else.
#[derive(Clone, Debug, PartialEq)]
pub struct Seed {
    pub node_id: u64,
    pub score: f32,
    pub name: String,
    pub kind: String,
}

/// What one recall returns (ADR-0003: "vector landing + graph traversal"): the
/// landing seeds, then every fact one hop out of them — both directions,
/// deduplicated by edge id (a fact between two seeds appears once), ordered
/// by the weight system's folded weight, highest first (ADR-0005). The
/// seeds keep their similarity order; the facts carry their counters and
/// clocks, so a caller can see why one outranks another.
#[derive(Clone, Debug, PartialEq)]
pub struct Recall {
    pub seeds: Vec<Seed>,
    /// The capsules of the communities the seeds belong to (ADR-0010 §5
    /// step 2): the layer-3 summary the caller injects at the prompt head,
    /// read as point gets — the read path runs no graph algorithm.
    pub communities: Vec<Capsule>,
    pub facts: Vec<FactView>,
}

/// The landing confidence floor (ADR-0003): below it nothing landed, and an
/// EMPTY result is the legal answer — the caller injects no memory. One place,
/// deliberately not a per-call parameter.
pub const LANDING_FLOOR: f32 = 0.35;

/// Byte budget for the per-scope vector tables. 512-dim f32 is ~2 KB per row,
/// so this holds roughly 32k vectors across every active scope; past it the
/// least-recently-used scope is dropped and rebuilt from the KV on demand.
const TABLE_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// How a scope's vectors are scanned. ONE shape exists today — `Flat`: the
/// table is loaded from the KV once and scanned in memory, which is what
/// ADR-0003 chose (the corpus is per-scope; ANN is re-opened only when a
/// measured corpus crosses the threshold). The bucketed form (IVF: coarse
/// centroids in the key, probe nprobe buckets) is the documented next step
/// and only pays off once one scope's table no longer fits the budget above;
/// it slots in behind this same `retrieve` signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanStrategy {
    Flat,
}

/// One scope's loaded table: `(node_id, name, kind_id, unit vector)` rows and
/// the bytes they occupy.
struct ScopeTable {
    rows: Vec<(u64, String, u16, Vec<f32>)>,
    bytes: usize,
}

/// The per-scope tables, with a hard byte budget and LRU eviction — the table
/// is derived state, so evicting costs one reload, never correctness.
#[derive(Default)]
struct ScopeTables {
    tables: HashMap<u64, ScopeTable>,
    /// Load order, oldest first.
    order: VecDeque<u64>,
    bytes: usize,
}

impl ScopeTables {
    fn contains(&self, scope: u64) -> bool {
        self.tables.contains_key(&scope)
    }

    fn get(&self, scope: u64) -> Option<&ScopeTable> {
        self.tables.get(&scope)
    }

    fn forget(&mut self, scope: u64) {
        if let Some(table) = self.tables.remove(&scope) {
            self.bytes -= table.bytes;
        }
        if let Some(pos) = self.order.iter().position(|s| *s == scope) {
            self.order.remove(pos);
        }
    }

    fn touch(&mut self, scope: u64) {
        if let Some(pos) = self.order.iter().position(|s| *s == scope) {
            self.order.remove(pos);
        }
        self.order.push_back(scope);
    }

    fn insert(&mut self, scope: u64, table: ScopeTable) {
        while self.bytes + table.bytes > TABLE_BUDGET_BYTES {
            let Some(oldest) = self.order.front().copied() else {
                break;
            };
            self.forget(oldest);
        }
        self.forget(scope);
        self.bytes += table.bytes;
        self.tables.insert(scope, table);
        self.order.push_back(scope);
    }
}

/// The graph's tables, attached to one already-open store handle (the
/// `memory` keyspace: different entity class, different ns, one tree — the
/// same rule flat memory and the code store follow).
pub struct GraphStore {
    entities: Collection<FjallStore, EntityKey, EntityRow>,
    /// One handle for the writes the collections cannot express in their
    /// own batch (the dictionary, and the edge's dynamic segment). The
    /// edge collection's own handle is built PER CALL — see `facts()`.
    store: FjallStore,
    next_node: u64,
    next_edge: u64,
    /// The built-in embedding engine (ADR-0003). `None` = no engine attached:
    /// rows land with an EMPTY vector frame and retrieval has nothing to
    /// rank (a legal, empty result — never an error).
    embedder: Option<Arc<Embedder>>,
    /// Per-scope vector tables, built from the KV on first use (ADR-0003's
    /// "per-ns in-memory table + brute-force inner product"). Bounded: see
    /// `TABLE_BUDGET_BYTES`.
    tables: ScopeTables,
    /// How those tables are scanned. `Flat` is the only strategy today.
    strategy: ScanStrategy,
    /// Next community id per scope (ADR-0010 §2 — a fresh seed per landed
    /// node). Loaded lazily as one past the scope's current maximum; the
    /// capsule table itself is the evidence, so there is no second counter
    /// row to keep in sync.
    next_community: HashMap<u64, u64>,
}

impl GraphStore {
    /// Open (or create) the graph tables in the `memory` keyspace at
    /// `path` — the standalone form (tests, single-table tools).
    pub fn open(path: &std::path::Path) -> Result<Self, fjall::Error> {
        Ok(Self::wrap(FjallStore::open(path, crate::KEYSPACE_MEMORY)?))
    }

    /// Attach the graph tables to an already-open store handle — the
    /// attach point `crate::service::open_service` uses.
    pub fn wrap(store: FjallStore) -> Self {
        let entities = <EntityRow as Document>::collection(store.clone());
        // Id recovery, both tables: scan the last key in table order.
        // Entity keys are `[scope][node_id]`, so the last key carries the
        // global max node id; the edge primary keys are `[edge_id]` under
        // ns 37's primary slot, in the same BE order. A fresh table
        // starts at 1 — ids are unique but not contiguous per scope.
        let next_node = entities
            .scan_keys()
            .last()
            .map_or(1, |k| k.node_id.saturating_add(1));
        // `[ns 2B][0x00]` — the primary slot is a fixed face, one byte
        // (okm ADR-0030), so the header is 3 bytes.
        let mut pk_prefix = Vec::with_capacity(3);
        pk_prefix.extend_from_slice(<EntityRow as Document>::NS_PREFIX);
        write_slot(&mut pk_prefix, PRIMARY_SLOT);
        let mut edge_prefix = pk_prefix.clone();
        edge_prefix[0..2].copy_from_slice(FactEdge::NS_PREFIX);
        let next_edge = store
            .scan_suffix(&edge_prefix)
            .iter()
            .last()
            .and_then(|sfx| sfx.as_slice().try_into().ok().map(u64::from_be_bytes))
            .map_or(1, |id| id.saturating_add(1));
        Self {
            entities,
            store,
            next_node,
            next_edge,
            embedder: None,
            tables: ScopeTables::default(),
            strategy: ScanStrategy::Flat,
            next_community: HashMap::new(),
        }
    }

    /// Attach the built-in embedding engine (ADR-0003). `open_service` cannot
    /// know about it — the engine is a serving-process concern — so the
    /// serving side attaches it here, before the store goes behind its lock.
    pub fn with_embedder(mut self, engine: Arc<Embedder>) -> Self {
        self.embedder = Some(engine);
        self
    }

    /// The edge collection handle. Predicates allocate through the graph's
    /// own KIND vocabulary (0x04/0x05) and the edge's open prop names
    /// through the payload's FIELD dictionary (0x02/0x03) — two vocabularies,
    /// two id spaces, so the collision that used to force a fresh handle per
    /// call cannot happen. A per-call handle stays cheap (the vocabularies
    /// load on first probe, ids only grow).
    fn facts(&self) -> Graph<FjallStore, FactEdge> {
        Graph::new(self.store.clone())
    }

    /// Allocate (or look up) an ENTITY KIND token in the collection's own
    /// kind vocabulary (0x04/0x05). The entity's props are field names and
    /// live in 0x02/0x03, so the two never share an id.
    fn dict_id(&mut self, ns_prefix: &[u8], name: &str) -> u16 {
        let mut dict = DictCache::new(KIND_VOCAB);
        dict.id_for(&mut self.store, ns_prefix, name)
    }

    /// The reverse direction of the kind vocabulary.
    fn dict_name(&self, ns_prefix: &[u8], id: u16) -> Option<String> {
        let mut dict = DictCache::new(KIND_VOCAB);
        dict.name_for(&self.store, ns_prefix, id)
    }

    /// One node by id.
    pub fn entity(&self, scope: u64, node_id: u64) -> Option<EntityRow> {
        self.entities.get(&EntityKey { scope, node_id })
    }

    /// The exact name/alias entrance: scan the `by_name` face and verify
    /// BOTH the row's name and the decoded key's scope (the entry is
    /// `[name][scope][node_id]`, so a prefix scan reaches every scope and
    /// every longer name that shares the prefix — the text-first regime's
    /// documented cost, paid here by the two comparisons).
    pub fn entity_by_name(&mut self, scope: u64, name: &str) -> Option<(u64, EntityRow)> {
        self.entities
            .scan::<ByName>(name.as_bytes())
            .into_iter()
            .filter_map(|(key, row)| row.map(|row| (key.decoded, row)))
            .find(|(key, row)| key.scope == scope && row.name == name)
            .map(|(key, row)| (key.node_id, row))
    }

    /// The entity's kind token (its dictionary name).
    pub fn entity_kind(&mut self, kind_id: u16) -> Option<String> {
        self.dict_name(<EntityRow as Document>::NS_PREFIX, kind_id)
    }

    /// The entity's open attributes (the dynamic segment).
    pub fn entity_props(&self, scope: u64, node_id: u64) -> Option<BTreeMap<String, DynamicValue>> {
        let key = self.fields_key(
            <EntityRow as Document>::NS_PREFIX,
            &EntityKey { scope, node_id }.encode(),
        );
        let raw = self.store.get(&key)?;
        decode_props(&raw, &self.store, <EntityRow as Document>::NS_PREFIX)
    }

    /// One fact, read back by id.
    pub fn fact(&mut self, edge_id: u64) -> Option<FactBody> {
        let mut facts = self.facts();
        let body = facts.get_edge(edge_id)?;
        let kind = facts.predicate_name(body.predicate_id)?;
        Some(FactBody {
            edge_id,
            src: body.src,
            dst: body.dst,
            kind,
            edge: FactEdge::decode_payload(&body.payload),
        })
    }

    /// One fact, read back by id, with both endpoints resolved to their
    /// node rows (an absent endpoint renders as a node id and an empty
    /// name — never an error: the edge is truth even if its node row was
    /// removed).
    pub fn fact_view(&mut self, edge_id: u64) -> Option<FactView> {
        let body = self.facts().get_edge(edge_id)?;
        let kind = self.facts().predicate_name(body.predicate_id)?;
        Some(FactView {
            edge_id,
            kind,
            src: self.resolve_ref(&body.src),
            dst: self.resolve_ref(&body.dst),
            edge: FactEdge::decode_payload(&body.payload),
        })
    }

    /// One node's outgoing facts, endpoints resolved — the one-hop
    /// traversal the read face serves. `out_edges` is the traversal
    /// currency (untyped, no dictionary involved); each id is then read
    /// back as a whole fact.
    pub fn out_facts(&mut self, scope: u64, node_id: u64) -> Vec<FactView> {
        self.out_edges(scope, node_id)
            .into_iter()
            .filter_map(|edge_id| self.fact_view(edge_id))
            .collect()
    }

    /// Every node of one scope, key order — the whole-scope read a bulk
    /// consumer (the visualization door) serves. The scan is bounded by
    /// the primary-key prefix `[scope]`, so no other scope's rows are
    /// touched.
    pub fn scope_entities(&mut self, scope: u64) -> Vec<(u64, EntityRow)> {
        let mut prefix = Vec::with_capacity(3 + 8);
        prefix.extend_from_slice(<EntityRow as Document>::NS_PREFIX);
        write_slot(&mut prefix, PRIMARY_SLOT);
        prefix.extend_from_slice(&scope.to_be_bytes());
        self.store
            .scan_suffix(&prefix)
            .iter()
            .filter_map(|sfx| {
                let mut full = prefix.clone();
                full.extend_from_slice(sfx);
                let key = EntityKey::decode(&full[3..]);
                let row = self.entities.get(&key)?;
                Some((key.node_id, row))
            })
            .collect()
    }

    /// Every fact of one scope: each node's outgoing facts, node order
    /// then edge order. Each edge is reached exactly once — `ingest`
    /// creates both endpoints in the scope it was called with, so an
    /// edge's source is always in this list.
    pub fn scope_facts(&mut self, scope: u64) -> Vec<FactView> {
        let nodes: Vec<u64> = self
            .scope_entities(scope)
            .into_iter()
            .map(|(node_id, _)| node_id)
            .collect();
        let mut facts = Vec::new();
        for node_id in nodes {
            facts.extend(self.out_facts(scope, node_id));
        }
        facts
    }

    /// Vector landing (ADR-0003): embed the query, scan the scope's table by
    /// inner product, drop everything under [`LANDING_FLOOR`], return the top
    /// `k`. The table is DERIVED state — built from the KV on first use and
    /// dropped on every write — so it never drifts from the rows.
    pub fn retrieve(
        &mut self,
        scope: u64,
        query: &str,
        k: usize,
    ) -> Result<Vec<Seed>, IngestError> {
        let Some(engine) = self.embedder.clone() else {
            // No engine attached: no query vector can exist, so nothing
            // lands. That is a legal answer, not an error.
            return Ok(Vec::new());
        };
        if query.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let q = engine
            .embed(query)
            .map_err(|e| IngestError::Embedding(e.to_string()))?;
        self.load_vector_table(scope);
        let Some(table) = self.tables.get(scope) else {
            return Ok(Vec::new());
        };
        let mut hits: Vec<(usize, f32)> = match self.strategy {
            ScanStrategy::Flat => table
                .rows
                .iter()
                .enumerate()
                .map(|(i, row)| (i, dot(&q, &row.3)))
                .filter(|(_, score)| *score >= LANDING_FLOOR)
                .collect(),
        };
        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(k);
        Ok(hits
            .into_iter()
            .map(|(i, score)| {
                let (node_id, name, kind_id, _) = &table.rows[i];
                Seed {
                    node_id: *node_id,
                    score,
                    name: name.clone(),
                    kind: self
                        .dict_name(<EntityRow as Document>::NS_PREFIX, *kind_id)
                        .unwrap_or_default(),
                }
            })
            .collect())
    }

    /// Landing + one hop (ADR-0003's read shape: "vector landing + graph
    /// traversal"): the seeds, then every fact touching them in EITHER
    /// direction, deduplicated by edge id — a fact between two seeds appears
    /// once. One hop is the whole traversal today: deeper walks are the
    /// caller's composition over `out_edges`/`in_edges`.
    ///
    /// `ts_ms` is the CALLER's clock (ADR-0008 §3). When it rides the call,
    /// every served fact counts as one read and its weight is re-folded
    /// (ADR-0005's direct-RMW start); the served set is then ordered by
    /// that weight, highest first. Without a clock the answer is identical
    /// except that nothing is counted — a read nobody stamped is not a
    /// read, and an absent timestamp never invents decay.
    pub fn recall(
        &mut self,
        scope: u64,
        query: &str,
        k: usize,
        ts_ms: Option<u64>,
    ) -> Result<Recall, IngestError> {
        let seeds = self.retrieve(scope, query, k)?;
        let communities = self.capsules_of(scope, &seeds);
        let facts = self.expand(scope, &seeds, ts_ms)?;
        Ok(Recall {
            seeds,
            communities,
            facts,
        })
    }

    /// Landing's other half, on its own: every fact one hop out of the
    /// given seeds, both directions, deduplicated by edge id, ordered by
    /// weight (ADR-0005). A caller that landed by another entrance (the
    /// exact name face) or that composes a deeper walk starts here instead
    /// of re-landing; `ts_ms` behaves exactly as in [`recall`](Self::recall).
    pub fn expand(
        &mut self,
        scope: u64,
        seeds: &[Seed],
        ts_ms: Option<u64>,
    ) -> Result<Vec<FactView>, IngestError> {
        let mut seen = HashSet::new();
        let mut facts = Vec::new();
        for seed in seeds {
            let mut ids = self.out_edges(scope, seed.node_id);
            ids.extend(self.in_edges(scope, seed.node_id));
            for edge_id in ids {
                if !seen.insert(edge_id) {
                    continue;
                }
                let view = match ts_ms {
                    Some(now) => self.bump_view(edge_id, Touch::Read, now)?,
                    None => self.fact_view(edge_id),
                };
                if let Some(view) = view {
                    facts.push(view);
                }
            }
        }
        // Weight-ordered (ADR-0005). Stable, so facts of equal weight keep
        // the seed order that pulled them in.
        facts.sort_by_key(|f| std::cmp::Reverse(f.edge.weight));
        Ok(facts)
    }

    /// Count one access to one edge at the caller's clock and answer the
    /// new view of it (ADR-0005's counters): a recall is a READ, a tool
    /// consult is a TOOL, and both re-fold the weight. `None` = the edge
    /// does not exist (the door turns that into 404).
    ///
    /// The scope rides the path, not the key: an edge carries no scope
    /// segment (its endpoints do), so the check decodes the source ref.
    pub fn touch(
        &mut self,
        scope: u64,
        edge_id: u64,
        kind: Touch,
        now: u64,
    ) -> Result<Option<FactView>, IngestError> {
        let Some(body) = self.facts().get_edge(edge_id) else {
            return Ok(None);
        };
        if EntityKey::decode(&body.src.pkey).scope != scope {
            return Ok(None);
        }
        self.bump_view(edge_id, kind, now)
    }

    /// The shared write half of a touch: decode the edge, bump the counter,
    /// re-fold the weight, and rewrite the row in ONE batch. The declared
    /// faces are derived from the row, so `by_weight` follows — which is
    /// why this is `unlink_into` + `link_into` rather than a bare put: the
    /// old weight entry must not survive the new weight.
    fn bump_view(
        &mut self,
        edge_id: u64,
        kind: Touch,
        now: u64,
    ) -> Result<Option<FactView>, IngestError> {
        let mut facts = self.facts();
        let Some(body) = facts.get_edge(edge_id) else {
            return Ok(None);
        };
        let Some(predicate) = facts.predicate_name(body.predicate_id) else {
            return Ok(None);
        };
        let edge = FactEdge::decode_payload(&body.payload).touched(kind, now);
        let head = EdgeHead {
            src: body.src.clone(),
            dst: body.dst.clone(),
            predicate: predicate.clone(),
        };
        let mut batch = MemBatch::default();
        facts
            .unlink_into(&mut batch, edge_id)
            .map_err(IngestError::Storage)?;
        facts
            .link_into(&mut batch, &head, &edge, edge_id)
            .map_err(IngestError::Storage)?;
        self.store
            .commit_batch(batch)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        Ok(Some(FactView {
            edge_id,
            kind: predicate,
            src: self.resolve_ref(&body.src),
            dst: self.resolve_ref(&body.dst),
            edge,
        }))
    }

    /// Build (or refresh the LRU position of) one scope's table. Rows written
    /// without an engine carry an EMPTY frame and are skipped by construction.
    fn load_vector_table(&mut self, scope: u64) {
        if self.tables.contains(scope) {
            self.tables.touch(scope);
            return;
        }
        let mut rows = Vec::new();
        let mut bytes = 0usize;
        for (node_id, row) in self.scope_entities(scope) {
            if row.embedding.is_empty() {
                continue;
            }
            bytes += row.embedding.len() * std::mem::size_of::<f32>() + row.name.len();
            rows.push((node_id, row.name, row.kind_id, row.embedding.elems));
        }
        self.tables.insert(scope, ScopeTable { rows, bytes });
    }

    /// A `NodeRef` back to its node: the ref is `[ns 2B][len varint]
    /// [entity key]`, so the entity key decodes straight out of it and
    /// the scope rides along (nothing external is consulted).
    pub fn resolve_ref(&mut self, node: &NodeRef) -> ResolvedNode {
        let key = EntityKey::decode(&node.pkey);
        let row = self.entity(key.scope, key.node_id);
        let (kind, literal_num, name) = match row {
            Some(row) => (
                self.entity_kind(row.kind_id).unwrap_or_default(),
                row.literal_num,
                row.name,
            ),
            None => (String::new(), 0, String::new()),
        };
        ResolvedNode {
            scope: key.scope,
            node_id: key.node_id,
            kind,
            literal_num,
            name,
        }
    }

    /// Outgoing edge ids of one node (the traversal currency).
    pub fn out_edges(&mut self, scope: u64, node_id: u64) -> Vec<u64> {
        self.facts().out_edges(&self.node_ref(scope, node_id))
    }

    /// Incoming edge ids of one node — the in-direction mirror of
    /// [`out_edges`](Self::out_edges). Both directions matter to a landing:
    /// a seed is useful through the facts pointing AT it as well.
    pub fn in_edges(&mut self, scope: u64, node_id: u64) -> Vec<u64> {
        self.facts().in_edges(&self.node_ref(scope, node_id))
    }

    /// Outgoing edge ids of one predicate out of one node — the 0x09 face,
    /// and the resolution path's incumbent scan.
    pub fn typed_out(&mut self, scope: u64, node_id: u64, kind: &str) -> Vec<u64> {
        self.facts()
            .typed_out(kind, &self.node_ref(scope, node_id))
    }

    /// Land one fact (ADR-0010 §1 steps 1–2): resolve both endpoints to
    /// nodes, then link the fact or adjudicate it against an incumbent.
    pub fn ingest(&mut self, scope: u64, fact: &FactTuple) -> Result<IngestOutcome, IngestError> {
        let src = self.resolve(scope, &fact.head)?;
        let dst = self.resolve(scope, &fact.tail)?;
        let functional = matches!(
            fact.rel.props.get(FUNCTIONAL_PROP),
            Some(DynamicValue::Bool(true))
        );
        let since = match fact.rel.props.get("since") {
            Some(DynamicValue::UInt(v)) => *v,
            Some(DynamicValue::Int(v)) => *v as u64,
            _ => 0,
        };
        let confidence = match fact.rel.props.get("confidence") {
            Some(DynamicValue::UInt(v)) => (*v).min(255) as u8,
            Some(DynamicValue::Int(v)) => (*v).clamp(0, 255) as u8,
            _ => 255,
        };
        // The rel props that ride the edge's dynamic segment: the open
        // modifiers. The declared fields are projections of their props —
        // kept out of the dynamic entry so one datum is not stored twice;
        // `functional` is the caller's declaration, the adjudication
        // input, and is kept (the assertion's own record). The weight
        // system's five counters are the same kind of exclusion: system
        // metadata never rides the business layer (ADR-0005).
        let props: BTreeMap<String, DynamicValue> = fact
            .rel
            .props
            .iter()
            .filter(|(k, _)| {
                !matches!(
                    k.as_str(),
                    "since"
                        | "confidence"
                        | "weight"
                        | "read_count"
                        | "write_count"
                        | "tool_count"
                        | "last_read"
                        | "last_write"
                )
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        if functional {
            if let Some(&incumbent) = self.typed_out(scope, src, &fact.rel.name).first() {
                let body = self
                    .facts()
                    .get_edge(incumbent)
                    .ok_or_else(|| IngestError::Storage(format!("edge {incumbent} unreadable")))?;
                let old = FactEdge::decode_payload(&body.payload);
                if since > old.since || confidence >= old.confidence {
                    // The assertion's own write: the counters carry over,
                    // `since`/`confidence` are the new fact's, and the
                    // weight folds in THIS write plus the decay the gap
                    // accrued (ADR-0005).
                    let next = FactEdge {
                        since,
                        confidence,
                        ..old
                    }
                    .touched(Touch::Write, since);
                    // Rewrite in place: same edge_id, new endpoints and
                    // attributes, in ONE engine batch (okm's `unlink_into` +
                    // `link_into` — ADR-0010's owed item, paid 2026-10-19).
                    // The old props entry is deleted in the same batch, so a
                    // rewrite with no props leaves none behind.
                    let mut batch = MemBatch::default();
                    self.facts()
                        .unlink_into(&mut batch, incumbent)
                        .map_err(IngestError::Storage)?;
                    self.write_edge_into(
                        &mut batch,
                        incumbent,
                        &next,
                        scope,
                        src,
                        dst,
                        &fact.rel.name,
                        &props,
                    )?;
                    self.store
                        .commit_batch(batch)
                        .map_err(|e| IngestError::Storage(e.to_string()))?;
                    self.bin(scope, &[src, dst], since)?;
                    return Ok(IngestOutcome::Rewritten { edge_id: incumbent });
                }
                return Ok(IngestOutcome::Dropped { edge_id: incumbent });
            }
        }
        let edge = FactEdge {
            since,
            confidence,
            ..Default::default()
        }
        .touched(Touch::Write, since);
        let edge_id = self.next_edge;
        self.next_edge += 1;
        self.write_edge(edge_id, &edge, scope, src, dst, &fact.rel.name, &props)?;
        self.bin(scope, &[src, dst], since)?;
        Ok(IngestOutcome::Linked { edge_id })
    }

    /// Resolve a term to a node id: the exact name entrance first, then a
    /// new node (kind allocated through the collection's dictionary).
    /// Props land on the node row — an entity's attributes are shared by
    /// every fact about it, so they MERGE into the node's dynamic
    /// segment (a same-named attribute takes the newer value; the rest
    /// survive, unlike a wholesale replace).
    fn resolve(&mut self, scope: u64, term: &Term) -> Result<u64, IngestError> {
        if let Some((node_id, _)) = self.entity_by_name(scope, &term.value) {
            self.merge_props(scope, node_id, &term.props);
            return Ok(node_id);
        }
        let kind_id = self.dict_id(<EntityRow as Document>::NS_PREFIX, &term.type_label);
        let node_id = self.next_node;
        self.next_node += 1;
        // ADR-0003: the vector is computed INSIDE k10r, over the node's NAME
        // (an entity's semantic handle). No engine = EMPTY frame, a legal row
        // the retrieval side skips. An engine that fails to embed is an error
        // the caller sees — never a silently vectorless node.
        let embedding = match &self.embedder {
            Some(engine) => Vector::new(
                engine
                    .embed(&term.value)
                    .map_err(|e| IngestError::Embedding(e.to_string()))?,
            ),
            None => Vector::new(Vec::new()),
        };
        let community_id = self.next_community_id(scope);
        let row = EntityRow {
            kind_id,
            literal_num: literal_num(term),
            community_id,
            name: term.value.clone(),
            embedding,
        };
        let key = EntityKey { scope, node_id };
        self.entities.put(&key, &row);
        // A fresh community's capsule row: the seed id always names a real
        // row, so the listing sees the community before its first summary
        // (state = pending, nothing summarized yet). Only when absent — an
        // overwrite would reset a live community's uncompressed counter.
        if self.capsule(scope, community_id).is_none() {
            self.capsules().put(
                &CommunityKey {
                    scope,
                    community_id,
                },
                &CommunityRow::default(),
            );
        }
        // The scope's in-memory vector table is derived state: drop it rather
        // than maintain it row by row (it reloads on the next retrieval).
        self.tables.forget(scope);
        if !term.props.is_empty() {
            self.entities.put_fields(&key, &term.props);
        }
        Ok(node_id)
    }

    /// Merge props into an existing node's dynamic segment.
    fn merge_props(&mut self, scope: u64, node_id: u64, props: &BTreeMap<String, DynamicValue>) {
        if props.is_empty() {
            return;
        }
        let key = EntityKey { scope, node_id };
        let mut merged = self.entities.get_fields(&key).unwrap_or_default();
        for (k, v) in props {
            merged.insert(k.clone(), v.clone());
        }
        self.entities.put_fields(&key, &merged);
    }

    /// Write one edge and its open props in ONE engine batch (ADR-0002:
    /// one WriteBatch = one transaction — kind allocation is the only
    /// thing okm commits separately, and an extra dictionary entry is
    /// harmless by its own rule).
    #[allow(clippy::too_many_arguments)]
    fn write_edge(
        &mut self,
        edge_id: u64,
        edge: &FactEdge,
        scope: u64,
        src: u64,
        dst: u64,
        kind: &str,
        props: &BTreeMap<String, DynamicValue>,
    ) -> Result<(), IngestError> {
        let mut batch = MemBatch::default();
        self.write_edge_into(&mut batch, edge_id, edge, scope, src, dst, kind, props)?;
        self.store
            .commit_batch(batch)
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// The edge's full write set into an externally owned batch — the
    /// cross-collection atomic path, and the rewrite's write half.
    #[allow(clippy::too_many_arguments)] // one edge's own fields, not a grab bag
    fn write_edge_into(
        &mut self,
        batch: &mut MemBatch,
        edge_id: u64,
        edge: &FactEdge,
        scope: u64,
        src: u64,
        dst: u64,
        kind: &str,
        props: &BTreeMap<String, DynamicValue>,
    ) -> Result<(), IngestError> {
        let head = EdgeHead {
            src: self.node_ref(scope, src),
            dst: self.node_ref(scope, dst),
            predicate: kind.to_string(),
        };
        let mut facts = self.facts();
        facts
            .link_into(batch, &head, edge, edge_id)
            .map_err(IngestError::Storage)?;
        // The dynamic segment is k10r's own entry, not okm's: delete it here
        // so a rewrite cannot inherit the previous assertion's props.
        batch.del(&self.fields_key(FactEdge::NS_PREFIX, &edge_id.to_be_bytes()));
        if !props.is_empty() {
            let mut body = Vec::new();
            // The prop names are FIELD names: their own vocabulary (0x02/0x03),
            // untouched by the predicate allocation link_into just did.
            let mut dict = DictCache::new(FIELD_DICT);
            {
                let store = &mut self.store;
                let mut resolver = |name: &str| dict.id_for(store, FactEdge::NS_PREFIX, name);
                for (name, value) in props {
                    put_frame_named(&mut body, name, value, &mut resolver);
                }
            }
            batch.put(
                self.fields_key(FactEdge::NS_PREFIX, &edge_id.to_be_bytes()),
                body,
            );
        }
        Ok(())
    }

    /// The fact's open props (its dynamic segment), when it has one.
    pub fn fact_props(&self, edge_id: u64) -> Option<BTreeMap<String, DynamicValue>> {
        let key = self.fields_key(FactEdge::NS_PREFIX, &edge_id.to_be_bytes());
        let raw = self.store.get(&key)?;
        decode_props(&raw, &self.store, FactEdge::NS_PREFIX)
    }

    /// One node as a graph endpoint: the ref carries the entity key's own
    /// bytes, so any collection participates with zero declaration.
    fn node_ref(&self, scope: u64, node_id: u64) -> NodeRef {
        NodeRef::new(
            <EntityRow as Document>::NS_PREFIX,
            &EntityKey { scope, node_id }.encode(),
        )
    }

    /// A dynamic-segment key: `[ns 2B][0x01][key payload]` — the dynamic
    /// segment is a FIXED FACE, one byte (okm ADR-0030), the same key
    /// `Collection::put_fields` writes.
    fn fields_key(&self, ns_prefix: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut key = Vec::with_capacity(3 + payload.len());
        key.extend_from_slice(ns_prefix);
        write_slot(&mut key, DYNAMIC_SLOT);
        key.extend_from_slice(payload);
        key
    }
}

/// Inner product of two L2-normalized vectors (= cosine, ADR-0003's metric).
/// A length mismatch means the table was built by a different model than the
/// query: score it as unreachable rather than compare across spaces.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return f32::NEG_INFINITY;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A literal's integer projection: the value's own text parsed as an
/// integer (dates and non-numeric strings project to 0 — a canonical
/// date-format projection is an owed ruling, not a guess).
fn literal_num(term: &Term) -> i64 {
    if term.type_label != LITERAL_KIND {
        return 0;
    }
    term.value.parse::<i64>().unwrap_or(0)
}

/// Decode a dynamic-segment entry to its name-keyed values (the field-name
/// dictionary resolves the ids).
fn decode_props(
    raw: &[u8],
    store: &FjallStore,
    ns_prefix: &[u8],
) -> Option<BTreeMap<String, DynamicValue>> {
    let mut dict = DictCache::new(FIELD_DICT);
    let frames =
        okm_core::obj_dynamic::decode_named(raw, &mut |id| dict.name_for(store, ns_prefix, id));
    if frames.is_empty() {
        return None;
    }
    let mut out = BTreeMap::new();
    for frame in frames {
        if let Some(name) = dict.name_for(store, ns_prefix, frame.id) {
            out.insert(name, frame.value);
        }
    }
    Some(out)
}

// ---- ADR-0010 steps 3–4: the write-side binning pass and the read side

impl GraphStore {
    /// The capsule collection's handle (reads only; the batched writes build
    /// their keys directly, since the row declares no faces).
    fn capsules(&self) -> Collection<FjallStore, CommunityKey, CommunityRow> {
        <CommunityRow as Document>::collection(self.store.clone())
    }

    /// The next community id of one scope: one past the scope's current
    /// maximum. The capsule table IS the evidence (every issued id owns a
    /// row), so this is the HighWater semantics without a second counter
    /// row to keep in sync — single writer, never reused.
    fn next_community_id(&mut self, scope: u64) -> u64 {
        if !self.next_community.contains_key(&scope) {
            let mut prefix = Vec::with_capacity(3 + 8);
        prefix.extend_from_slice(CommunityRow::NS_PREFIX);
        write_slot(&mut prefix, PRIMARY_SLOT);
            prefix.extend_from_slice(&scope.to_be_bytes());
            let max = self
                .store
                .scan_suffix(&prefix)
                .iter()
                .filter_map(|sfx| {
                    if sfx.len() < 8 {
                        return None;
                    }
                    Some(u64::from_be_bytes(sfx[..8].try_into().ok()?))
                })
                .max()
                .unwrap_or(0);
            self.next_community.insert(scope, max + 1);
        }
        // Issuing ADVANCES the watermark — a cached "next" that did not move
        // hands the same id to every node landed after it, which silently
        // puts unrelated seeds in one community.
        let next = self.next_community.get_mut(&scope).expect("just seeded");
        let id = *next;
        *next = id.saturating_add(1);
        id
    }

    /// The entity primary key `[ns 2B][0x00][scope][node_id]`.
    fn entity_pkey(&self, key: &EntityKey) -> Vec<u8> {
        let mut buf = Vec::with_capacity(3 + 16);
        buf.extend_from_slice(EntityRow::NS_PREFIX);
        write_slot(&mut buf, PRIMARY_SLOT);
        buf.extend_from_slice(&key.encode());
        buf
    }

    /// Write one entity row (primary + every declared face) into an owned
    /// batch. The OLD row's face entries are deleted first when they are no
    /// longer produced by the new one — okm's document overwrite leaves a
    /// stale index entry dangling (the entry key encodes the indexed value,
    /// so a changed value lands at a new key), and a re-binned node's old
    /// `by_community` entry is exactly that case.
    fn write_entity_into(
        &self,
        batch: &mut MemBatch,
        key: &EntityKey,
        row: &EntityRow,
        old: Option<&EntityRow>,
    ) {
        let header = EntityRow::NS_PREFIX;
        if let Some(old) = old {
            let stale: HashSet<Vec<u8>> = EntityRow::index_entries(key, row, header)
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            for (face, _) in EntityRow::index_entries(key, old, header) {
                if !stale.contains(&face) {
                    batch.del(&face);
                }
            }
        }
        batch.put(self.entity_pkey(key), row.encode_payload());
        for (face, value) in EntityRow::index_entries(key, row, header) {
            batch.put(face, value);
        }
    }

    /// The capsule key `[ns 2B][0x00][scope][community_id]`.
    fn community_pkey(&self, scope: u64, community_id: u64) -> Vec<u8> {
        let mut buf = Vec::with_capacity(3 + 16);
        buf.extend_from_slice(CommunityRow::NS_PREFIX);
        write_slot(&mut buf, PRIMARY_SLOT);
        buf.extend_from_slice(&scope.to_be_bytes());
        buf.extend_from_slice(&community_id.to_be_bytes());
        buf
    }

    /// ADR-0010 §2–§4's background pass, run after a fact lands: load the
    /// 2-hop neighbourhood, run incremental LPA over it, write the winning
    /// community on every node that moved, and update the capsules of the
    /// communities the fact joined (the uncompressed counter, the density
    /// trigger, the `pending` mark). One engine batch covers the whole
    /// pass.
    ///
    /// No LLM, no transport, no global graph: the vote is local and the
    /// weights are ADR-0005's — a fresh assertion weighs `ln(2)×0.5`, so a
    /// vote never degenerates to an all-zero tie.
    fn bin(&mut self, scope: u64, touched: &[u64], now: u64) -> Result<(), IngestError> {
        let local = self.local_subgraph(scope, touched)?;
        if local.is_empty() {
            return Ok(());
        }
        let members: HashSet<u64> = local.keys().copied().collect();
        let mut adjacency: HashMap<u64, Vec<(u64, f64)>> = HashMap::new();
        for &node in &members {
            let mut ids = self.out_edges(scope, node);
            ids.extend(self.in_edges(scope, node));
            let mut seen = HashSet::new();
            for edge_id in ids {
                if !seen.insert(edge_id) {
                    continue;
                }
                let Some(body) = self.facts().get_edge(edge_id) else {
                    continue;
                };
                let src = EntityKey::decode(&body.src.pkey).node_id;
                let dst = EntityKey::decode(&body.dst.pkey).node_id;
                let other = if src == node { dst } else { src };
                if other == node || !members.contains(&other) {
                    continue;
                }
                let edge = FactEdge::decode_payload(&body.payload);
                let vote = (edge.weight as f64 / WEIGHT_SCALE) * (edge.confidence as f64 / 255.0);
                adjacency.entry(node).or_default().push((other, vote));
            }
        }

        let mut assign: HashMap<u64, u64> = local
            .iter()
            .map(|(id, row)| (*id, row.community_id))
            .collect();
        // A row that predates the field (community 0) is seeded here rather
        // than voting for "unassigned".
        let mut ids: Vec<u64> = assign.keys().copied().collect();
        ids.sort_unstable();
        for id in &ids {
            if assign[id] == 0 {
                let fresh = self.next_community_id(scope);
                assign.insert(*id, fresh);
            }
        }

        for _ in 0..LPA_ROUNDS {
            let mut changed = false;
            for node in &ids {
                let Some(neighbors) = adjacency.get(node) else {
                    continue;
                };
                if neighbors.is_empty() {
                    continue;
                }
                let mut votes: BTreeMap<u64, f64> = BTreeMap::new();
                for (other, weight) in neighbors {
                    *votes.entry(assign[other]).or_insert(0.0) += *weight;
                }
                // Highest accumulated vote wins; a tie goes to the SMALLER
                // community id, so the pass is deterministic.
                let best = votes
                    .iter()
                    .max_by(|a, b| {
                        a.1.partial_cmp(b.1)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| b.0.cmp(a.0))
                    })
                    .map(|(community, _)| *community);
                if let Some(best) = best {
                    if assign[node] != best {
                        assign.insert(*node, best);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }

        let mut batch = MemBatch::default();
        for node in &ids {
            let row = &local[node];
            let winning = assign[node];
            if row.community_id != winning {
                let mut next = row.clone();
                next.community_id = winning;
                let key = EntityKey {
                    scope,
                    node_id: *node,
                };
                self.write_entity_into(&mut batch, &key, &next, Some(row));
            }
        }

        // A capsule whose last member just left is dropped in the same
        // batch: the seed's row would otherwise be a phantom that the
        // consolidation pull hands out empty. The face scan sees the
        // PRE-batch state, so the nodes this pass moved are subtracted from
        // it rather than looked up.
        let moved: HashSet<u64> = ids
            .iter()
            .filter(|node| local[*node].community_id != assign[*node])
            .copied()
            .collect();
        let still_used: HashSet<u64> = assign.values().copied().collect();
        for row in local.values() {
            let abandoned = row.community_id;
            if abandoned == 0 || still_used.contains(&abandoned) {
                continue;
            }
            let remaining = self
                .community_members(scope, abandoned)
                .into_iter()
                .filter(|(member, _)| !moved.contains(member))
                .count();
            if remaining == 0 {
                batch.del(&self.community_pkey(scope, abandoned));
            }
        }

        // The communities THIS fact joined (the endpoints' final homes) get
        // the uncompressed increment and the density check; the endpoints
        // may have landed in one community, so dedupe.
        let mut joined: BTreeSet<u64> = BTreeSet::new();
        for node in touched {
            if let Some(community) = assign.get(node) {
                joined.insert(*community);
            }
        }
        for community in joined {
            let mut row = self.capsule(scope, community).unwrap_or_default();
            row.uncompressed = row.uncompressed.saturating_add(1);
            let (vertices, edges) = self.community_size(scope, community);
            let density = if vertices < DENSITY_MIN_VERTICES {
                0.0
            } else {
                2.0 * edges as f64 / (vertices as f64 * (vertices as f64 - 1.0))
            };
            if density >= DENSITY_THRESHOLD || row.uncompressed >= UNCOMPRESSED_WATERMARK {
                row.state = STATE_PENDING;
            }
            row.updated_at = now;
            batch.put(self.community_pkey(scope, community), row.encode_payload());
        }
        self.store
            .commit_batch(batch)
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// The nodes within `LPA_RADIUS` hops of the touched ones, loaded with
    /// their rows (the temporary in-memory graph the vote runs on).
    fn local_subgraph(
        &mut self,
        scope: u64,
        touched: &[u64],
    ) -> Result<HashMap<u64, EntityRow>, IngestError> {
        let mut seen: HashSet<u64> = HashSet::new();
        let mut frontier: Vec<u64> = Vec::new();
        for node in touched {
            if seen.insert(*node) {
                frontier.push(*node);
            }
        }
        for _ in 0..LPA_RADIUS {
            let mut next = Vec::new();
            for node in frontier.drain(..) {
                let mut ids = self.out_edges(scope, node);
                ids.extend(self.in_edges(scope, node));
                for edge_id in ids {
                    let Some(body) = self.facts().get_edge(edge_id) else {
                        continue;
                    };
                    let src = EntityKey::decode(&body.src.pkey).node_id;
                    let dst = EntityKey::decode(&body.dst.pkey).node_id;
                    let other = if src == node { dst } else { src };
                    if seen.insert(other) {
                        next.push(other);
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        let mut local = HashMap::new();
        for node in seen {
            if let Some(row) = self.entity(scope, node) {
                local.insert(node, row);
            }
        }
        Ok(local)
    }

    /// One community's size, `(|V|, |E|)` over its internal edges — the
    /// density water line's two numbers (ADR-0010 §3). A vertex set of one
    /// has no internal edge to speak of, so callers treat it as density 0.
    fn community_size(&mut self, scope: u64, community: u64) -> (usize, usize) {
        let members: HashSet<u64> = self
            .community_members(scope, community)
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        let mut edges = 0usize;
        for &node in &members {
            for edge_id in self.out_edges(scope, node) {
                if let Some(body) = self.facts().get_edge(edge_id) {
                    let dst = EntityKey::decode(&body.dst.pkey).node_id;
                    if members.contains(&dst) {
                        edges += 1;
                    }
                }
            }
        }
        (members.len(), edges)
    }

    /// Every capsule of one scope, community-id order — the write-side
    /// dashboard's and the consolidation pull's listing.
    pub fn communities(&mut self, scope: u64) -> Vec<(u64, CommunityRow)> {
        let mut prefix = Vec::with_capacity(3 + 8);
        prefix.extend_from_slice(CommunityRow::NS_PREFIX);
        write_slot(&mut prefix, PRIMARY_SLOT);
        prefix.extend_from_slice(&scope.to_be_bytes());
        let table = self.capsules();
        self.store
            .scan_suffix(&prefix)
            .iter()
            .filter_map(|sfx| {
                if sfx.len() < 8 {
                    return None;
                }
                let community = u64::from_be_bytes(sfx[..8].try_into().ok()?);
                let row = table.get(&CommunityKey {
                    scope,
                    community_id: community,
                })?;
                Some((community, row))
            })
            .collect()
    }

    /// One capsule, if the community exists.
    pub fn capsule(&mut self, scope: u64, community: u64) -> Option<CommunityRow> {
        self.capsules().get(&CommunityKey {
            scope,
            community_id: community,
        })
    }

    /// The community's members (the `by_community` face — the drafts'
    /// `index:community:`, now a declared access method). The row's own
    /// `community_id` is re-checked: the face entry is derived, and a
    /// re-bin deletes the stale one, so this is a belt-and-braces compare,
    /// not a filter over a table scan.
    pub fn community_members(&mut self, scope: u64, community: u64) -> Vec<(u64, EntityRow)> {
        self.entities
            .scan::<ByCommunity>(&community.to_be_bytes())
            .into_iter()
            .filter_map(|(key, row)| row.map(|row| (key.decoded, row)))
            .filter(|(key, row)| key.scope == scope && row.community_id == community)
            .map(|(key, row)| (key.node_id, row))
            .collect()
    }

    /// The facts INSIDE one community (both endpoints members), endpoints
    /// resolved — the bucket read of ADR-0010 §5 step 3.
    pub fn community_facts(&mut self, scope: u64, community: u64) -> Vec<FactView> {
        let members: Vec<u64> = self
            .community_members(scope, community)
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        let in_community: HashSet<u64> = members.into_iter().collect();
        let mut facts = Vec::new();
        for &node in &in_community {
            for view in self.out_facts(scope, node) {
                if in_community.contains(&view.dst.node_id) {
                    facts.push(view);
                }
            }
        }
        facts
    }

    /// One community's consolidation packet (ADR-0010 §3's export, §4's
    /// input): the internal facts sliced into the canonical 8-slot tuples,
    /// one JSON array per line. The packet is the CALLER's to summarize —
    /// k10r holds no LLM and never calls out (ADR-0007).
    pub fn community_packet(&mut self, scope: u64, community: u64) -> Option<CommunityPacket> {
        let row = self.capsule(scope, community)?;
        let facts = self.community_facts(scope, community);
        let members = self.community_members(scope, community).len() as u32;
        let mut lines = Vec::with_capacity(facts.len());
        for fact in &facts {
            let head_props = self.entity_props(scope, fact.src.node_id).unwrap_or_default();
            let tail_props = self.entity_props(scope, fact.dst.node_id).unwrap_or_default();
            let rel_props: BTreeMap<String, DynamicValue> = self
                .fact_props(fact.edge_id)
                .unwrap_or_default()
                .into_iter()
                .filter(|(k, _)| k.as_str() != FUNCTIONAL_PROP)
                .collect();
            // The tuple IS the line: serde writes the eight slots
            // positionally and the props ride the value form's own serde —
            // no intermediate value tree, no hand-written conversion.
            let tuple = (
                fact.src.name.as_str(),
                fact.src.kind.as_str(),
                &head_props,
                fact.kind.as_str(),
                &rel_props,
                fact.dst.name.as_str(),
                fact.dst.kind.as_str(),
                &tail_props,
            );
            // These shapes cannot fail to serialize (string keys, no custom
            // impls); if one ever did, no packet beats a half packet.
            lines.push(serde_json::to_string(&tuple).ok()?);
        }
        Some(CommunityPacket {
            community_id: community,
            state: row.state,
            uncompressed: row.uncompressed,
            member_count: members,
            updated_at: row.updated_at,
            title: row.title,
            summary: row.summary,
            facts: lines,
        })
    }

    /// The consolidation write-back (ADR-0010 §4): a POINT overwrite of the
    /// capsule row — the latest summary replaces the previous one, the
    /// uncompressed counter resets, and the state becomes `done`. An absent
    /// community answers `None` (the door's 404): a summary of a community
    /// that never existed is a caller bug, not a new community.
    pub fn complete_community(
        &mut self,
        scope: u64,
        community: u64,
        title: &str,
        summary: &str,
        ts_ms: u64,
    ) -> Option<CommunityRow> {
        self.capsule(scope, community)?;
        let row = CommunityRow {
            updated_at: ts_ms,
            state: STATE_DONE,
            uncompressed: 0,
            title: title.to_string(),
            summary: summary.to_string(),
        };
        self.capsules().put(
            &CommunityKey {
                scope,
                community_id: community,
            },
            &row,
        );
        Some(row)
    }

    /// The capsules of the communities these nodes belong to, deduped —
    /// the layer-3 context a recall injects (ADR-0010 §5 step 2: one point
    /// read per community, no algorithm on the read path).
    fn capsules_of(&mut self, scope: u64, seeds: &[Seed]) -> Vec<Capsule> {
        let mut communities: BTreeSet<u64> = BTreeSet::new();
        for seed in seeds {
            if let Some(row) = self.entity(scope, seed.node_id) {
                if row.community_id != 0 {
                    communities.insert(row.community_id);
                }
            }
        }
        communities
            .into_iter()
            .filter_map(|community| {
                let row = self.capsule(scope, community)?;
                Some(Capsule {
                    community_id: community,
                    state: row.state,
                    updated_at: row.updated_at,
                    uncompressed: row.uncompressed,
                    title: row.title,
                    summary: row.summary,
                })
            })
            .collect()
    }
}
