//! The fact graph (ADR-0010 §1, write side): entities (ns 36) and facts
//! (ns 37), plus the deterministic `ingest(fact)` path that lands one
//! 8-slot fact tuple (ADR-0004).
//!
//! Shape, in okm's terms (ADR-0010 §1 — the retired `edge:{src}:{label}:{dst}`
//! prefix layouts were never reimplemented here):
//!
//! - **Entities** = a node collection (ns 36), ordinary document layout:
//!   `[scope][node_id]` primary, `kind` + `literal_num` as declared
//!   fields, `name` as the cold segment, and three declared 0x1 faces —
//!   `by_name` (the exact name/alias entrance), `by_kind` (the node-kind
//!   face). The kind is a CATEGORICAL
//!   FIELD — `kind: Vocab<u16>` (okm ADR-0032): the label set is that
//!   field's own data vocabulary, so the write path interns it and the read
//!   path resolves it back with no allocation code and no reserved faces,
//!   and the field is named `kind` rather than `kind_id` because the value
//!   IS the label. A literal's numeric form is a
//!   declared FIELD, so a numeric read stays an ordered key scan rather than
//!   vector distance; the face over it lands with the read that scans it.
//! - **Facts** = an okm edge collection (ns 37, one ns per graph): the
//!   declaration is `Facts` (`GraphEncode`, ns 37, member set generated from
//!   `#[ok_nodes(EntityRow, MemberRow)]` — people are nodes too, ADR-0014), the
//!   edge's payload is an ordinary obj payload
//!   carrying `since` / `confidence` / `weight` as declared fields
//!   (ADR-0010 §1's three algorithm-queried fields, one 0x1 face each), and
//!   the endpoints stay self-describing `NodeRef`s — a ref carries its own
//!   pkey width, so endpoints need no declaration. The relation NAME is the
//!   edge's PREDICATE: it allocates in the graph's own predicate vocabulary
//!   (0x04/0x05) and is listed by one prefix scan of it.
//!
//! - **A fact has no number** (okm ADR-0067): its identity is the tuple
//!   `(src, predicate, dst)`, carried by okm's [`EdgeKey`]. Every verb here
//!   takes that head, writing the same tuple twice is writing the same row,
//!   and a fact is addressed on the wire by [`key_hex`] — the tuple's own
//!   bytes, which is the only name an edge has.
//! - **The relation's open props** (condition / cause / strength / the
//!   per-assertion `functional` flag) ride the edge's dynamic segment
//!   (okm's fixed face 0x01, one entry per edge), built by okm's segment
//!   encoder (`obj_segment::encode`, ADR-0057); their names allocate in the
//!   field-name dictionary (0x02/0x03), separate from the predicate
//!   vocabulary.
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
//! ADR-0010's steps 3–4 are here: incremental LPA and community binning
//! run inside `ingest`, and the bins live where okm keeps groupings: a cluster
//! whose members are EDGES (ADR-0066). The node row carries no community, and
//! k10r keeps no counter — the store allocates cluster ids itself.
//! What is NOT here is a summary: the read side renders the community
//! (crate::view), so there is no capsule table to keep in step.
//!
//! The weight system (ADR-0005) is the edge's own metadata layer: the
//! counters ride the edge row (`read_count` / `write_count` /
//! `tool_count` / `last_read` / `last_write`), the folded `weight` is the
//! row's declared field, and its `by_weight` face follows the row — so a
//! weight change is one row rewrite, never a second index write plus a
//! tombstone. `recall` orders what it serves by that weight; the counters
//! it bumps are the caller's clock riding the call (`ts_ms`), because a
//! read without a clock is served but not counted.

use crate::channel::{MemberKey, MemberRow};
use crate::code::{CodeKey, CodeRow};
use crate::embedding::Embedder;
use crate::render::RenderFormat;
use crate::tasks::{TaskSheet, TASK_KIND};
use okm_core::okm_value::Variant;
use okm_core::parts;
use okm_core::Uuid7;
use okm_core::{
    scan_suffix_kv, Batch, Cluster, Collection, Decl, Document, DocumentEncode, EdgeHead, EdgeKey,
    FjallStore, Fragment, Graph, GraphEncode, Hyperedges, KeyEncode, Kind, KvIndex, NodeRef,
    NodeSet, Selection, Vector, VectorElem, VirtualStorage, Vocab,
};
use okm_lake::bridge::EdgeTable;
use okm_lake::snapshot::parquet_io::EdgeRestore;
use okm_vector::ExactIndex;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// Entity node collection (ADR-0010 §1's ns 36 — allocated at landing,
/// next free after the code store's 35).
pub const ENTITY_NS: u16 = 36;
/// The node vectors' own table (ADR-0019): same key type, one ns of its own.
/// 38-42 are spoken for (community seq, read cursor, log, members).
pub const NODE_VECTOR_NS: u16 = 43;
/// The facts' edge collection (ns 37, one ns per graph).
pub const FACT_NS: u16 = 37;
/// The identity collection's ns — channel members, which are ALSO the graph's
/// user nodes (ADR-0014). It lives in the channel keyspace, not this one.
pub const MEMBER_NS: u16 = 42;
/// The reserved type token that names a user endpoint (ADR-0004's open type
/// vocabulary: `literal` first, `user` second). A value under it is the wire
/// reference `member-{uuid}`, parsed into a node at ns 42 —
/// nothing is created on a miss, because a person is registered, never
/// discovered.
pub const USER_KIND: &str = "user";
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

// ---- the recency ring (the payload's tail) ---------------------------
//
// The counters answer "how many" and the two clocks answer "how recently".
// Neither answers "were those accesses bunched into one week or spread over
// two years", and the decay bill is made of exactly that distribution. The
// ring keeps the last K accesses in seconds since a fixed epoch.
//
// It is a WINDOW, not a history: older accesses stay summarized by the
// counters. That is the fixed price of keeping the row bounded, and it is
// deliberate — an unbounded per-access log would put 8 bytes of every touch
// into a row that every recall re-encodes (okm MODELING, the k10r case).
/// The ring's epoch: 2026-01-01T00:00:00Z in unix SECONDS. A u32 offset from
/// here spans ~136 years (to 2162). `Offset` would only buy this width as a
/// scalar FIELD spelling, while a `Vector` element is a plain `u32` — so the
/// base lives here as a consumer constant and okm stores the offsets alone.
pub const RECENT_BASE: u64 = 1_767_225_600;
/// Slots in the ring — the last K accesses, oldest first.
pub const RECENT_SLOTS: usize = 8;

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

/// One entity node. `kind` is a categorical field — `Vocab<u16>`, whose label
/// set is that field's own data vocabulary (okm ADR-0032): `put` interns the
/// label on write and `get` resolves it back, so neither side carries
/// allocation code and the field needs no reserved faces. Named `kind`, not
/// `kind_id`: the value is the label, the id is only its encoding.
///
/// The node carries NO community column: a community is an okm cluster over
/// EDGES (ADR-0066), so membership is the edge's row and the pass writes it
/// there. A node's communities are therefore the clusters of the facts that
/// touch it, and one node can sit in several.
///
/// `name` is an ATTRIBUTE (never key material): the human identity reaches
/// the exact entrance through the `by_name` face. `literal_num` is a
/// literal node's integer projection (0 for an ordinary entity) — a
/// declared, fixed-width, ordered field, so an interval or nearest read
/// stays a `scan_range` over a face rather than a vector-distance guess.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(EntityKey)]
#[ok_ns(36)]
#[ok_name("entities")]
#[ok_layout(version = 2)]
#[ok_index(
    // Payload fields only: the key tail appends the primary key, so the
    // entries land as `[kind][scope][node_id]` and `[name][scope][node_id]`.
    // Text-first regime: an exact match re-verifies the row's name and the
    // decoded key's scope — a prefix scan for `alpha` also reaches
    // `alphabet`, which is what dictionary order means.
    by_kind { fields(kind) },
    by_name { fields(name) },
)]
pub struct EntityRow {
    pub kind: Vocab<u16>,
    /// Invocations STARTED (ADR-0018): a failed call still counts, because
    /// "tried and broke" is what a reader is looking for.
    pub invoke_calls: u32,
    /// Invocations that returned. `calls - ok` is the failure count.
    pub invoke_ok: u32,
    /// The caller's clock at the last count, so "hot" and "abandoned" differ.
    pub last_invoked: u64,
    /// A literal node's integer projection (0 for an ordinary entity): a
    /// declared, fixed-width, ordered field, so a numeric read later stays
    /// a range scan rather than a vector-distance guess. No face is
    /// declared over it yet — the face lands with the read that scans it.
    #[ok_default(0)]
    pub literal_num: i64,
    pub name: String,
    // The vector is NOT here (ADR-0019). It moved to `NodeVector` (ns 43)
    // because this row is written WHOLE by every prop merge — the usage counters
    // included — and re-encoding a 1.5-2 KB embedding that a count cannot change
    // is the frequent writer paying for the rare field.
    //
    // `#[ok_layout(version = 1)]` is a STAMP, not a compatibility promise (okm
    // ADR-0042): a field is placed where it belongs, the layout number is reset
    // when the shape changes, and a store written by an older build is rebuilt or
    // carried over by export/import. Nothing cascades and the corpus is
    // regenerable, so an empty `data/` is the expected upgrade path — this is
    // what memory cores do before they hold anything that cannot be re-derived.
    //
}

/// What a split did: the entity that was divided, the one it was divided into,
/// and how many edges changed hands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SplitReport {
    pub node: u64,
    pub created: u64,
    pub repointed: usize,
}

/// A head in the terms a plan compares heads in: both endpoints as `(ns, pkey)`
/// plus the predicate id. The same shape as okm's internal `EdgeIdentity`, which
/// is not public — the merge only needs to answer "did two edges land on one
/// head", and that is a byte comparison.
type ClaimedHead = (u16, Vec<u8>, u16, u16, Vec<u8>);

/// What a merge did, in the caller's terms: which entity stands, how many edges
/// moved, how many died because the fold made them loops, and how many were
/// duplicates an incumbent row already stated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeReport {
    pub survivor: u64,
    pub repointed: usize,
    pub collapsed: usize,
    pub absorbed: usize,
}

/// The node's vector, out of the entity row (ADR-0019, ns 43).
///
/// Same KEY TYPE as the row it belongs to, which is what makes the side table
/// cheap: the key payload is byte-identical, so no lookup table joins them, and
/// the pair goes into one batch. A delete has to take both — nothing cascades in
/// this model, and nothing here deletes a node yet.
///
/// Presence is the boolean: no `Option`, no zero-length sentinel. A node
/// written without an engine attached has no row here, and retrieval skips it
/// the same way it used to skip an EMPTY frame.
///
/// It declares no face — a vector has no key order. `okm-vector`'s
/// `ExactIndex` rebuilds from THIS table, so the index's maintenance is its
/// own: a change in the entity row never has to work out whether it feeds a
/// heavyweight index.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(EntityKey)]
#[ok_ns(43)]
#[ok_name("node_vectors")]
pub struct NodeVector {
    /// The engine's output over the entity's name, a cold frame of ANY length,
    /// L2-normalized at the source so the contract's `cosine` is an inner
    /// product. EMPTY is legal (a row written with no engine) and is treated
    /// as absent, never as a zero vector.
    pub embedding: Vector<f32>,
}

use __OkmIndex_EntityRow_by_name as ByName;
/// The scope-prefixed weight face: one probe yields a scope's keys in weight
/// order, so the progressive read decodes k rows for k answers.
use __OkmIndex_Facts_by_scope_weight as ByScopeWeight;
/// The temporal face: ascending in `since`, walked in reverse for newest-first.
use __OkmIndex_Facts_by_since as BySince;

/// Bytes of the `by_since` face's declared field segment, under an empty probe:
/// `since` is a `u64` and okm writes a declared field as its own fixed-width
/// big-endian encoding (ADR-0016). A face entry is `[fields][head]`, so this is
/// the offset the head starts at.
const BY_SINCE_FIELDS: usize = 8;
/// Bytes of the `by_scope_weight` segment under a `scope` probe: the declared
/// fields are `(scope, weight)` in declaration order, so the probe eats `scope`
/// (`u64`, 8) and `weight` (`u32`, 4) is what is left in front of the head.
const BY_WEIGHT_FIELD: usize = 4;

/// k10r's label for the community cluster kind (okm ADR-0066). The store only
/// interns it; nothing in okm reads the word.
const COMMUNITY_KIND: &str = "community";

/// A fact: an edge of the graph. The declared fields are `since` (the
/// caller's clock, the adjudication input), `confidence` (the write-side's,
/// defaulting to full confidence so a fact that states none is not
/// penalized), `weight` (the weight system's fold, ADR-0005) and `scope` (the
/// denormalized ownership the ranking face is probed by — see below).
///
/// The four counters plus two clocks after `weight` are the weight
/// system's raw inputs (ADR-0005's `weight_meta`): read / write / tool
/// counts and the last access in each direction. They ride the SAME row
/// as the weight — okm's declared faces are derived from the row, so a
/// counter bump rewrites one row and its face entries follow it.
/// The delta-append table the ADR sketches is the multi-writer escape; it
/// folds into these same fields, so switching to it changes no schema.
#[derive(Clone, Debug, PartialEq, GraphEncode)]
#[ok_ns(37)]
#[ok_name("facts")]
#[ok_nodes(EntityRow, MemberRow)]
// Layout v2 appended the counters and clocks after `weight` (the hot width
// grew). v3 appends the recency ring at the TAIL, which is okm's one legal way
// to add a field: a payload written by an older version simply lacks the frame
// and decodes to the declared default, so no store rebuild is involved.
#[ok_layout(version = 3)]
// Each algorithm-queried field gets its own counting-family 0x1 index,
// declared here (a face is a declaration, never implied by the field's
// existence). Two faces only, because two reads exist; `by_confidence` and a
// bare `by_weight` are not declared — an unscanned face is a write nobody
// asked for, on every touch.
#[ok_index(
    // Two faces, and each names its reader — the discipline ADR-0005 records:
    // a face with no scanner is an extra index write on every touch, forever.
    // `by_since` answers the temporal entrance (`timeline`).
    by_since { fields(since) },
    // `by_scope_weight` answers the progressive one (`top_by_weight`). It is the
    // scope-prefixed form of a plain weight face, which it therefore replaces: a
    // weight-only face cannot answer "the heaviest facts of scope 7" without a row
    // fetch per key, because the scope lives on the endpoints, not in the index.
    by_scope_weight { fields(scope, weight) },
)]
pub struct Facts {
    /// The scope this fact lives in — a DENORMALIZED copy of its source
    /// endpoint's scope, written once and never changed. It exists so the
    /// `by_scope_weight` face can be probed by scope (PLAN 4a); the endpoints
    /// remain the authority, and `write_edge` sets this from the call's scope.
    #[ok_default(0)]
    pub scope: u64,
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
    /// The last `RECENT_SLOTS` accesses, oldest first, in SECONDS since
    /// `RECENT_BASE`; `0` is an unused slot (that instant is never a real
    /// access, so it doubles as the sentinel). Always `RECENT_SLOTS` long:
    /// `#[ok_len(8)]` is an encode-time contract, so `touch` pads rather than
    /// leaving a short ring behind, and a payload that predates the ring
    /// decodes padded through `#[ok_default]`.
    #[ok_default(::okm_core::Vector::new(vec![0u32; RECENT_SLOTS]))]
    #[ok_len(8)]
    pub recent: Vector<u32>,
}

/// A fresh fact row: counters and clocks at zero, the ring fully unused.
/// Hand-written (not derived) because `#[ok_len(8)]` needs the ring padded from
/// the FIRST encode — a derived `Default` would hand back an empty `Vector` and
/// panic at the write boundary.
impl Default for Facts {
    fn default() -> Self {
        Facts {
            scope: 0,
            since: 0,
            confidence: 255,
            weight: 0,
            read_count: 0,
            write_count: 0,
            tool_count: 0,
            last_read: 0,
            last_write: 0,
            recent: Vector::new(vec![0; RECENT_SLOTS]),
        }
    }
}

impl Facts {
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

    /// This fact's last access, in the caller's clock unit (ms). The RING is
    /// the authority (its newest used slot); the two clocks are the fallback
    /// for payloads written before the ring existed, and the `max` keeps
    /// either source from moving time backwards.
    pub fn last_access(&self) -> u64 {
        let from_ring = self
            .recent
            .elems
            .iter()
            .rev()
            .find(|&&slot| slot != 0)
            .map(|&slot| (RECENT_BASE + u64::from(slot)) * 1000);
        let clocks = self.last_read.max(self.last_write);
        from_ring.map_or(clocks, |ms| ms.max(clocks))
    }

    /// Shift one access into the ring: oldest slot out, the new offset in.
    /// `now` is the caller's ms and the ring stores SECONDS since the epoch —
    /// second granularity is the declared price (a u32 second offset buys 136
    /// years in 4 bytes; milliseconds would buy 49 days).
    fn push_recent(&mut self, now: u64) {
        let offset = (now / 1000)
            .saturating_sub(RECENT_BASE)
            .min(u64::from(u32::MAX)) as u32;
        let mut slots = std::mem::take(&mut self.recent).elems;
        // A short ring is a payload written before the field existed: pad the
        // PAST with zeros so "oldest first" survives the normalisation.
        while slots.len() < RECENT_SLOTS {
            slots.insert(0, 0);
        }
        // A long ring can only come from a hand-built row; keep the NEWEST
        // slots rather than letting the frame outgrow the contract.
        slots.drain(..slots.len() - RECENT_SLOTS);
        slots.remove(0);
        slots.push(offset);
        self.recent = Vector::new(slots);
    }

    /// The elapsed decay from the last recorded access to `now`, in weight
    /// units. No recorded access (a caller that stamps no clock) or a
    /// clock that did not move costs nothing — decay is never invented
    /// from an absent timestamp.
    pub fn decay_at(&self, now: u64) -> f64 {
        let last = self.last_access();
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
    /// the counter bumps, the clock advances (never backwards), the access
    /// enters the recency ring, and the weight is folded with the decay the
    /// gap had accrued. The decay is measured against the PREVIOUS access and
    /// charged once here; the same fold re-bases the clock, so it is not
    /// charged twice.
    pub fn touched(&self, kind: Touch, now: u64) -> Facts {
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
        next.push_recent(now);
        next.fold_weight(elapsed_decay);
        next
    }
}

// The member set is GENERATED by `#[ok_nodes(EntityRow, MemberRow)]`: the
// derive writes `FactsNode { EntityRow, MemberRow }` and takes each ns from the
// table's own `#[ok_ns]` (36, 42) — the corpus's entities and its PEOPLE
// (ADR-0014). A compile-time fact in code, checked by okm at write AND at
// decode; refs stay wire data.

/// One fact's endpoint on the WRITE side: where the term resolved to. The
/// graph's reads see refs; the ingest path needs to tell a corpus node (which
/// gets binned into a community) from a person (who never does).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endpoint {
    /// A corpus entity: ns 36, scoped by the ingest call.
    Entity(u64),
    /// A registered member: ns 42, whose key carries its own channel.
    User { member: Uuid7 },
}

impl Endpoint {
    /// The entity node id, when this endpoint is one — the only endpoints that
    /// participate in community binning.
    fn entity_id(self) -> Option<u64> {
        match self {
            Endpoint::Entity(node_id) => Some(node_id),
            Endpoint::User { .. } => None,
        }
    }

    /// The endpoint as a graph node reference: `[ns 2B][len][pkey]`, each
    /// collection contributing its own key bytes.
    fn to_ref(self, scope: u64) -> NodeRef {
        match self {
            Endpoint::Entity(node_id) => NodeRef::new(
                <EntityRow as Document>::NS_PREFIX,
                &EntityKey { scope, node_id }.encode(),
            ),
            Endpoint::User { member } => NodeRef::new(
                <MemberRow as Document>::NS_PREFIX,
                &MemberKey { member_id: member }.encode(),
            ),
        }
    }
}

/// Parse the `user` type token's wire reference `member-{uuid}` (ADR-0051: a member is one
/// identity, so the node is named by the identity alone; a channel lives in the membership row,
/// not in the person's name). A uuid only: the syntax is a reference, never a name — a display
/// name is a row field and renaming must not move a fact.
fn parse_member_ref(value: &str) -> Option<Uuid7> {
    value.strip_prefix("member-")?.parse::<Uuid7>().ok()
}

/// A member endpoint's key bytes back to the member id — the decode side of [`MemberKey`]'s
/// `[member 16B]`. The id IS the user id (ADR-0025 §4): the identity goes into the node's pkey
/// verbatim, so a node can always be turned back into the person it names.
fn member_from_pkey(pkey: &[u8]) -> Option<Uuid7> {
    if pkey.len() != 16 {
        return None;
    }
    Some(Uuid7::from_bytes(pkey.try_into().ok()?))
}

/// An entity node id out of a ref, or `None` for any other collection — the
/// guard every reader needs before decoding a pkey as an `EntityKey`.
fn entity_node_of(node: &NodeRef) -> Option<u64> {
    if node.ns_id() != ENTITY_NS {
        return None;
    }
    Some(EntityKey::decode(&node.pkey).node_id)
}

// ---- ADR-0010 steps 3–4: binning, and the community id's issuer ------

/// The community sequence collection: ONE row per scope, holding the last
/// community id issued. It takes the ns the capsule table had (38) — the
/// table is gone, the ns is not spent.
pub const COMMUNITY_NS: u16 = 38;
/// How many propagation rounds one LPA pass runs (ADR-0010 §2: "3 rounds
/// converge in the common case"; the cap keeps a pass bounded).
pub const LPA_ROUNDS: usize = 3;
/// The neighbourhood radius the local subgraph is loaded at, in hops.
pub const LPA_RADIUS: usize = 2;

/// One end of a fact: a value, its type label, and its props (the
/// entity's attributes — they land on the node row, shared by every fact
/// about that entity, ADR-0004/ADR-0010 §1).
#[derive(Clone, Debug, PartialEq)]
pub struct Term {
    pub value: String,
    pub type_label: String,
    pub props: BTreeMap<String, Variant>,
}

/// The relation: its name IS the edge's PREDICATE (the graph's kind
/// vocabulary allocates it), so it carries no type label; its props are the
/// assertion's modifiers.
#[derive(Clone, Debug, PartialEq)]
pub struct Relation {
    pub name: String,
    pub props: BTreeMap<String, Variant>,
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
) -> Result<BTreeMap<String, Variant>, IngestError> {
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
/// caller can tell a landed fact from an adjudicated loss. The answer is the
/// fact's HEAD (`(src, predicate, dst)`), because an edge has no number
/// (okm ADR-0067): it is what the caller keeps, and what every read verb takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    /// No incumbent: the fact is a new edge.
    Linked { key: EdgeKey },
    /// The relation is functional at this source and this fact won the
    /// adjudication. A rewrite is a DELETE plus a WRITE now — two different
    /// heads are two different edges — so the key answered is the one that
    /// stands, and the incumbent's head is gone.
    Rewritten { key: EdgeKey },
    /// Functional relation, incumbent kept: nothing was written. `key` is the
    /// incumbent's head (it is gone from the graph only if the caller deletes
    /// it).
    Dropped { key: EdgeKey },
    /// The assertion already stands — same head, same kind, same tail — so the
    /// edge is the incumbent and nothing was written. Not an error and not a
    /// "drop": the fact is IN the graph, which is the answer the caller asked
    /// for. A second row for one assertion would double it in every read while
    /// adding no knowledge.
    Bumped { key: EdgeKey },
}

impl IngestOutcome {
    /// The head this outcome is about — the tuple a caller addresses it by.
    pub fn key(&self) -> &EdgeKey {
        match self {
            IngestOutcome::Linked { key }
            | IngestOutcome::Rewritten { key }
            | IngestOutcome::Dropped { key }
            | IngestOutcome::Bumped { key } => key,
        }
    }
}

/// The wire spelling of a head: the tuple's own bytes, hex.
///
/// It is an ADDRESS, not a number (okm ADR-0067): one path segment, no escaping,
/// and the same string in JSON, in a URL and in a rendered line. A caller that
/// wants to point at a fact carries this back to [`GraphStore::key_of_hex`].
pub fn key_hex(key: &EdgeKey) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let wire = key.wire();
    let mut out = String::with_capacity(wire.len() * 2);
    for byte in wire {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0F)] as char);
    }
    out
}

/// The bytes a hex address names, or `None` for a string that is not hex of an
/// even length. A parse, never a guess: the tuple's width still has to be read
/// by the graph's declaration, which is [`GraphStore::key_of_hex`]'s job.
fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            u8::try_from(hi * 16 + lo).ok()
        })
        .collect()
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
    /// The fact was refused on its own terms (an unregistered `user`
    /// reference, or a `user` endpoint with no channel keyspace attached) —
    /// a caller error, never a storage failure.
    #[error("refused: {0}")]
    Refused(String),
    /// A conditional write whose premise is gone: the caller named the revision
    /// it read, and the stored row has moved on. Nothing is written — the caller
    /// re-reads and decides, which is the only way two writers keep one row.
    #[error("revision conflict: expected {expected}, stored {stored}")]
    Conflict { expected: u32, stored: u32 },
}

/// A decoded fact, read back from storage.
#[derive(Clone, Debug, PartialEq)]
pub struct FactBody {
    /// The head this row was read at — the fact's whole identity.
    pub key: EdgeKey,
    pub src: NodeRef,
    pub dst: NodeRef,
    pub kind: String,
    pub edge: Facts,
}

/// Which collection a resolved endpoint came from (ADR-0014): a corpus entity,
/// or the identity collection's user node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeId {
    /// ns 36 — extracted knowledge, scoped to the corpus.
    Entity { scope: u64, node_id: u64 },
    /// ns 42 — a registered person; the channel is in the key because a member
    /// id is only unique inside its channel.
    User { member: Uuid7 },
}

impl NodeId {
    /// The entity node id, `None` for a user node.
    pub fn entity_id(self) -> Option<u64> {
        match self {
            NodeId::Entity { node_id, .. } => Some(node_id),
            NodeId::User { .. } => None,
        }
    }

    /// The corpus scope, `None` for a user node (identity is store-global: a
    /// channel already belongs to one scope).
    pub fn scope(self) -> Option<u64> {
        match self {
            NodeId::Entity { scope, .. } => Some(scope),
            NodeId::User { .. } => None,
        }
    }

    /// The user identity, `None` for an entity.
    pub fn user(self) -> Option<Uuid7> {
        match self {
            NodeId::User { member } => Some(member),
            NodeId::Entity { .. } => None,
        }
    }

    /// `n12` / `u3_7` — what a rendered line prints and what a diagram uses as
    /// a node id (no punctuation a mermaid id would reject).
    pub fn label(self) -> String {
        match self {
            NodeId::Entity { node_id, .. } => format!("n{node_id}"),
            NodeId::User { member } => format!("u{member}"),
        }
    }
}

/// One node as a fact read returns it: its identity, its name, its kind
/// token and (for a literal) its numeric projection — the refs carry the
/// scope, so a caller never passes one in.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedNode {
    pub id: NodeId,
    pub kind: String,
    pub literal_num: i64,
    pub name: String,
}

/// One fact with BOTH endpoints resolved — what a wire face or a
/// renderer needs (a head alone is not renderable).
#[derive(Clone, Debug, PartialEq)]
pub struct FactView {
    /// The fact's identity: its head. No number (okm ADR-0067) — a caller
    /// addresses it by [`key_hex`].
    pub key: EdgeKey,
    pub kind: String,
    pub src: ResolvedNode,
    pub dst: ResolvedNode,
    pub edge: Facts,
    /// The facts this fact POINTS AT (PLAN 4c): the reference links a writer
    /// left when the referenced fact is not central enough to sit in the head.
    /// One extra point read per shown fact — the price of the pointer being
    /// visible in the answer instead of discoverable by a second query.
    ///
    /// Each entry is a HEAD, because that is what a fact is called now: the
    /// `ref` prop carries the tuples' own bytes, not numbers that would have to
    /// be allocated, preserved and looked up.
    pub refs: Vec<EdgeKey>,
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
    /// The communities the seeds landed in, seed order, deduplicated. The
    /// community is k10r's internal slicing, not a consumer-facing unit —
    /// it rides the answer so an audit read can name the same slice.
    pub communities: Vec<u64>,
    /// The facts the answer shows: weight-descending, trimmed to the budget.
    pub facts: Vec<FactView>,
    /// The rendered memory answer, in the caller's [`RenderFormat`]: one
    /// block per community, every node and edge line carrying its id so the
    /// caller can point at a line. `None` under [`RenderFormat::Struct`] —
    /// there the answer IS the data and the caller assembles it.
    ///
    /// [`RenderFormat`]: crate::render::RenderFormat
    /// [`RenderFormat::Struct`]: crate::render::RenderFormat::Struct
    pub text: Option<String>,
    /// Facts shown vs facts the landed communities hold (the header's
    /// elision accounting).
    pub shown: usize,
    pub total: usize,
}

/// One community's slice of an answer: the facts the read side renders, the
/// community's size, and how many facts it holds in total (the budget's
/// denominator).
#[derive(Clone, Debug, PartialEq)]
pub struct CommunitySlice {
    pub community: u64,
    pub members: usize,
    pub total: usize,
    pub facts: Vec<FactView>,
}

/// The prop name a fact uses to point at ANOTHER fact (PLAN 4c): one fact's
/// address, or an array of them, in the same scope. The wire spelling of an
/// address is the head's tuple hex ([`key_hex`]), and the write stores the bytes
/// it names — an edge has no number to point at (okm ADR-0067). It is a NAME the
/// writer uses, not a schema: a dangling address is not an error, it is a 404 the
/// reader meets when it chooses to follow (`fact_in_scope`). Nothing validates it
/// on write: the reader owns the decision to descend.
pub const REF_FIELD: &str = "ref";

/// The conventional props a node carries so a reader never needs the document.
///
/// `title` is what a view draws (≤ 10 characters or 2–3 words), `content` is what
/// the node SAYS (~60 characters, two or three sentences), `source` points back
/// at the file and line it came from. They are props, not declared fields: they
/// merge into the node's dynamic segment like every other attribute, so a node
/// that has none is still a legal node, and no data is stored twice.
pub const TITLE_FIELD: &str = "title";
pub const CONTENT_FIELD: &str = "content";
pub const SOURCE_FIELD: &str = "source";

/// What a node's vector is computed over (ADR-0003 as amended 2026-10-23): its
/// label AND the text it carries.
///
/// A vector over the label alone lets a reader search a node by NAME and never
/// by what it says — which sends the reader back into the source document, the
/// one thing a fact graph exists to avoid. Only `title` and `content` join the
/// vector; every other attribute stays out, or an arbitrary prop would move the
/// node's semantic position.
fn embedding_text(value: &str, props: &BTreeMap<String, Variant>) -> String {
    let mut parts: Vec<&str> = vec![value];
    for name in [TITLE_FIELD, CONTENT_FIELD] {
        if let Some(Variant::Str(text)) = props.get(name) {
            if !text.trim().is_empty() {
                parts.push(text.as_str());
            }
        }
    }
    parts.join("\n")
}

/// The heads a `ref` value names: one head, or an array of heads. Anything else
/// (a number, a name, a nested object) is not a reference and names nothing — a
/// writer's mistake must not turn into a phantom target.
///
/// The value is the tuple's own bytes (`Variant::Bytes`), because a fact has no
/// number to store (okm ADR-0067). A payload written by the numbered build
/// carries `U64`s here; they are not heads, so they resolve to nothing rather
/// than to a maybe-address.
/// The WIRE form of a reference link: a fact's address is its tuple's own hex
/// (okm ADR-0067), so a writer states `ref` as that string — or as an array of
/// them — and the write stores the tuple's BYTES, which is what the reader
/// parses back. Anything that is not hex is left as it came: a dangling pointer
/// is the reader's 404, never a refused write.
fn wire_refs(props: &mut BTreeMap<String, Variant>) {
    let Some(value) = props.get_mut(REF_FIELD) else {
        return;
    };
    let as_bytes = |item: &Variant| match item {
        Variant::Str(text) => hex_bytes(text).map(Variant::Bytes),
        _ => None,
    };
    match value {
        Variant::Str(text) => {
            if let Some(bytes) = hex_bytes(text) {
                *value = Variant::Bytes(bytes);
            }
        }
        Variant::Array(items) => {
            for item in items.iter_mut() {
                if let Some(bytes) = as_bytes(item) {
                    *item = bytes;
                }
            }
        }
        _ => {}
    }
}

fn refs_of(value: &Variant, members: &okm_core::Members) -> Vec<EdgeKey> {
    fn head(wire: &[u8], members: &okm_core::Members) -> Option<EdgeKey> {
        let (key, taken) = EdgeKey::parse(wire, members)?;
        (taken == wire.len()).then_some(key)
    }
    let one = |item: &Variant| match item {
        Variant::Bytes(wire) => head(wire, members),
        _ => None,
    };
    match value {
        Variant::Bytes(wire) => head(wire, members).into_iter().collect(),
        Variant::Array(items) => items.iter().filter_map(one).collect(),
        _ => Vec::new(),
    }
}

/// One node's anchored memory (PLAN 4b): the facts that touch it, then its
/// community's, and which community that was (`0` = not binned yet).
#[derive(Clone, Debug, PartialEq)]
pub struct Anchored {
    pub node_id: u64,
    pub community: u64,
    pub facts: Vec<FactView>,
}

/// One hop of a causal chain: how many steps out it is, and the fact that takes
/// that step.
#[derive(Clone, Debug, PartialEq)]
pub struct ChainHop {
    pub depth: usize,
    pub fact: FactView,
}

/// The landing confidence floor (ADR-0003): below it nothing landed, and an
/// EMPTY result is the legal answer — the caller injects no memory. One place,
/// deliberately not a per-call parameter. It is a COSINE floor; the index
/// answers in squared euclidean distance, and for the unit vectors this store
/// keeps the two order identically (`dist² = 2 − 2·cos`), so the floor has one
/// translation and it lives here.
pub const LANDING_FLOOR: f32 = 0.35;

/// [`LANDING_FLOOR`] in the index's own metric.
const LANDING_DIST2: f32 = 2.0 * (1.0 - LANDING_FLOOR);

/// Byte budget for the per-scope indexes. 512-dim f32 is ~2 KB per row,
/// so this holds roughly 32k vectors across every active scope; past it the
/// least-recently-used scope is dropped and rebuilt from the KV on demand.
const TABLE_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// One scope's loaded index. The search itself is `okm_vector::ExactIndex` —
/// the vector path is okm's crate end to end (an approximate index would be the
/// same crate's business, not a decision taken here), and this side owns only
/// what is k10r's: when to build, what the answer needs besides the id, and how
/// much derived state may sit in memory.
struct ScopeTable {
    index: ExactIndex,
    /// `node_id` as the index key (BE bytes) → the name and kind LABEL the
    /// answer reports beside it (the label the row's `Vocab` resolves to; a
    /// derived table stores what it reports, never an id it would have to
    /// resolve again).
    rows: HashMap<Vec<u8>, (String, String)>,
    bytes: usize,
}

/// The per-scope indexes, with a hard byte budget and LRU eviction — the index
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

    /// Drop every cached table: a restore wrote rows underneath the cache, so
    /// what it holds is no longer what a read would load.
    fn clear(&mut self) {
        self.tables.clear();
        self.order.clear();
        self.bytes = 0;
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
    /// The vectors (ADR-0019): a side table on the SAME keyspace and the same
    /// key type, so a node and its vector commit in one batch and no join is
    /// needed to pair them.
    vectors: Collection<FjallStore, EntityKey, NodeVector>,
    /// The task sheets (ADR-0021): a side table on the SAME keyspace and the
    /// same key type, so "is this node a task" is answered by a row's presence
    /// and no join pairs a task with its state.
    sheets: Collection<FjallStore, EntityKey, TaskSheet>,
    /// The run snapshots (gravity ADR-0011 §7): an engine's serialised state,
    /// keyed by the pair it speaks in and never joined to the corpus.
    pub(crate) runs: Collection<FjallStore, crate::runs::RunKey, crate::runs::RunRow>,
    /// The identity collection (ns 42), on the CHANNEL keyspace — `None` when
    /// the caller attached only the memory side. Reads are point reads across
    /// handles; a fact's batch never spans the two keyspaces (okm's
    /// `commit_batch` writes into the handle it was called on), which is why
    /// this is a separate handle and not part of the graph's own tree.
    members: Option<Collection<FjallStore, MemberKey, MemberRow>>,
    /// The code sheets (ns 35, k10r ADR-0023): the payload behind each `code`
    /// vertex. Presence is the meaning — a node with no sheet is not code.
    pub(crate) codes: Collection<FjallStore, CodeKey, CodeRow>,
    /// One handle for the writes the collections cannot express in their
    /// own batch (the dictionary, and the edge's dynamic segment). The
    /// edge collection's own handle is built PER CALL — see `facts()`.
    store: FjallStore,
    next_node: u64,
    /// The code id watermark (k10r ADR-0023), seeded from the sheets at open.
    next_code: u64,
    /// The embedding engine (ADR-0003). `None` = no engine attached:
    /// rows land with an EMPTY vector frame and retrieval has nothing to
    /// rank (a legal, empty result — never an error). WHICH engine (the built-in
    /// model, or a service the shell talks to) is behind the trait, not here.
    embedder: Option<Arc<dyn Embedder>>,
    /// Per-scope vector indexes, built from the KV on first use (ADR-0003).
    /// The scan is okm-vector's; this map only decides when to rebuild and how
    /// much derived state may be held — see `TABLE_BUDGET_BYTES`.
    tables: ScopeTables,
    /// The interned id of k10r's community kind. Interning a label is a
    /// write-side read of the store's dictionary with the SAME answer every
    /// time, so the pass asks once per handle: `bin` and the write path would
    /// otherwise look the label up on every fact.
    community_kind: Option<Kind>,
}

impl GraphStore {
    /// Open (or create) the graph tables in the `memory` keyspace at
    /// `path` — the standalone form (tests, single-table tools).
    pub fn open(path: &std::path::Path) -> Result<Self, String> {
        let store = FjallStore::open(path, crate::KEYSPACE_MEMORY).map_err(|e| e.to_string())?;
        Self::wrap(store)
    }

    /// Attach the graph tables to an already-open store handle — the
    /// attach point `crate::service::open_service` uses.
    pub fn wrap(store: FjallStore) -> Result<Self, String> {
        // Every table attaches as it opens (okm ADR-0074): a store written by another
        // declaration is refused here, at the assembly point, not at the first read.
        let entities = <EntityRow as Document>::open(store.clone()).map_err(|e| e.to_string())?;
        let vectors = <NodeVector as Document>::open(store.clone()).map_err(|e| e.to_string())?;
        let sheets = <TaskSheet as Document>::open(store.clone()).map_err(|e| e.to_string())?;
        let runs =
            <crate::runs::RunRow as Document>::open(store.clone()).map_err(|e| e.to_string())?;
        let codes = crate::code::code_collection(store.clone())?;
        // Code ids are a watermark over the code sheets (k10r ADR-0023): the
        // highest id EVER written, so a deleted row does not free its id. One
        // scan at open; the sheet is small by construction (a row per script).
        let next_code = codes
            .scan_keys(&okm_core::PrefixProbe::empty())
            .map(|r| r.map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .map(|key| key.code_id)
            .max()
            .map_or(1, |id| id.saturating_add(1));
        // Id recovery, nodes only: entity keys are `[scope][node_id]`, so the
        // last key in table order carries the global max node id. A fresh table
        // starts at 1 — ids are unique but not contiguous per scope. There is no
        // edge watermark to recover: an edge has no number to issue (okm
        // ADR-0067), so a restore cannot collide with one either.
        let next_node = entities
            .scan_keys(&okm_core::PrefixProbe::empty())
            .map(|r| r.map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?
            .last()
            .map_or(1, |k| k.node_id.saturating_add(1));
        Ok(Self {
            entities,
            vectors,
            sheets,
            runs,
            codes,
            store,
            next_node,
            next_code,
            embedder: None,
            members: None,
            tables: ScopeTables::default(),
            community_kind: None,
        })
    }

    /// Attach the CHANNEL keyspace, which is where the identity collection
    /// lives: without it the graph cannot validate or render a `user`
    /// endpoint (ADR-0014), and an ingest that names one is refused.
    pub fn with_channel(mut self, channel_store: FjallStore) -> Result<Self, String> {
        self.members =
            Some(<MemberRow as Document>::open(channel_store).map_err(|e| e.to_string())?);
        Ok(self)
    }

    /// Open both keyspaces of one database directory — the standalone form a
    /// test needs when it exercises user endpoints (a process opens the
    /// database once and attaches keyspaces; see `crate::service`).
    pub fn open_with_channel(path: &std::path::Path) -> Result<Self, String> {
        let db = fjall::Database::create_or_recover(fjall::Config::new(path))
            .map_err(|e| e.to_string())?;
        let memory =
            FjallStore::from_db(db.clone(), crate::KEYSPACE_MEMORY).map_err(|e| e.to_string())?;
        let channel =
            FjallStore::from_db(db, crate::KEYSPACE_CHANNEL).map_err(|e| e.to_string())?;
        Self::wrap(memory)?.with_channel(channel)
    }

    /// Attach the embedding engine (ADR-0003). `open_service` cannot
    /// know about it — the engine is a serving-process concern — so the
    /// serving side attaches it here, before the store goes behind its lock.
    pub fn with_embedder(mut self, engine: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(engine);
        self
    }

    /// The edge collection handle. Predicates allocate through the graph's own
    /// PREDICATE vocabulary (the 0x04/0x05 pair, a key-side vocabulary owned by
    /// the carrier) and the edge's open prop names through the payload's FIELD
    /// dictionary (0x02/0x03) — two vocabularies, two id spaces, so the
    /// collision that used to force a fresh handle per call cannot happen. A
    /// per-call handle stays cheap (the vocabularies load on first probe, ids
    /// only grow).
    fn facts(&self) -> Graph<FjallStore, Facts> {
        Graph::new(self.store.clone())
    }

    /// The same namespace's clusters (okm ADR-0066). A cluster lives in the
    /// graph's ns and holds EDGES, so the handle is built from the same
    /// declaration as `facts()`.
    fn hyper(&self) -> Hyperedges<FjallStore> {
        Hyperedges::of::<Facts>(self.store.clone())
    }

    /// k10r's cluster kind for communities, interned on first use.
    ///
    /// okm assigns the number no meaning — the label is k10r's word, the store
    /// keeps the dictionary (ADR-0066 §1). Interning is idempotent, so every
    /// call answers the same id and a restart cannot renumber it.
    fn community_kind(&mut self) -> Result<Kind, IngestError> {
        if let Some(kind) = self.community_kind {
            return Ok(kind);
        }
        let kind = self
            .hyper()
            .kind_id(COMMUNITY_KIND)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        self.community_kind = Some(kind);
        Ok(kind)
    }

    /// The communities one node sits in, in the order its edges name them.
    ///
    /// Public because "which community is this in" is a question about the
    /// corpus, and the answer lives on the edges (okm ADR-0066) — a caller
    /// should not have to walk them itself. Empty means "in none yet".
    pub fn communities(&mut self, scope: u64, node_id: u64) -> Result<Vec<u64>, IngestError> {
        self.communities_of_node(scope, node_id)
    }

    /// The communities one entity sits in, in the order the mirror names them.
    ///
    /// A community is a VERTEX partition (okm ADR-0068): its members are the
    /// entities themselves, declared on the node face, so this is ONE prefix
    /// scan of the node mirror and costs nothing per degree. That is the whole
    /// reason the pass stores entities rather than the edges between them — a
    /// node with no edge is a legal member, and a hub pays the same as a leaf.
    fn communities_of_node(&mut self, scope: u64, node_id: u64) -> Result<Vec<u64>, IngestError> {
        let kind = self.community_kind()?;
        let node = self.node_ref(scope, node_id);
        let mut communities = Vec::new();
        for set in self
            .facts()
            .node_sets_of(&node)
            .filter(|set| set.kind == kind)
            .map(|set| set.id)
        {
            if !communities.contains(&set) {
                communities.push(set);
            }
        }
        Ok(communities)
    }

    /// The declaration's width table — how a head's bytes split into endpoints.
    fn members(&self) -> okm_core::Members {
        self.facts().member_widths().clone()
    }

    /// The head a hex address names ([`key_hex`]'s inverse), parsed by this
    /// graph's own declaration: `None` for a string that is not hex, or is not a
    /// tuple whose endpoints are the declared member tables at the declared
    /// widths. A no-match is the caller's 404, never an invented key.
    pub fn key_of_hex(&self, text: &str) -> Option<EdgeKey> {
        self.key_of_wire(&hex_bytes(text)?)
    }

    /// The head a WIRE tuple names — `EdgeKey::wire`'s inverse, split by the
    /// declaration's widths. This is the form a stored head takes: the channel
    /// checkpoint freezes the head it built a summary under, and a document
    /// field carries the tuple itself rather than a number (okm ADR-0067).
    pub fn key_of_wire(&self, bytes: &[u8]) -> Option<EdgeKey> {
        let members = self.members();
        let (key, taken) = EdgeKey::parse(bytes, &members)?;
        (taken == bytes.len()).then_some(key)
    }

    /// One node by id.
    pub fn entity(&self, scope: u64, node_id: u64) -> Result<Option<EntityRow>, IngestError> {
        self.entities
            .get_row(&EntityKey { scope, node_id })
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// The exact name/alias entrance: scan the `by_name` face and verify
    /// BOTH the row's name and the decoded key's scope (the entry is
    /// `[name][scope][node_id]`, so a prefix scan reaches every scope and
    /// every longer name that shares the prefix — the text-first regime's
    /// documented cost, paid here by the two comparisons).
    pub fn entity_by_name(
        &mut self,
        scope: u64,
        name: &str,
    ) -> Result<Option<(u64, EntityRow)>, IngestError> {
        let hits = self
            .entities
            .scan::<ByName>(name.as_bytes())
            .parts::<(parts::Key, parts::Row)>()
            .collect::<okm_core::error::Result<Vec<_>>>()
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        Ok(hits
            .into_iter()
            .filter_map(|(key, row)| row.map(|row| (key.decoded, row)))
            .find(|(key, row)| key.scope == scope && row.name == name)
            .map(|(key, row)| (key.node_id, row)))
    }

    /// The entity's open attributes: the document's names the schema does not
    /// declare. The whole document is read (ADR-0035 — a document is the pair
    /// of slots), so the row's own fields are dropped here; `None` = no props,
    /// the same answer the retired segment-only read gave.
    pub fn entity_props(
        &mut self,
        scope: u64,
        node_id: u64,
    ) -> Result<Option<BTreeMap<String, Variant>>, IngestError> {
        // No stored document (either slot) is absence; a store that cannot
        // answer is not, and it propagates.
        let Some(mut doc) = crate::document::at(&self.entities, &EntityKey { scope, node_id })
            .map_err(|e| IngestError::Storage(e.to_string()))?
        else {
            return Ok(None);
        };
        doc.retain(|name, _| !EntityRow::FIELDS.iter().any(|f| f.name == name));
        Ok((!doc.is_empty()).then_some(doc))
    }

    /// One fact, read back at its head.
    pub fn fact(&mut self, key: &EdgeKey) -> Result<Option<FactBody>, IngestError> {
        let mut facts = self.facts();
        let Some(body) = facts
            .get_edge(key)
            .map_err(|e| IngestError::Storage(e.to_string()))?
        else {
            return Ok(None);
        };
        let Some(kind) = facts
            .predicate_vocab_name(body.predicate_vocab_id)
            .map_err(|e| IngestError::Storage(e.to_string()))?
        else {
            return Ok(None);
        };
        Ok(Some(FactBody {
            key: key.clone(),
            src: body.src,
            dst: body.dst,
            kind,
            edge: Facts::decode_payload(&body.payload),
        }))
    }

    /// One fact, read back by id, with both endpoints resolved to their
    /// node rows (an absent endpoint renders as a node id and an empty
    /// name — never an error: the edge is truth even if its node row was
    /// removed).
    pub fn fact_view(&mut self, key: &EdgeKey) -> Result<Option<FactView>, IngestError> {
        let mut facts = self.facts();
        let Some(body) = facts
            .get_edge(key)
            .map_err(|e| IngestError::Storage(e.to_string()))?
        else {
            return Ok(None);
        };
        let Some(kind) = facts
            .predicate_vocab_name(body.predicate_vocab_id)
            .map_err(|e| IngestError::Storage(e.to_string()))?
        else {
            return Ok(None);
        };
        let members = self.members();
        let refs = self
            .fact_props(key)?
            .and_then(|props| props.get(REF_FIELD).map(|v| refs_of(v, &members)))
            .unwrap_or_default();
        // A fact whose endpoint row is gone is not a fact any more. Rendering it
        // would put a hollow `n42  []` in a prompt for the model to name — an
        // invitation to invent — and there is no ledger that could say where the
        // entity went, because nothing records a re-cut. The read simply does not
        // see it. Both rows were read for the view anyway, so this costs nothing.
        let (src, src_live) = self.resolve_live(&body.src)?;
        let (dst, dst_live) = self.resolve_live(&body.dst)?;
        if !src_live || !dst_live {
            return Ok(None);
        }
        Ok(Some(FactView {
            key: key.clone(),
            kind,
            src,
            dst,
            edge: Facts::decode_payload(&body.payload),
            refs,
        }))
    }

    /// This graph's tables, `(name, ns)` — the names from the declarations.
    pub fn tables(&self) -> Vec<(&'static str, u16)> {
        vec![
            (
                <EntityRow as Document>::TABLE_NAME,
                crate::export::ns(EntityRow::NS_PREFIX),
            ),
            (
                <Facts as Document>::TABLE_NAME,
                crate::export::ns(Facts::NS_PREFIX),
            ),
        ]
    }

    /// Export one table by NAME as Parquet (ADR-0007's backup boundary).
    ///
    /// Nodes are a `Document` collection and export like any other table. FACTS
    /// are the edge set as ONE table (okm ADR-0034 §3): `(table, key)` endpoints,
    /// the declaration's fields, and the edge's open attributes as a `variant`
    /// column — which is why that segment lives on the graph rather than in a
    /// consumer. `None` = not this graph's table.
    /// Land one table's rows at `ns` — the restore's per-table door (ADR-0026).
    ///
    /// `ns_of` answers a lake table's DESTINATION ns, because an edge's endpoints
    /// are `[ns 2B]` refs: a mapped restore that moved the node table and left the
    /// edges pointing at the old ns would restore edges to nothing.
    pub fn restore(
        &mut self,
        table: &str,
        ns: u16,
        batches: &[arrow::array::RecordBatch],
        ns_of: &dyn Fn(&str) -> Option<u16>,
    ) -> Option<Result<usize, String>> {
        let out = if table == <EntityRow as Document>::TABLE_NAME {
            crate::export::land_into(&mut self.entities.rebased(ns), batches)
        } else if table == <Facts as Document>::TABLE_NAME {
            let mut graph = self.facts().rebased(ns);
            let mut rows = 0usize;
            for batch in batches {
                match graph.import_table(batch, ns_of) {
                    Ok(n) => rows += n,
                    Err(e) => return Some(Err(e)),
                }
            }
            Ok(rows)
        } else if table == <crate::code::CodeRow as Document>::TABLE_NAME
            || table == crate::export::CODE_REDUCES
        {
            return self.restore_code(table, ns, batches);
        } else {
            return None;
        };
        if out.is_ok() {
            if let Err(e) = self.reseed() {
                return Some(Err(e));
            }
        }
        Some(out)
    }

    /// Whether a table's namespace is untouched (the restore's refusal).
    pub fn ns_is_empty(&self, table: &str, ns: u16) -> Option<Result<bool, String>> {
        if table == <EntityRow as Document>::TABLE_NAME {
            Some(crate::export::ns_is_empty_into(&self.entities.rebased(ns)))
        } else if table == <Facts as Document>::TABLE_NAME {
            // The edges share the node table's ns, so "no edge" is asked of the
            // graph itself: a restore that found edges there would fold two
            // corpora into one number space.
            Some(
                self.facts()
                    .rebased(ns)
                    .has_edges()
                    .map(|edges| !edges)
                    .map_err(|e| e.to_string()),
            )
        } else if table == <crate::code::CodeRow as Document>::TABLE_NAME
            || table == crate::export::CODE_REDUCES
        {
            self.ns_is_empty_code(table, ns)
        } else {
            None
        }
    }

    pub fn export(&mut self, name: &str) -> Option<Result<crate::export::Export, String>> {
        if name == <EntityRow as Document>::TABLE_NAME {
            Some(crate::export::table_bytes(&mut self.entities))
        } else if let Some((src, dst)) = crate::export::edge_pair_of(name) {
            // One edge table per endpoint pair (ADR-0027): the pair's rows, with
            // the endpoints decoded into that vertex table's key columns, so a
            // property-graph declaration can reference them.
            let mut facts = self.facts();
            let batch = match facts.to_record_batch_table(crate::export::name_of) {
                Ok(batch) => batch,
                Err(e) => return Some(Err(format!("the facts table: {e}"))),
            };
            match crate::export::edge_batch(&batch, src, dst) {
                Ok(pair) => Some(crate::export::batch_bytes(&pair)),
                Err(e) => Some(Err(format!("{name}: {e}"))),
            }
        } else if name == <Facts as Document>::TABLE_NAME {
            let mut facts = self.facts();
            let batch = match facts.to_record_batch_table(crate::export::name_of) {
                Ok(batch) => batch,
                Err(e) => return Some(Err(format!("the facts table: {e}"))),
            };
            // An EDGE batch, not a document table: its identity is the triple the edge set
            // is keyed by, which this layer does not restate.
            Some(crate::export::batch_bytes(&batch))
        } else if name == <crate::code::CodeRow as Document>::TABLE_NAME {
            // The code sheets (k10r ADR-0023): a table of this graph's family,
            // so the lake carries it with the rest of the corpus.
            self.export_code(name)
        } else {
            None
        }
    }

    /// Recompute every in-process issuer from the store — the open path, reused
    /// after a restore wrote rows underneath. A restore that skips this hands
    /// out ids the restored rows already carry. Edges have no issuer to seed:
    /// their identity is the head, which no counter can collide with.
    pub fn reseed(&mut self) -> Result<(), String> {
        let next_node = self
            .entities
            .scan_keys(&okm_core::PrefixProbe::empty())
            .map(|r| r.map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?
            .last()
            .map_or(1, |k| k.node_id.saturating_add(1));
        self.next_node = next_node;
        self.tables.clear();
        // The dictionary is the store's: a restored corpus can carry a
        // different numbering, so the cached id goes with the other caches.
        self.community_kind = None;
        Ok(())
    }

    /// One fact by ITS HEAD — the deterministic read (ADR-0004's identity), the
    /// mirror of `ingest`'s answer: no search, no embedding, no ranking. The
    /// scope is checked against the edge's own source endpoint, so a head from
    /// another scope answers `None` rather than leaking.
    ///
    /// This is what a reference LINK resolves to: a fact can name a less central
    /// fact by head in its props, and the reader decides whether to follow it —
    /// the navigation shape skills use, one level down.
    pub fn fact_in_scope(
        &mut self,
        scope: u64,
        key: &EdgeKey,
    ) -> Result<Option<FactView>, IngestError> {
        let Some(body) = self
            .facts()
            .get_edge(key)
            .map_err(|e| IngestError::Storage(e.to_string()))?
        else {
            return Ok(None);
        };
        if EntityKey::decode(&body.src.pkey).scope != scope {
            return Ok(None);
        }
        self.fact_view(key)
    }

    /// Withdraw one fact: its row and every derived face entry in ONE batch
    /// (okm's `retract` — the same path a functional rewrite takes for the
    /// incumbent). `false` = no such edge in this scope, so the caller can
    /// answer 404 without guessing.
    ///
    /// The head names the edge, and a delete is the ONLY way to take it away:
    /// nothing can be "rewritten in place" into a different tuple (okm
    /// ADR-0067), so a replacement is this verb plus a write.
    pub fn withdraw(&mut self, scope: u64, key: &EdgeKey) -> Result<bool, IngestError> {
        let facts = self.facts();
        let Some(body) = facts
            .get_edge(key)
            .map_err(|e| IngestError::Storage(e.to_string()))?
        else {
            return Ok(false);
        };
        if EntityKey::decode(&body.src.pkey).scope != scope {
            return Ok(false);
        }
        // The delete verb takes a head, and the head is what the caller
        // already has — `purge_head` resolves the predicate name for it.
        self.purge_head(key)?;
        Ok(true)
    }

    /// Both directions out of one node — the symmetric neighbourhood. Cheap by
    /// construction: each side is a ref list on the node rows (okm's `out_edges`
    /// and `in_edges`), so the cost follows the NODE's degree, never the scope's
    /// size. This is the entity-anchored entrance (PLAN 4b): a caller holding an
    /// entity id gets its facts without a query, an embedding, or a landing.
    pub fn neighbours(&mut self, scope: u64, node_id: u64) -> Result<Vec<FactView>, IngestError> {
        let node = self.node_ref(scope, node_id);
        let mut keys = self
            .facts()
            .out_edges(&node)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        keys.extend(
            self.facts()
                .in_edges(&node)
                .map_err(|e| IngestError::Storage(e.to_string()))?,
        );
        self.views_of(keys)
    }

    /// One node's anchored memory (PLAN 4b): the facts that touch it, heaviest
    /// first, then the rest of its community — the same slice a recalled landing
    /// renders, entered by IDENTITY instead of by similarity. A node with no
    /// community yet (a fresh row) answers its own neighbourhood and nothing
    /// else. A node with no edges, or one whose facts are not in a cluster yet,
    /// answers its own neighbourhood and nothing more.
    pub fn anchored(&mut self, scope: u64, node_id: u64) -> Result<Option<Anchored>, IngestError> {
        // Presence is the answer's precondition: an unknown node anchors nothing.
        if self.entity(scope, node_id)?.is_none() {
            return Ok(None);
        }
        let own = self.neighbours(scope, node_id)?;
        let mut seen: HashSet<EdgeKey> = own.iter().map(|f| f.key.clone()).collect();
        let mut facts = own;
        // The node's community is the one its facts belong to (okm ADR-0066);
        // a node with no edges, or one not yet binned, answers its own
        // neighbourhood and nothing else.
        let community = self
            .communities_of_node(scope, node_id)?
            .first()
            .copied()
            .unwrap_or(0);
        if community != 0 {
            let mut rest: Vec<FactView> = self
                .community_facts(scope, community)?
                .into_iter()
                .filter(|view| seen.insert(view.key.clone()))
                .collect();
            rest.sort_by(|a, b| {
                b.edge
                    .weight
                    .cmp(&a.edge.weight)
                    .then_with(|| a.key.cmp(&b.key))
            });
            facts.extend(rest);
        }
        Ok(Some(Anchored {
            node_id,
            community,
            facts,
        }))
    }

    /// The keys of one declared face under a probe that stops SHORT of the
    /// declared fields, in the face's own order.
    ///
    /// okm's `by_attr_face` is an EQUALITY filter, not a range scan: it probes
    /// `[ns][slot][encoded]` and keeps an entry only when the bytes left over
    /// parse as a whole edge identity. That is the correct answer for a probe at
    /// the face's full declared width, and the wrong one the moment a caller
    /// omits a field — the declared fields then sit in front of the tail and
    /// every entry is dropped (silently, because a face with no match is also
    /// the honest answer to "nothing here").
    ///
    /// So a partial-prefix read is done here, over okm's public pieces: the
    /// face's own prefix (`KvIndex::entry_prefix`, which is `[ns][slot][probe]`
    /// — the declaration states its slot, never a hand-built number), the
    /// suffix range under it, and the split of `[fields][head]` at the field
    /// segment's known width. What parses as a head is kept; an entry with
    /// bytes left over belongs to another layout and is skipped, exactly as
    /// `by_attr_face` skips it.
    ///
    /// `fields_under_probe` is the width of the declared segment the probe did
    /// NOT consume, in bytes — a fact of the face's declaration, which is why
    /// its two values live beside the face names above.
    fn face_keys<I: KvIndex>(
        &self,
        probe: &[u8],
        fields_under_probe: usize,
    ) -> Result<Vec<EdgeKey>, IngestError> {
        let facts = self.facts();
        let prefix = <I as KvIndex>::entry_prefix(&facts.ns().to_be_bytes(), probe);
        let members = facts.member_widths();
        let mut keys = Vec::new();
        for (mut suffix, _) in scan_suffix_kv(facts.store(), &prefix)
            .map_err(|e| IngestError::Storage(e.to_string()))?
        {
            if suffix.len() < fields_under_probe {
                continue;
            }
            suffix.drain(..fields_under_probe);
            if let Some((key, taken)) = EdgeKey::parse(&suffix, members) {
                if taken == suffix.len() {
                    keys.push(key);
                }
            }
        }
        Ok(keys)
    }

    /// The scope's facts in the order they were LEARNED, newest first (PLAN 4b's
    /// temporal entrance).
    ///
    /// It walks the `by_since` face instead of the rows: the whole face (an
    /// empty probe, its `since` segment split off), reverse order, and only the
    /// rows it keeps are decoded and resolved. The face's tail IS the edge's
    /// head (okm ADR-0067), so the walk's order is the answer's order, newest
    /// first, ties by head descending.
    ///
    /// The face is scope-blind, so the walk skips the keys of other scopes; that
    /// costs a row read per key until `limit` of this scope are found. A
    /// scope-prefixed time face (`by_scope_since`) would remove that, and it is
    /// the same declaration shape as `by_scope_weight` — worth it only when a
    /// scope carries enough facts for the skip to matter.
    pub fn timeline(&mut self, scope: u64, limit: usize) -> Result<Vec<FactView>, IngestError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // An empty probe stops in front of `since`, so the read is the face's
        // whole range with that one declared field split off (`face_keys`) —
        // not `by_attr_face`, which would demand a `since` value and is an
        // equality filter besides.
        let keys: Vec<EdgeKey> = self.face_keys::<BySince>(&[], BY_SINCE_FIELDS)?;
        let mut out = Vec::new();
        for key in keys.into_iter().rev() {
            let Some(fact) = self.fact_view(&key)? else {
                continue;
            };
            if fact.edge.scope != scope {
                continue;
            }
            out.push(fact);
            if out.len() == limit {
                break;
            }
        }
        Ok(out)
    }

    /// A causal chain (PLAN 4b): follow ONE predicate from `from`, hop by hop,
    /// breadth-first, cycle-safe by node id and bounded by `max_hops`. No search
    /// and no scoring — the order of the answer is the order of the walk, which is
    /// what makes "A leads to B leads to C" readable as a sequence rather than a
    /// pile of facts.
    pub fn chain(
        &mut self,
        scope: u64,
        from: u64,
        kind: &str,
        max_hops: usize,
    ) -> Result<Vec<ChainHop>, IngestError> {
        let mut out: Vec<ChainHop> = Vec::new();
        let mut visited: HashSet<u64> = HashSet::new();
        visited.insert(from);
        let mut frontier = vec![from];
        for depth in 0..max_hops {
            let mut next: Vec<u64> = Vec::new();
            for node_id in frontier {
                let keys = self.typed_out(scope, node_id, kind)?;
                for key in keys {
                    let Some(view) = self.fact_view(&key)? else {
                        continue;
                    };
                    out.push(ChainHop {
                        depth,
                        fact: view.clone(),
                    });
                    // Only an entity target continues the walk: a user node is
                    // where identity lives, not another hop of the chain.
                    if let Some(target) = view.dst.id.entity_id() {
                        if visited.insert(target) {
                            next.push(target);
                        }
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(out)
    }

    /// Deduplicate heads and order them weight-descending, ties by head — the
    /// ordering every read that shows facts uses (ADR-0005).
    fn views_of(&mut self, keys: Vec<EdgeKey>) -> Result<Vec<FactView>, IngestError> {
        let mut seen = HashSet::new();
        let mut facts: Vec<FactView> = Vec::with_capacity(keys.len());
        for key in keys {
            if !seen.insert(key.clone()) {
                continue;
            }
            // A key that names no edge is absence (the face outlived its row);
            // a store that cannot answer is not, and it propagates.
            if let Some(fact) = self.fact_view(&key)? {
                facts.push(fact);
            }
        }
        facts.sort_by(|a, b| {
            b.edge
                .weight
                .cmp(&a.edge.weight)
                .then_with(|| a.key.cmp(&b.key))
        });
        Ok(facts)
    }

    /// The heaviest facts of a scope, weight-descending, ties by edge id — the
    /// progressive face (PLAN 4a).
    ///
    /// One probe of the `by_scope_weight` face gives this scope's keys in weight
    /// order (`[scope][weight][edge_id]`), the walk stops at `k` keys, and only
    /// those k rows are decoded — so the read is O(k rows) + O(scope keys), not
    /// O(scope rows). The keys belong to one scope by construction, which is why
    /// the scope is a denormalized field on the row: a face cannot be probed by
    /// something the index does not carry.
    pub fn top_by_weight(&mut self, scope: u64, k: usize) -> Result<Vec<FactView>, IngestError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let keys: Vec<EdgeKey> =
            self.face_keys::<ByScopeWeight>(&scope.to_be_bytes(), BY_WEIGHT_FIELD)?;
        let mut facts: Vec<FactView> = Vec::with_capacity(k);
        // The face orders by (scope, weight, head) ascending; reversed that is
        // weight-descending with the tie on the head DESCENDING. The documented
        // order keeps ties ascending, so the k rows are put back in it below — k
        // rows, never the scope.
        for key in keys.into_iter().rev().take(k) {
            // A face entry whose row is gone is absence, never a failure.
            if let Some(fact) = self.fact_view(&key)? {
                facts.push(fact);
            }
        }
        facts.sort_by(|a, b| {
            b.edge
                .weight
                .cmp(&a.edge.weight)
                .then_with(|| a.key.cmp(&b.key))
        });
        Ok(facts)
    }

    /// One node's outgoing facts, endpoints resolved — the one-hop
    /// traversal the read face serves. `out_edges` is the traversal
    /// currency (untyped, no dictionary involved); each id is then read
    /// back as a whole fact.
    pub fn out_facts(&mut self, scope: u64, node_id: u64) -> Result<Vec<FactView>, IngestError> {
        let keys = self.out_edges(scope, node_id)?;
        let mut facts = Vec::with_capacity(keys.len());
        for key in keys {
            // A key that names no edge is absence, never a failure.
            if let Some(fact) = self.fact_view(&key)? {
                facts.push(fact);
            }
        }
        Ok(facts)
    }

    /// One node's vector, or absence (ADR-0019): the side table's presence is
    /// the boolean, so a node written without an engine answers `None` rather
    /// than an empty frame.
    pub fn node_vector(
        &self,
        scope: u64,
        node_id: u64,
    ) -> Result<Option<Vector<f32>>, IngestError> {
        let key = EntityKey { scope, node_id };
        Ok(self
            .vectors
            .get_row(&key)
            .map_err(|e| IngestError::Storage(e.to_string()))?
            .map(|row| row.embedding)
            .filter(|embedding| !embedding.is_empty()))
    }

    /// Every node of one scope, key order — the whole-scope read a bulk
    /// consumer (the visualization door) serves. The scan is bounded by
    /// the primary-key prefix `[scope]`, so no other scope's rows are
    /// touched.
    pub fn scope_entities(&mut self, scope: u64) -> Result<Vec<(u64, EntityRow)>, IngestError> {
        let probe = EntityKey::prefix().scope(scope).finish();
        let mut out = Vec::new();
        for key in self.entities.scan_keys(&probe) {
            let key = key.map_err(|e| IngestError::Storage(e.to_string()))?;
            // A node that vanished between the scan and the read is absence; a
            // store that cannot answer is not, and it propagates.
            if let Some(row) = self
                .entities
                .get_row(&key)
                .map_err(|e| IngestError::Storage(e.to_string()))?
            {
                out.push((key.node_id, row));
            }
        }
        Ok(out)
    }

    /// Every fact of one scope: each node's outgoing facts, node order
    /// then edge order. Each edge is reached exactly once — `ingest`
    /// creates both endpoints in the scope it was called with, so an
    /// edge's source is always in this list.
    pub fn scope_facts(&mut self, scope: u64) -> Result<Vec<FactView>, IngestError> {
        let nodes: Vec<u64> = self
            .scope_entities(scope)?
            .into_iter()
            .map(|(node_id, _)| node_id)
            .collect();
        let mut facts = Vec::new();
        for node_id in nodes {
            facts.extend(self.out_facts(scope, node_id)?);
        }
        Ok(facts)
    }

    /// Vector landing (ADR-0003): embed the query, ask the scope's index for
    /// its nearest rows, drop everything under [`LANDING_FLOOR`], return the
    /// top `k`. The scan is okm-vector's exact one (squared euclidean, which
    /// orders like cosine for unit vectors — see [`LANDING_FLOOR`]); the index
    /// is DERIVED state — built from the KV on first use and dropped on every
    /// write — so it never drifts from the rows.
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
        self.load_vector_table(scope)?;
        let Some(table) = self.tables.get(scope) else {
            return Ok(Vec::new());
        };
        // An empty index has no dimension to compare against, and a scope whose
        // rows were written by a different engine cannot be ranked by this
        // query — either way nothing lands.
        if table.index.is_empty() || table.index.dim() != q.len() {
            return Ok(Vec::new());
        }
        Ok(table
            .index
            .search(&q, table.index.live())
            .into_iter()
            .filter(|(_, dist2)| *dist2 <= LANDING_DIST2)
            .take(k)
            .filter_map(|(key, dist2)| {
                let (name, kind) = table.rows.get(&key)?;
                Some(Seed {
                    node_id: u64::from_be_bytes(key.as_slice().try_into().ok()?),
                    score: 1.0 - dist2 / 2.0,
                    name: name.clone(),
                    kind: kind.clone(),
                })
            })
            .collect())
    }

    /// Landing + the landed COMMUNITY (ADR-0010 §5): the query lands, the
    /// seeds' communities are read as buckets (the cluster over their edges
    /// the facts whose endpoints are both members), and the answer is those
    /// facts rendered as id-bearing text. There is no traversal policy and
    /// no N-hop: a community is a bounded slice by construction, and
    /// `budget` (a MAX FACT COUNT, 0 = no limit) keeps the answer at a few
    /// hundred tokens — dropping the lowest-weight facts, never a half line.
    ///
    /// `format` says how the answer is represented (ADR-0010 §5): the three
    /// text forms render the same content in different syntax, and
    /// [`RenderFormat::Struct`] renders nothing — the caller assembles the
    /// data itself.
    ///
    /// `ts_ms` is the CALLER's clock (ADR-0008 §3). When it rides the call,
    /// every fact the answer SHOWS counts as one read and its weight is
    /// re-folded (ADR-0005); the shown set is re-ordered by the new weight.
    /// Without a clock the answer is identical except that nothing is
    /// counted — a read nobody stamped is not a read.
    pub fn recall(
        &mut self,
        scope: u64,
        query: &str,
        k: usize,
        ts_ms: Option<u64>,
        budget: usize,
        format: RenderFormat,
    ) -> Result<Recall, IngestError> {
        let seeds = self.retrieve(scope, query, k)?;
        let mut communities: Vec<u64> = Vec::new();
        for seed in &seeds {
            // A seed's communities are the clusters of the edges that touch it
            // (okm ADR-0066): the node row carries no bin, because membership is
            // a property of FACTS, and one node can sit in more than one.
            for community in self.communities_of_node(scope, seed.node_id)? {
                if !communities.contains(&community) {
                    communities.push(community);
                }
            }
        }

        // One bucket per community, weight-descending inside each, every edge
        // once (the same edge is reachable from both endpoints).
        let mut seen = HashSet::new();
        let mut slices: Vec<CommunitySlice> = Vec::new();
        for &community in &communities {
            // The denominator is the number of facts INSIDE the set — the same
            // unit as the hits below. A node set does not store those edges, so
            // this is the induced subgraph's length rather than a stored count:
            // the read that produces the hits is the read that knows the total.
            let mut facts: Vec<FactView> = self
                .community_facts(scope, community)?
                .into_iter()
                .filter(|view| seen.insert(view.key.clone()))
                .collect();
            let members = facts.len();
            facts.sort_by(|a, b| {
                b.edge
                    .weight
                    .cmp(&a.edge.weight)
                    .then_with(|| a.key.cmp(&b.key))
            });
            slices.push(CommunitySlice {
                community,
                members,
                total: facts.len(),
                facts,
            });
        }
        let total: usize = slices.iter().map(|s| s.total).sum();

        // The budget is spent in seed order: the first landed community is
        // the one the query is most about.
        let mut room = budget;
        for slice in &mut slices {
            if budget != 0 && slice.facts.len() > room {
                slice.facts.truncate(room);
            }
            room = room.saturating_sub(slice.facts.len());
        }

        // Count what is actually shown, then order by the folded weight.
        if let Some(now) = ts_ms {
            for slice in &mut slices {
                for view in &mut slice.facts {
                    // A counter that cannot be written is not a reason to fail
                    // a read: the count is a by-product of serving, not part of
                    // the answer (ADR-0018). The view below is still the one
                    // built from the row as it was.
                    if let Ok(Some(served)) = self.bump_view(&view.key, Touch::Read, now) {
                        *view = served;
                    }
                }
                slice.facts.sort_by(|a, b| {
                    b.edge
                        .weight
                        .cmp(&a.edge.weight)
                        .then_with(|| a.key.cmp(&b.key))
                });
            }
        }

        let shown: usize = slices.iter().map(|s| s.facts.len()).sum();
        let facts: Vec<FactView> = slices
            .iter()
            .flat_map(|slice| slice.facts.iter().cloned())
            .collect();
        let text = format
            .renders_text()
            .then(|| crate::render::render_memory(&slices, format));
        Ok(Recall {
            seeds,
            communities,
            facts,
            text,
            shown,
            total,
        })
    }

    /// A caller's own expansion primitive: every fact one hop out of the
    /// given seeds, both directions, deduplicated by edge id, ordered by
    /// weight (ADR-0005). [`recall`](Self::recall) does NOT use it — the read
    /// surface is the landed community — but a caller that landed by another
    /// entrance (the exact name face) or that composes a deeper walk starts
    /// here; `ts_ms` behaves exactly as in `recall`.
    pub fn expand(
        &mut self,
        scope: u64,
        seeds: &[Seed],
        ts_ms: Option<u64>,
    ) -> Result<Vec<FactView>, IngestError> {
        let mut seen = HashSet::new();
        let mut facts = Vec::new();
        for seed in seeds {
            let mut keys = self.out_edges(scope, seed.node_id)?;
            keys.extend(self.in_edges(scope, seed.node_id)?);
            for key in keys {
                if !seen.insert(key.clone()) {
                    continue;
                }
                // Same rule as the community path: counting is a by-product of
                // serving, so a failed count answers the fact as it stands
                // rather than failing the read (ADR-0018).
                let view = match ts_ms {
                    Some(now) => match self.bump_view(&key, Touch::Read, now) {
                        Ok(served) => served,
                        Err(_) => self.fact_view(&key)?,
                    },
                    None => self.fact_view(&key)?,
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
        key: &EdgeKey,
        kind: Touch,
        now: u64,
    ) -> Result<Option<FactView>, IngestError> {
        let Some(body) = self
            .facts()
            .get_edge(key)
            .map_err(|e| IngestError::Storage(e.to_string()))?
        else {
            return Ok(None);
        };
        if EntityKey::decode(&body.src.pkey).scope != scope {
            return Ok(None);
        }
        self.bump_view(key, kind, now)
    }

    /// The shared write half of a touch: decode the edge, bump the counter,
    /// re-fold the weight, and land the row again at the SAME head. The declared
    /// faces are derived from the row, so `by_scope_weight` follows — which is
    /// why this is a re-assertion of the tuple (`Decl::edge`, one reduce): okm
    /// retires the entries the old payload wrote and puts the new ones in the
    /// same batch, so the old weight never survives beside the new.
    ///
    /// Same head means same edge (okm ADR-0067), so a bump is the one update
    /// that stays in place — and the open attributes (0x01) are not touched by
    /// a re-assertion.
    fn bump_view(
        &mut self,
        key: &EdgeKey,
        kind: Touch,
        now: u64,
    ) -> Result<Option<FactView>, IngestError> {
        let mut facts = self.facts();
        let Some(body) = facts
            .get_edge(key)
            .map_err(|e| IngestError::Storage(e.to_string()))?
        else {
            return Ok(None);
        };
        let Some(predicate) = facts
            .predicate_vocab_name(body.predicate_vocab_id)
            .map_err(|e| IngestError::Storage(e.to_string()))?
        else {
            return Ok(None);
        };
        let edge = Facts::decode_payload(&body.payload).touched(kind, now);
        let head = EdgeHead {
            src: body.src.clone(),
            dst: body.dst.clone(),
            predicate: predicate.clone(),
        };
        // Declaring the SAME head is the update: the row stands and the
        // conflict hook answers the whole payload change, so the edge is never
        // absent. The bumped row was read a few lines up, and both sides the
        // hook is handed are older than that read, so its answer is the row
        // itself.
        let next = edge.clone();
        let mut fragment = Fragment::<Facts>::new([Decl::edge(head)]);
        fragment.on_edge_conflict(move |_stored, _declared| next.clone());
        facts
            .incorporate(fragment)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        // The dynamic segment is untouched by a re-assertion, so the reference
        // links a writer left are still the ones this row carries.
        let members = self.members();
        let refs = self
            .fact_props(key)?
            .and_then(|props| props.get(REF_FIELD).map(|v| refs_of(v, &members)))
            .unwrap_or_default();
        Ok(Some(FactView {
            key: key.clone(),
            kind: predicate,
            src: self.resolve_ref(&body.src)?,
            dst: self.resolve_ref(&body.dst)?,
            edge,
            refs,
        }))
    }

    /// One scope's vectors, read from their own table (ADR-0019): presence is
    /// the boolean, so a node without a row is a node without a vector — the
    /// `Option`/sentinel the row used to carry is carried by the table now.
    fn scope_vectors(&self, scope: u64) -> Result<HashMap<u64, Vector<f32>>, IngestError> {
        let probe = EntityKey::prefix().scope(scope).finish();
        let mut out = HashMap::new();
        for key in self.vectors.scan_keys(&probe) {
            let key = key.map_err(|e| IngestError::Storage(e.to_string()))?;
            match self
                .vectors
                .get_row(&key)
                .map_err(|e| IngestError::Storage(e.to_string()))?
            {
                Some(row) if !row.embedding.is_empty() => {
                    out.insert(key.node_id, row.embedding);
                }
                // A row that vanished between the scan and the read is absence;
                // an EMPTY one is a node written without an engine (v4 stores
                // that as no row at all, so this is only older data).
                _ => {}
            }
        }
        Ok(out)
    }

    /// Build (or refresh the LRU position of) one scope's table, from the
    /// vectors' own table (ADR-0019). Nodes without a vector row are skipped by
    /// construction — absence is the boolean.
    fn load_vector_table(&mut self, scope: u64) -> Result<(), IngestError> {
        if self.tables.contains(scope) {
            self.tables.touch(scope);
            return Ok(());
        }
        let rows = self.scope_entities(scope)?;
        let vectors = self.scope_vectors(scope)?;
        // One dimension per scope: the first vector sets it (a row of another
        // width cannot be ranked against this query, and a node without a
        // vector cannot be ranked at all).
        let dim = vectors.values().next().map_or(0, Vector::len);
        let mut index = ExactIndex::new(dim);
        let mut named = HashMap::new();
        let mut bytes = 0usize;
        for (node_id, row) in rows {
            let Some(embedding) = vectors.get(&node_id) else {
                continue;
            };
            if embedding.len() != dim {
                continue;
            }
            let name_len = row.name.len();
            let key = node_id.to_be_bytes().to_vec();
            index.add(key.clone(), &embedding.elems);
            named.insert(
                key,
                (row.name, row.kind.as_str().unwrap_or_default().to_string()),
            );
            bytes += dim * std::mem::size_of::<f32>() + name_len;
        }
        self.tables.insert(
            scope,
            ScopeTable {
                index,
                rows: named,
                bytes,
            },
        );
        Ok(())
    }

    /// One node as a graph endpoint, plus whether the table it names still holds
    /// it. `live == false` means a stale id: the graph kept the edge, the entity
    /// row is gone — the state a crash between a merge's two batches leaves, and
    /// the one a restore that moved a table can leave behind it.
    ///
    /// A MEMBER ref is always live: a person's identity is the uuid, so a missing
    /// display name is a missing name, not a missing person.
    fn resolve_live(&mut self, node: &NodeRef) -> Result<(ResolvedNode, bool), IngestError> {
        if node.ns_id() == MEMBER_NS {
            let member = member_from_pkey(&node.pkey).unwrap_or(Uuid7::from_bytes([0u8; 16]));
            // An unregistered member is absence (the ref still names the
            // person); a store that cannot answer is not, and it propagates.
            let name = match self.members.as_ref() {
                Some(members) => members
                    .get_row(&MemberKey { member_id: member })
                    .map_err(|e| IngestError::Storage(e.to_string()))?
                    .map(|row| row.display_name),
                None => None,
            }
            .unwrap_or_default();
            return Ok((
                ResolvedNode {
                    id: NodeId::User { member },
                    kind: USER_KIND.to_string(),
                    literal_num: 0,
                    name,
                },
                true,
            ));
        }
        let key = EntityKey::decode(&node.pkey);
        let row = self.entity(key.scope, key.node_id)?;
        let live = row.is_some();
        let (kind, literal_num, name) = match row {
            Some(row) => (
                row.kind.as_str().unwrap_or_default().to_string(),
                row.literal_num,
                row.name,
            ),
            None => (String::new(), 0, String::new()),
        };
        Ok((
            ResolvedNode {
                id: NodeId::Entity {
                    scope: key.scope,
                    node_id: key.node_id,
                },
                kind,
                literal_num,
                name,
            },
            live,
        ))
    }

    /// One node as a graph endpoint. A stale id still answers a ref — the caller
    /// that must know whether the entity is still there asks [`resolve_live`].
    pub fn resolve_ref(&mut self, node: &NodeRef) -> Result<ResolvedNode, IngestError> {
        Ok(self.resolve_live(node)?.0)
    }

    /// Outgoing facts of one node as heads (the traversal currency).
    pub fn out_edges(&mut self, scope: u64, node_id: u64) -> Result<Vec<EdgeKey>, IngestError> {
        self.facts()
            .out_edges(&self.node_ref(scope, node_id))
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// Outgoing facts of ANY node reference, as heads — the form a user node
    /// needs: identity is not scoped to the corpus (ADR-0014).
    pub fn out_edges_of(&mut self, node: &NodeRef) -> Result<Vec<EdgeKey>, IngestError> {
        self.facts()
            .out_edges(node)
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// Incoming facts of any node reference, as heads; see
    /// [`out_edges_of`](Self::out_edges_of).
    pub fn in_edges_of(&mut self, node: &NodeRef) -> Result<Vec<EdgeKey>, IngestError> {
        self.facts()
            .in_edges(node)
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// Facts of one predicate out of any node reference, as heads (the 0x08
    /// face).
    pub fn typed_out_of(
        &mut self,
        node: &NodeRef,
        kind: &str,
    ) -> Result<Vec<EdgeKey>, IngestError> {
        self.facts()
            .typed_out(kind, node)
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// The ENTITY ids one predicate points at out of a scoped node, in edge
    /// order — the read a sheet needs when the meaning of a relation is "this
    /// row is about that record" (a task bound to the code that finishes it,
    /// k10r ADR-0021 + gravity ADR-0003). Callers that need the whole fact use
    /// [`typed_out_of`](Self::typed_out_of) and [`fact`](Self::fact).
    ///
    /// Non-entity endpoints are skipped rather than guessed at: a user node
    /// (identity ns) has no entity id, and a task bound to a person is not a
    /// binding this reader can honour.
    pub fn targets_of(
        &mut self,
        scope: u64,
        node_id: u64,
        kind: &str,
    ) -> Result<Vec<u64>, IngestError> {
        let node = self.node_ref(scope, node_id);
        let mut out = Vec::new();
        for key in self.typed_out_of(&node, kind)? {
            if let Some(fact) = self.fact(&key)? {
                if let Some(id) = entity_node_of(&fact.dst) {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    /// One member's node reference (`[42][member]`) — the entry a context head starts its
    /// traversal from.
    pub fn user_ref(&self, member: Uuid7) -> NodeRef {
        NodeRef::new(
            <MemberRow as Document>::NS_PREFIX,
            &MemberKey { member_id: member }.encode(),
        )
    }

    /// Incoming facts of one node, as heads — the in-direction mirror of
    /// [`out_edges`](Self::out_edges). Both directions matter to a landing:
    /// a seed is useful through the facts pointing AT it as well.
    pub fn in_edges(&mut self, scope: u64, node_id: u64) -> Result<Vec<EdgeKey>, IngestError> {
        self.facts()
            .in_edges(&self.node_ref(scope, node_id))
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// Facts of one predicate out of one node, as heads — the 0x08 face, and
    /// the resolution path's incumbent scan.
    pub fn typed_out(
        &mut self,
        scope: u64,
        node_id: u64,
        kind: &str,
    ) -> Result<Vec<EdgeKey>, IngestError> {
        self.facts()
            .typed_out(kind, &self.node_ref(scope, node_id))
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// Declare one node directly — the write face for a node that is not an
    /// endpoint of a fact (a skill, ADR-0026).
    ///
    /// Resolution is the rule `ingest` already follows: a name that exists in
    /// the scope IS the same node and the props merge, so declaring twice is
    /// declaring once. The bool answers whether this call CREATED the node —
    /// the one thing a caller cannot work out from the id it got back.
    pub fn put_entity(&mut self, scope: u64, term: &Term) -> Result<(u64, bool), IngestError> {
        let existed = self.entity_by_name(scope, &term.value)?.is_some();
        match self.resolve(scope, term)? {
            Endpoint::Entity(node_id) => Ok((node_id, !existed)),
            // A `user` term resolves into the identity collection (ADR-0014),
            // never into a corpus node: there is no node id to hand back, and
            // letting members through here would be a second identity path.
            Endpoint::User { .. } => Err(IngestError::Refused(
                "a `user` term is an identity, not a node to declare".to_string(),
            )),
        }
    }

    /// Land one fact (ADR-0010 §1 steps 1–2): resolve both endpoints to
    /// nodes, then link the fact or adjudicate it against an incumbent.
    pub fn ingest(&mut self, scope: u64, fact: &FactTuple) -> Result<IngestOutcome, IngestError> {
        let src = self.resolve(scope, &fact.head)?;
        let dst = self.resolve(scope, &fact.tail)?;
        let functional = matches!(
            fact.rel.props.get(FUNCTIONAL_PROP),
            Some(Variant::Bool(true))
        );
        let since = match fact.rel.props.get("since") {
            Some(Variant::U64(v)) => *v,
            Some(Variant::I64(v)) => *v as u64,
            _ => 0,
        };
        let confidence = match fact.rel.props.get("confidence") {
            Some(Variant::U64(v)) => (*v).min(255) as u8,
            Some(Variant::I64(v)) => (*v).clamp(0, 255) as u8,
            _ => 255,
        };
        // The rel props that ride the edge's dynamic segment: the open
        // modifiers. The declared fields are projections of their props —
        // kept out of the dynamic entry so one datum is not stored twice;
        // `functional` is the caller's declaration, the adjudication
        // input, and is kept (the assertion's own record). The weight
        // system's five counters are the same kind of exclusion: system
        // metadata never rides the business layer (ADR-0005).
        let mut props: BTreeMap<String, Variant> = fact
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
        // A reference link travels as an ADDRESS (the tuple's hex), and is
        // stored as the tuple's bytes — one spelling in JSON, in a URL and in
        // a rendered line.
        wire_refs(&mut props);

        // ASSERTION IDENTITY (okm ADR-0041): `(head, kind, tail)` is ONE edge.
        // The pair lives in the index key, so this is a seek rather than a scan
        // over a hub's out-degree — and it is what makes a re-read harmless: a
        // review pass that reads an overlapping span re-posts facts, and they
        // land on the edges that already stand.
        //
        // Only a STRICT improvement rewrites: a re-assertion that states a newer
        // `since` or a higher `confidence` is news about the same fact and
        // rewrites the row (the write is countable, as the weight system wants);
        // one that states the same thing is `Bumped` and writes nothing, so a
        // background pass reading the same material twice cannot inflate a
        // weight by repeating itself.
        // One exact key, not a scan: the tuple IS the address, so the
        // adjudication probe is a point read on the same key the write will
        // name (okm ADR-0067, `Graph::edge_at`).
        if let Some(existing) = self
            .facts()
            .edge_at(&fact.rel.name, &src.to_ref(scope), &dst.to_ref(scope))
            .map_err(|e| IngestError::Storage(e.to_string()))?
        {
            let body = self
                .facts()
                .get_edge(&existing)
                .map_err(|e| IngestError::Storage(e.to_string()))?
                .ok_or_else(|| {
                    IngestError::Storage(format!("edge {} unreadable", key_hex(&existing)))
                })?;
            let old = Facts::decode_payload(&body.payload);
            if since > old.since || confidence > old.confidence {
                let next = Facts {
                    since,
                    confidence,
                    ..old
                }
                .touched(Touch::Write, since);
                // Same head, same edge: the write re-asserts the row (faces and
                // attributes standing, no gap) and the assertion's open props
                // follow in their own carrier.
                let key = self.write_edge(&next, scope, src, dst, &fact.rel.name, &props)?;
                self.bin(scope, &[src, dst])?;
                return Ok(IngestOutcome::Rewritten { key });
            }
            return Ok(IngestOutcome::Bumped { key: existing });
        }
        if functional {
            if let Some(incumbent) = self
                .typed_out_of(&src.to_ref(scope), &fact.rel.name)?
                .first()
                .cloned()
            {
                // The face named the incumbent, so a missing row is an
                // inconsistency, not an absent fact.
                let body = self
                    .facts()
                    .get_edge(&incumbent)
                    .map_err(|e| IngestError::Storage(e.to_string()))?
                    .ok_or_else(|| {
                        IngestError::Storage(format!("edge {} unreadable", key_hex(&incumbent)))
                    })?;
                let old = Facts::decode_payload(&body.payload);
                if since > old.since || confidence >= old.confidence {
                    // The assertion's own write: the counters carry over,
                    // `since`/`confidence` are the new fact's, and the
                    // weight folds in THIS write plus the decay the gap
                    // accrued (ADR-0005).
                    let next = Facts {
                        since,
                        confidence,
                        ..old
                    }
                    .touched(Touch::Write, since);
                    // A DIFFERENT head is a DIFFERENT edge (okm ADR-0067), so
                    // this rewrite is a delete plus a write: the incumbent's
                    // rows and its own attributes go, then the new tuple lands
                    // with the props of THIS assertion. Two batches, in that
                    // order — okm has no in-place renumbering to offer, and
                    // inventing one would be the numbered model back again.
                    self.purge_head(&incumbent)?;
                    let key = self.write_edge(&next, scope, src, dst, &fact.rel.name, &props)?;
                    self.bin(scope, &[src, dst])?;
                    return Ok(IngestOutcome::Rewritten { key });
                }
                return Ok(IngestOutcome::Dropped { key: incumbent });
            }
        }
        let edge = Facts {
            scope,
            since,
            confidence,
            ..Default::default()
        }
        .touched(Touch::Write, since);
        let key = self.write_edge(&edge, scope, src, dst, &fact.rel.name, &props)?;
        self.bin(scope, &[src, dst])?;
        Ok(IngestOutcome::Linked { key })
    }

    /// Resolve a term to a node id: the exact name entrance first, then a
    /// new node (kind allocated through the collection's dictionary).
    /// Props land on the node row — an entity's attributes are shared by
    /// every fact about it, so they MERGE into the node's dynamic
    /// segment (a same-named attribute takes the newer value; the rest
    /// survive, unlike a wholesale replace).
    fn resolve(&mut self, scope: u64, term: &Term) -> Result<Endpoint, IngestError> {
        // The `user` type token names a REGISTERED member (ADR-0014): the value
        // is the wire reference `member-{channel}-{member}`, resolved through
        // the identity collection. Nothing is created on a miss — a person is
        // registered, never discovered — and no props land on a member row:
        // admin-time identity is not extracted knowledge.
        if term.type_label == USER_KIND {
            let member = parse_member_ref(&term.value).ok_or_else(|| {
                IngestError::Refused(format!(
                    "user reference `{}` is not member-{{uuid}}",
                    term.value
                ))
            })?;
            let members = self.members.as_ref().ok_or_else(|| {
                IngestError::Refused(
                    "a `user` endpoint needs the channel keyspace attached".to_string(),
                )
            })?;
            let known = members
                .get_row(&MemberKey { member_id: member })
                .map_err(|e| IngestError::Storage(e.to_string()))?
                .is_some();
            if !known {
                return Err(IngestError::Refused(format!(
                    "`{}` is not a registered member",
                    term.value
                )));
            }
            return Ok(Endpoint::User { member });
        }
        if let Some((node_id, _)) = self.entity_by_name(scope, &term.value)? {
            self.merge_props(scope, node_id, &term.props)?;
            return Ok(Endpoint::Entity(node_id));
        }
        let node_id = self.next_node;
        self.next_node += 1;
        // ADR-0003: the vector is computed INSIDE k10r, over the node's NAME
        // (an entity's semantic handle). No engine = EMPTY frame, a legal row
        // the retrieval side skips. An engine that fails to embed is an error
        // the caller sees — never a silently vectorless node.
        let embedding = match &self.embedder {
            Some(engine) => Vector::new(
                engine
                    .embed(&embedding_text(&term.value, &term.props))
                    .map_err(|e| IngestError::Embedding(e.to_string()))?,
            ),
            None => Vector::new(Vec::new()),
        };
        let row = EntityRow {
            kind: Vocab::of(term.type_label.clone()),
            literal_num: literal_num(term),
            name: term.value.clone(),
            // A fresh node has been used zero times; a merge onto an existing
            // node keeps whatever it had (this path only creates).
            invoke_calls: 0,
            invoke_ok: 0,
            last_invoked: 0,
        };
        let key = EntityKey { scope, node_id };
        // One batch: the node and its vector land together or not at all
        // (ADR-0019 — the side table's whole point is that the pair is one
        // write, not two writes that can be torn apart by a crash).
        let mut batch = Batch::default();
        self.entities
            .save_with_prev_into(&mut batch, &key, &row, None)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        // A task owns a sheet the moment it exists (ADR-0021): presence of the
        // sheet IS "this node is a task", so a task created by a FACT — not by
        // the task door — still lists as open. Same batch, so a vertex can never
        // be visible without the row that says what it is.
        if row.kind.as_str() == Some(TASK_KIND) {
            let sheet = TaskSheet::opened();
            self.sheets
                .save_with_prev_into(&mut batch, &key, &sheet, None)
                .map_err(|e| IngestError::Storage(e.to_string()))?;
        }
        // No engine = no vector row at all, rather than an EMPTY frame nothing
        // reads (presence is the boolean).
        if !embedding.is_empty() {
            let vector = NodeVector { embedding };
            self.vectors
                .save_with_prev_into(&mut batch, &key, &vector, None)
                .map_err(|e| IngestError::Storage(e.to_string()))?;
        }
        self.store
            .commit_batch(&batch)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        // The scope's in-memory vector table is derived state: drop it rather
        // than maintain it row by row (it reloads on the next retrieval).
        self.tables.forget(scope);
        if !term.props.is_empty() {
            // Non-declared names: a map payload writes the segment, the row
            // stays as written above.
            self.entities
                .put(&key, &Variant::Map(crate::document::dict(&term.props)))
                .map_err(|e| IngestError::Storage(e.to_string()))?;
        }
        Ok(Endpoint::Entity(node_id))
    }

    /// One task's sheet, if the node has one (ADR-0021).
    pub(crate) fn task_sheet(
        &mut self,
        scope: u64,
        node_id: u64,
    ) -> Result<Option<TaskSheet>, IngestError> {
        self.sheets
            .get_row(&EntityKey { scope, node_id })
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// Write one task's sheet. The vertex is untouched: a state change is not a
    /// re-label, a re-embed or a prop merge.
    pub(crate) fn put_task_sheet(
        &mut self,
        scope: u64,
        node_id: u64,
        sheet: &TaskSheet,
    ) -> Result<(), IngestError> {
        self.sheets
            .put(&EntityKey { scope, node_id }, sheet)
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// The sheet collection, for the face scan `tasks::list_tasks` runs.
    pub(crate) fn sheet_collection(&self) -> &Collection<FjallStore, EntityKey, TaskSheet> {
        &self.sheets
    }

    /// One code version's payload, if that revision was ever written.
    pub(crate) fn code_sheet_at(
        &mut self,
        scope: u64,
        code_id: u64,
        rev: u64,
    ) -> Result<Option<CodeRow>, IngestError> {
        self.codes
            .get_row(&CodeKey {
                scope,
                code_id,
                rev,
            })
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// The CURRENT version of a script: the last key of its range.
    ///
    /// A script's revisions are contiguous in key order, so ordering answers
    /// "which is current" — there is no head pointer to fall out of step with
    /// the rows. The read is ONE backward seek to the end of that script's key
    /// range (never the table, never the history) plus one point read of the
    /// version it lands on.
    ///
    /// The probe is TYPED (okm ADR-0049): the fields are the key's own leading
    /// segments, so the address header (ns prefix + primary slot) is spliced by
    /// the Collection instead of by hand — an inserted key field breaks this call
    /// site at compile time, where the hand-encoded form would have silently
    /// re-bound the scope's bytes to the wrong segment.
    pub(crate) fn latest_code(
        &mut self,
        scope: u64,
        code_id: u64,
    ) -> Result<Option<(u64, CodeRow)>, IngestError> {
        let probe = CodeKey::prefix().scope(scope).code_id(code_id).finish();
        let Some(key) = self.codes.scan_keys(&probe).next_back() else {
            return Ok(None);
        };
        let key = key.map_err(|e| IngestError::Storage(e.to_string()))?;
        Ok(self
            .codes
            .get_row(&key)
            .map_err(|e| IngestError::Storage(e.to_string()))?
            .map(|row| (key.rev, row)))
    }

    /// Write one code version's payload. The vertex is untouched: publishing a
    /// script is not a re-label and not a prop merge — and an existing version is
    /// never overwritten (a revision that may already have run stays readable).
    pub(crate) fn put_code_sheet(
        &mut self,
        scope: u64,
        code_id: u64,
        rev: u64,
        row: &CodeRow,
    ) -> Result<(), IngestError> {
        self.codes
            .put(
                &CodeKey {
                    scope,
                    code_id,
                    rev,
                },
                row,
            )
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// The code table, for the export boundary (ADR-0007's backup).
    pub fn code_table(&self) -> &Collection<FjallStore, CodeKey, CodeRow> {
        &self.codes
    }

    /// The next code id, and move the watermark one past it.
    pub(crate) fn take_code_id(&mut self) -> u64 {
        let id = self.next_code;
        self.next_code = id.saturating_add(1);
        id
    }

    /// Recompute the code watermark from the sheets — the open path, reused
    /// after a restore wrote rows underneath it (a restored row's id must never
    /// be handed out again).
    pub fn reseed_code(&mut self) -> Result<(), IngestError> {
        self.next_code = self
            .codes
            .scan_keys(&okm_core::PrefixProbe::empty())
            .map(|r| r.map_err(|e| IngestError::Storage(e.to_string())))
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .map(|key| key.code_id)
            .max()
            .map_or(1, |id| id.saturating_add(1));
        Ok(())
    }

    /// Merge props into an existing node's dynamic segment — the write a
    /// counter bump uses: no re-embed, no row rewrite, just the segment.
    pub(crate) fn merge_props(
        &mut self,
        scope: u64,
        node_id: u64,
        props: &BTreeMap<String, Variant>,
    ) -> Result<(), IngestError> {
        if props.is_empty() {
            return Ok(());
        }
        // A map payload IS the delta write: it reads the stored segment,
        // overwrites the names given and leaves the rest — so the
        // read-modify-write that used to live here is `put`'s own work now.
        self.entities
            .put(
                &EntityKey { scope, node_id },
                &Variant::Map(crate::document::dict(props)),
            )
            .map_err(|e| IngestError::Storage(e.to_string()))
    }

    /// Write one assertion at its HEAD and its open props — the one write path
    /// for a create and a re-assertion.
    ///
    /// Two carriers: the edge's rows go through okm's single write verb
    /// ([`Graph::incorporate`], a fragment naming the head), which owns its own
    /// engine batch and — create or re-assertion alike — retires the rows and
    /// declared faces the previous payload wrote before putting the new ones, so
    /// no face ever carries the edge twice; and the open attributes ride their
    /// own segment (0x01) in a second batch. The ordering is what keeps the edge
    /// whole: the ROW lands first, so the fact is never observable as absent, and
    /// the props cannot precede it because attributes of a row that is not there
    /// would describe nothing.
    ///
    /// There is no third case: a different tuple is a different edge, so a
    /// "rewrite" is [`purge_head`](Self::purge_head) plus this call, never an
    /// update in place (okm ADR-0067).
    #[allow(clippy::too_many_arguments)]
    fn write_edge(
        &mut self,
        edge: &Facts,
        scope: u64,
        src: Endpoint,
        dst: Endpoint,
        kind: &str,
        props: &BTreeMap<String, Variant>,
    ) -> Result<EdgeKey, IngestError> {
        let head = EdgeHead {
            src: src.to_ref(scope),
            dst: dst.to_ref(scope),
            predicate: kind.to_string(),
        };
        // Naming the head IS the write: the tuple creates the edge when no row
        // holds it and re-asserts it when one does — one batch, one commit. The
        // conflict hook answers the payload, so it does not read either side:
        // the caller already decided.
        let next = edge.clone();
        let mut fragment = Fragment::<Facts>::new([Decl::edge(head)]);
        fragment.on_edge_conflict(move |_stored, _declared| next.clone());
        let report = self
            .facts()
            .incorporate(fragment)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        // The fragment reports the identity it wrote — the tuple, not a number.
        let Some(key) = report.edges.first().cloned() else {
            return Err(IngestError::Storage(
                "the write fragment reported no edge".to_string(),
            ));
        };
        let mut batch = Batch::default();
        // The edge's open attributes (okm ADR-0034 §4): the 0x01 segment is the
        // GRAPH's, and `replace_into` deletes the previous assertion's segment
        // first — props belong to the claim being written, never to the one it
        // replaces.
        self.facts()
            .replace_into(&mut batch, &key, &crate::document::dict(props))
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        self.store
            .commit_batch(&batch)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        Ok(key)
    }

    /// Remove one assertion outright: its row, every derived face entry and its
    /// open attributes, in one batch (okm's `retract`).
    ///
    /// The delete verb takes a head by predicate NAME while a read answers one by
    /// predicate ID, so the name comes back through the graph's own vocabulary —
    /// one 0x04 read, and a predicate no edge ever carried is nothing to do.
    fn purge_head(&mut self, key: &EdgeKey) -> Result<(), IngestError> {
        let mut facts = self.facts();
        let Some(predicate) = facts
            .predicate_vocab_name(key.predicate)
            .map_err(|e| IngestError::Storage(e.to_string()))?
        else {
            return Ok(());
        };
        let mut fragment = Fragment::<Facts>::new([]);
        fragment.delete_edge(EdgeHead {
            src: key.src.clone(),
            dst: key.dst.clone(),
            predicate,
        });
        facts
            .retract(fragment)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Fold one entity into another: the loser's edges move onto the survivor
    /// WITH their payloads, its row goes, and the graph half is ONE batch
    /// (okm `replace`: no reader sees the gap, and a refusal writes nothing).
    ///
    /// Three cases, and the caller decides none of them:
    ///
    /// - An edge between the two becomes a self-loop, so it is DELETED, not moved.
    /// - Two edges that land on the same head after the fold (the survivor already
    ///   had one, or two of the loser's do) collapse: the incumbent row wins and
    ///   the duplicate is deleted. Silently keeping both would need a head to hold
    ///   two payloads, which is the one thing an edge cannot do.
    /// - Everything else is repointed — the head changes, the payload rides along.
    ///
    /// Every edge incident to the loser is stated in the fragment, which is what
    /// okm's completeness check demands before it will retire a node.
    ///
    /// The loser's entity row and vector go afterwards, in their own batch: they
    /// are k10r's documents, not graph rows. A crash between the two leaves an
    /// entity with no edges — visible, and cheaper than the reverse.
    pub fn merge(
        &mut self,
        scope: u64,
        loser: u64,
        survivor: u64,
    ) -> Result<MergeReport, IngestError> {
        if loser == survivor {
            return Err(IngestError::Refused(
                "a merge needs two different entities".to_string(),
            ));
        }
        if self.entity(scope, loser)?.is_none() {
            return Err(IngestError::Refused(format!(
                "entity {loser} is not in scope {scope}"
            )));
        }
        if self.entity(scope, survivor)?.is_none() {
            return Err(IngestError::Refused(format!(
                "entity {survivor} is not in scope {scope}"
            )));
        }
        let loser_ref = self.node_ref(scope, loser);
        let survivor_ref = self.node_ref(scope, survivor);
        let mut keys = self.out_edges(scope, loser)?;
        keys.extend(self.in_edges(scope, loser)?);

        let mut facts = self.facts();
        let mut fragment = Fragment::<Facts>::new([]);
        let mut repointed = 0usize;
        let mut collapsed = 0usize;
        let mut absorbed = 0usize;
        // The heads this batch already claims: two edges of the loser that fold
        // onto one head is a duplicate, not two facts.
        let mut claimed: HashSet<ClaimedHead> = HashSet::new();
        for key in keys {
            let Some(predicate) = facts
                .predicate_vocab_name(key.predicate)
                .map_err(|e| IngestError::Storage(e.to_string()))?
            else {
                // A predicate the vocabulary no longer holds is a face without a
                // name; nothing can be re-attached under it.
                continue;
            };
            let from = EdgeHead {
                src: key.src.clone(),
                dst: key.dst.clone(),
                predicate: predicate.clone(),
            };
            let src_is_loser = key.src == loser_ref;
            let far = if src_is_loser {
                key.dst.clone()
            } else {
                key.src.clone()
            };
            // Both ends are inside the fold: the edge becomes a self-loop.
            if far == survivor_ref || far == loser_ref {
                fragment.delete_edge(from);
                collapsed += 1;
                continue;
            }
            let to = if src_is_loser {
                EdgeHead {
                    src: survivor_ref.clone(),
                    dst: key.dst.clone(),
                    predicate,
                }
            } else {
                EdgeHead {
                    src: key.src.clone(),
                    dst: survivor_ref.clone(),
                    predicate,
                }
            };
            let head = (
                to.src.ns_id(),
                to.src.pkey.clone(),
                key.predicate,
                to.dst.ns_id(),
                to.dst.pkey.clone(),
            );
            if !claimed.insert(head.clone()) {
                fragment.delete_edge(from);
                absorbed += 1;
                continue;
            }
            let incumbent = facts
                .key_of(&to)
                .map_err(|e| IngestError::Storage(e.to_string()))?
                .is_some_and(|existing| facts.get_edge(&existing).ok().flatten().is_some());
            if incumbent {
                fragment.delete_edge(from);
                absorbed += 1;
                continue;
            }
            fragment.repoint(from, to);
            repointed += 1;
        }
        fragment.delete_node(loser_ref);
        facts
            .replace(fragment)
            .map_err(|e| IngestError::Storage(e.to_string()))?;

        // k10r's own rows for the loser: the entity and its vector. Nothing
        // cascades, and nothing else refers to them.
        let key = EntityKey {
            scope,
            node_id: loser,
        };
        self.entities
            .purge(&key)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        self.vectors
            .purge(&key)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        Ok(MergeReport {
            survivor,
            repointed,
            collapsed,
            absorbed,
        })
    }

    /// Divide one entity: a new entity is declared, and the edges the caller names
    /// move onto it — payloads and all (okm `repoint`).
    ///
    /// The caller decides WHICH edges change hands; that is judgement, and it stays
    /// with whoever proposed the split. Everything else is mechanical: the row
    /// lands first (its own batch), the graph batch second, so a failure between
    /// them leaves an isolated entity rather than an edge pointing at a node with
    /// no row.
    ///
    /// Nothing is recorded about the split — no ledger, no alias. The shape that is
    /// is the shape; the old cut was unreasonable by definition, or it would not
    /// have been cut.
    ///
    /// The graph half is ONE batch and is okm's `replace`, not `incorporate`:
    /// repointing is a retraction of the old head, and `incorporate` refuses to
    /// retract.
    pub fn split(
        &mut self,
        scope: u64,
        node: u64,
        name: &str,
        kind: &str,
        moving: &[EdgeKey],
    ) -> Result<SplitReport, IngestError> {
        if self.entity(scope, node)?.is_none() {
            return Err(IngestError::Refused(format!(
                "entity {node} is not in scope {scope}"
            )));
        }
        let source_ref = self.node_ref(scope, node);
        let created = self.next_node;
        let new_ref = self.node_ref(scope, created);

        // The new entity's row, before the graph learns its name: an isolated
        // entity is visible and harmless, a dangling endpoint is not.
        self.next_node += 1;
        let embedding = match &self.embedder {
            Some(engine) => Vector::new(
                engine
                    .embed(name)
                    .map_err(|e| IngestError::Embedding(e.to_string()))?,
            ),
            None => Vector::new(Vec::new()),
        };
        let row = EntityRow {
            kind: Vocab::of(kind.to_string()),
            literal_num: 0,
            name: name.to_string(),
            invoke_calls: 0,
            invoke_ok: 0,
            last_invoked: 0,
        };
        let key = EntityKey {
            scope,
            node_id: created,
        };
        let mut batch = Batch::default();
        self.entities
            .save_with_prev_into(&mut batch, &key, &row, None)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        if row.kind.as_str() == Some(TASK_KIND) {
            let sheet = TaskSheet::opened();
            self.sheets
                .save_with_prev_into(&mut batch, &key, &sheet, None)
                .map_err(|e| IngestError::Storage(e.to_string()))?;
        }
        if !embedding.is_empty() {
            let vector = NodeVector { embedding };
            self.vectors
                .save_with_prev_into(&mut batch, &key, &vector, None)
                .map_err(|e| IngestError::Storage(e.to_string()))?;
        }
        self.store
            .commit_batch(&batch)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        self.tables.forget(scope);

        let mut facts = self.facts();
        let mut fragment = Fragment::<Facts>::new([]);
        let mut repointed = 0usize;
        for edge in moving {
            let Some(predicate) = facts
                .predicate_vocab_name(edge.predicate)
                .map_err(|e| IngestError::Storage(e.to_string()))?
            else {
                return Err(IngestError::Refused(
                    "an edge names a predicate the vocabulary does not hold".to_string(),
                ));
            };
            let from = EdgeHead {
                src: edge.src.clone(),
                dst: edge.dst.clone(),
                predicate: predicate.clone(),
            };
            let to = if edge.src == source_ref {
                EdgeHead {
                    src: new_ref.clone(),
                    dst: edge.dst.clone(),
                    predicate,
                }
            } else if edge.dst == source_ref {
                EdgeHead {
                    src: edge.src.clone(),
                    dst: new_ref.clone(),
                    predicate,
                }
            } else {
                return Err(IngestError::Refused(format!(
                    "edge {} does not touch entity {node}",
                    key_hex(edge)
                )));
            };
            // A row that is already there means this edge cannot move here: the
            // pair is already stated, and two payloads on one head is the one
            // thing an edge cannot do.
            if facts
                .key_of(&to)
                .map_err(|e| IngestError::Storage(e.to_string()))?
                .is_some_and(|existing| facts.get_edge(&existing).ok().flatten().is_some())
            {
                return Err(IngestError::Refused(format!(
                    "the split would state an edge that already exists ({})",
                    key_hex(edge)
                )));
            }
            fragment.repoint(from, to);
            repointed += 1;
        }
        facts
            .replace(fragment)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        Ok(SplitReport {
            node,
            created,
            repointed,
        })
    }

    /// The fact's open props (its dynamic segment), when it has one — the
    /// graph's own read (okm ADR-0034 §4), not a second decoder beside it.
    pub fn fact_props(
        &self,
        key: &EdgeKey,
    ) -> Result<Option<BTreeMap<String, Variant>>, IngestError> {
        // `attrs` answers okm's name map (`Dict`); this crate's bag is the
        // sorted one, so the conversion is here, at the okm door.
        Ok(self
            .facts()
            .attrs(key)
            .map_err(|e| IngestError::Storage(e.to_string()))?
            .map(|names| names.into_iter().collect()))
    }

    /// One node as a graph endpoint: the ref carries the entity key's own
    /// bytes, so any collection participates with zero declaration.
    fn node_ref(&self, scope: u64, node_id: u64) -> NodeRef {
        NodeRef::new(
            <EntityRow as Document>::NS_PREFIX,
            &EntityKey { scope, node_id }.encode(),
        )
    }
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

// ---- ADR-0010 steps 3–4: the write-side binning pass and the read side

impl GraphStore {
    /// ADR-0010 §2's binning pass, run after a fact lands: load the 2-hop
    /// neighbourhood, run incremental LPA over it, and move the community
    /// clusters to whatever the vote decided — as VERTEX sets (okm ADR-0068),
    /// because a partition's members are the entities and not the edges between
    /// them: an entity no edge touches is a legal member, and a hub costs the
    /// same to read as a leaf.
    ///
    /// The pass OWNS the bins. Membership is a row on the node face, so the
    /// seeds are the node's own declared community (smallest, the vote's own
    /// tie-break), and a neighbourhood nobody has binned yet gets a fresh set
    /// per connected component — allocated by okm's own `next_free_cluster`,
    /// which is why k10r keeps no counter of its own.
    ///
    /// No LLM, no transport, no global graph: the vote is local and the
    /// weights are ADR-0005's — a fresh assertion weighs `ln(2)×0.5`, so a
    /// vote never degenerates to an all-zero tie.
    fn bin(&mut self, scope: u64, touched: &[Endpoint]) -> Result<(), IngestError> {
        // A user node never enters a community (it is not corpus knowledge):
        // the bin pass takes the entity endpoints of the fact and nothing else.
        let touched: Vec<u64> = touched.iter().filter_map(|e| e.entity_id()).collect();
        let members = self.local_subgraph(scope, &touched)?;
        if members.is_empty() {
            return Ok(());
        }
        let kind = self.community_kind()?;

        // One walk over the neighbourhood's edges: the vote's adjacency, the
        // head that names them (the membership rows are keyed by it) and the
        // cluster each one is in today — the pass reads its own previous
        // answer, because the write path no longer leaves one.
        let mut adjacency: HashMap<u64, Vec<(u64, f64)>> = HashMap::new();
        let mut edges: BTreeMap<EdgeKey, (EdgeHead, u64, u64)> = BTreeMap::new();
        for &node in &members {
            let mut keys = self.out_edges(scope, node)?;
            keys.extend(self.in_edges(scope, node)?);
            for key in keys {
                if edges.contains_key(&key) {
                    continue;
                }
                // A head whose edge is gone is absence (the ref list outlived
                // its row), not a failure of the pass.
                let Some(view) = self.fact_view(&key)? else {
                    continue;
                };
                let (Some(src), Some(dst)) = (view.src.id.entity_id(), view.dst.id.entity_id())
                else {
                    continue;
                };
                if src == dst || !members.contains(&src) || !members.contains(&dst) {
                    continue;
                }
                let head = EdgeHead {
                    src: self.node_ref(scope, src),
                    dst: self.node_ref(scope, dst),
                    predicate: view.kind.clone(),
                };
                let vote = (view.edge.weight as f64 / WEIGHT_SCALE)
                    * (view.edge.confidence as f64 / 255.0);
                adjacency.entry(src).or_default().push((dst, vote));
                adjacency.entry(dst).or_default().push((src, vote));
                edges.insert(view.key.clone(), (head, src, dst));
            }
        }

        // Seeds: the SMALLEST community the node is DECLARED in — the same
        // tie-break the vote uses, so the pass does not depend on which edge
        // happened to come first in key order. A node in none starts at 0 ("no
        // community yet"), and the component allocation below gives it one.
        // This reads the node's own membership: with a vertex partition the
        // edge's community would be a second, derivable answer, and two owners
        // of one answer is the failure this pass exists to avoid.
        let mut assign: HashMap<u64, u64> = HashMap::new();
        for &node in &members {
            let current = self
                .facts()
                .node_sets_of(&self.node_ref(scope, node))
                .filter(|set| set.kind == kind)
                .map(|set| set.id)
                .min();
            assign.insert(node, current.unwrap_or(0));
        }
        let mut ids: Vec<u64> = assign.keys().copied().collect();
        ids.sort_unstable();

        for _ in 0..LPA_ROUNDS {
            let mut changed = false;
            for node in &ids {
                let Some(neighbours) = adjacency.get(node) else {
                    continue;
                };
                if neighbours.is_empty() {
                    continue;
                }
                let mut votes: BTreeMap<u64, f64> = BTreeMap::new();
                for (other, weight) in neighbours {
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

        // Whatever the vote left at zero is a community nobody has created
        // yet: one fresh cluster per connected component of those nodes, and
        // okm hands out the id.
        let mut unassigned: HashSet<u64> = ids
            .iter()
            .copied()
            .filter(|node| assign[node] == 0)
            .collect();
        while let Some(&start) = unassigned.iter().next() {
            unassigned.remove(&start);
            let mut component = vec![start];
            let mut cursor = 0;
            while cursor < component.len() {
                let node = component[cursor];
                cursor += 1;
                if let Some(neighbours) = adjacency.get(&node) {
                    for (other, _) in neighbours {
                        if unassigned.remove(other) {
                            component.push(*other);
                        }
                    }
                }
            }
            // The component IS the membership: an entity no edge touches is a
            // legal member of a node set, which is why this arm no longer needs
            // an edge to name the cluster with.
            let mut fragment = Fragment::<Facts>::new([]);
            fragment.cluster(Cluster::nodes(
                COMMUNITY_KIND,
                crate::document::dict(&BTreeMap::new()),
                component.iter().map(|node| self.node_ref(scope, *node)),
            ));
            let report = self
                .facts()
                .incorporate(fragment)
                .map_err(|e| IngestError::Storage(e.to_string()))?;
            let Some(id) = report.node_sets.first().map(|set| set.id) else {
                continue;
            };
            for node in component {
                assign.insert(node, id);
            }
        }

        // The memberships follow the bins: a node whose vote named a different
        // community leaves its old set and joins the new one in ONE fragment,
        // so no reader sees it in both.
        self.sync_membership(scope, kind, &assign)
    }

    /// Move every neighbourhood entity's membership to the community the vote
    /// gave it.
    ///
    /// One fragment per `(new community, old communities)` group carries the
    /// removal and the addition together: split across two commits, a window
    /// exists where the entity is in neither bin. `remove_nodes` names the nodes
    /// explicitly, so the fragment does not have to re-declare them.
    fn sync_membership(
        &mut self,
        scope: u64,
        kind: Kind,
        assign: &HashMap<u64, u64>,
    ) -> Result<(), IngestError> {
        let mut groups: BTreeMap<(u64, Vec<u64>), Vec<u64>> = BTreeMap::new();
        for (&node, &wanted) in assign {
            let current: Vec<u64> = self
                .facts()
                .node_sets_of(&self.node_ref(scope, node))
                .filter(|set| set.kind == kind)
                .map(|set| set.id)
                .collect();
            let settled = match wanted {
                0 => current.is_empty(),
                id => current.len() == 1 && current[0] == id,
            };
            if settled {
                continue;
            }
            groups.entry((wanted, current)).or_default().push(node);
        }
        for ((wanted, old), nodes) in groups {
            let mut fragment = Fragment::<Facts>::new([]);
            for cluster in old {
                fragment.remove_nodes(
                    NodeSet::new(kind, cluster),
                    nodes.iter().map(|node| self.node_ref(scope, *node)),
                );
            }
            if wanted != 0 {
                fragment.cluster(
                    Cluster::nodes(
                        COMMUNITY_KIND,
                        crate::document::dict(&BTreeMap::new()),
                        nodes.iter().map(|node| self.node_ref(scope, *node)),
                    )
                    .at(wanted),
                );
            }
            self.facts()
                .incorporate(fragment)
                .map_err(|e| IngestError::Storage(e.to_string()))?;
        }
        Ok(())
    }

    /// The neighbourhood the vote looks at: everything within `LPA_RADIUS` hops
    /// of the touched entities, as entity ids.
    ///
    /// This is okm's `subgraph`, not a walk of our own: it answers the selection
    /// (members, the edges inside, the half-edges crossing out) in one pass over
    /// the traversal faces, with a `BTreeSet` visited set instead of a `Vec` scan.
    /// The seeds are a SET — the pass starts from every endpoint the fact touched —
    /// and `hops` counts the rings that are INSIDE the selection, so the answer is
    /// the same depth this pass always used.
    ///
    /// Only the member ids are read back: the caller votes on adjacency, and the
    /// entities' rows are never looked at.
    fn local_subgraph(&mut self, scope: u64, touched: &[u64]) -> Result<HashSet<u64>, IngestError> {
        let seeds: Vec<NodeRef> = touched
            .iter()
            .map(|node| self.node_ref(scope, *node))
            .collect();
        let cut = self
            .facts()
            .subgraph(Selection::of(seeds), LPA_RADIUS as u8, &[])
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        Ok(cut
            .members
            .iter()
            .map(|node| EntityKey::decode(&node.pkey).node_id)
            .collect())
    }

    /// The community's entities, as DECLARED (okm ADR-0068): one prefix scan of
    /// the node mirror and one registry batch, with no edge involved. An entity
    /// no fact touches is still a member — declaring it is the whole statement.
    #[allow(clippy::type_complexity)]
    pub fn community_members(
        &mut self,
        scope: u64,
        community: u64,
    ) -> Result<Vec<(u64, EntityRow)>, IngestError> {
        let kind = self.community_kind()?;
        let handles = self
            .hyper()
            .node_members(NodeSet::new(kind, community))
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        let mut members = Vec::with_capacity(handles.len());
        for node in handles {
            let id = EntityKey::decode(&node.pkey).node_id;
            if let Some(row) = self.entity(scope, id)? {
                members.push((id, row));
            }
        }
        Ok(members)
    }

    /// The facts INSIDE one community: both endpoints are members of the set,
    /// each edge once — the read side's bucket (ADR-0010 §5).
    ///
    /// A node set does not store the edges between its members, so this is an
    /// INDUCED subgraph: walk the members' out-edges and keep the ones whose far
    /// end is a member too (an undirected pair is seen twice, so the visit is
    /// deduped by key). The cost is the members' out-degree instead of one
    /// member-face scan — the price of storing a partition rather than the
    /// assertions, and the bucket read is where it shows.
    pub fn community_facts(
        &mut self,
        scope: u64,
        community: u64,
    ) -> Result<Vec<FactView>, IngestError> {
        let kind = self.community_kind()?;
        let handles = self
            .hyper()
            .node_members(NodeSet::new(kind, community))
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        let inside: HashSet<u64> = handles
            .iter()
            .map(|node| EntityKey::decode(&node.pkey).node_id)
            .collect();
        let mut seen: HashSet<EdgeKey> = HashSet::new();
        let mut facts = Vec::new();
        for &node in &inside {
            let node_ref = self.node_ref(scope, node);
            for key in self
                .facts()
                .out_edges(&node_ref)
                .map_err(|e| IngestError::Storage(e.to_string()))?
            {
                if !seen.insert(key.clone()) {
                    continue;
                }
                let Some(view) = self.fact_view(&key)? else {
                    continue;
                };
                if view.edge.scope != scope {
                    continue;
                }
                let far = view.dst.id.entity_id();
                if far.is_some_and(|far| inside.contains(&far)) {
                    facts.push(view);
                }
            }
        }
        Ok(facts)
    }
}

#[cfg(test)]
mod stale_id_tests {
    //! A ref whose entity row is gone is a STALE id: the graph kept the edge, the
    //! entity it names does not exist. The read must not render it — a hollow
    //! `n42  []` in a prompt is an invitation to invent, and nothing records where
    //! the entity went, because nothing records a re-cut.

    use super::*;

    fn tuple(text: &str) -> FactTuple {
        FactTuple::from_json(text).expect("the tuple decodes")
    }

    #[test]
    fn a_fact_whose_endpoint_row_is_gone_is_not_rendered() {
        let dir = tempfile::tempdir().unwrap();
        let mut graph = GraphStore::open(dir.path()).unwrap();
        let key = match graph
            .ingest(
                1,
                &tuple(r#"["alice","person",{},"KNOWS",{},"bob","person",{}]"#),
            )
            .unwrap()
        {
            IngestOutcome::Linked { key } => key,
            other => panic!("expected a link, got {other:?}"),
        };
        assert!(graph.fact_view(&key).unwrap().is_some(), "the fact renders");

        // The state a crash between a merge's two batches leaves: the graph batch
        // landed, the entity row did not go.
        let (alice, _) = graph.entity_by_name(1, "alice").unwrap().unwrap();
        graph
            .entities
            .purge(&EntityKey {
                scope: 1,
                node_id: alice,
            })
            .unwrap();

        assert!(
            graph.fact_view(&key).unwrap().is_none(),
            "the fact is not an answer any more: its endpoint does not exist"
        );
        let (bob, _) = graph.entity_by_name(1, "bob").unwrap().unwrap();
        assert!(
            graph.neighbours(1, bob).unwrap().is_empty(),
            "and no read path resurrects it from the other end"
        );
        // The ref itself still answers: the filter is at the fact door, not a panic.
        let node = graph.node_ref(1, alice);
        let resolved = graph.resolve_ref(&node).unwrap();
        assert_eq!(resolved.id.label(), format!("n{alice}"));
        assert!(resolved.name.is_empty(), "the row is what is missing");
    }
}
