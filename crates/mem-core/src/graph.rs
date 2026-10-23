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
//!   face) and `by_community` (the bucket read). The kind is a CATEGORICAL
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
//! ADR-0010's steps 3–4 are here: incremental LPA and community binning
//! run inside `ingest`, the node row carries `community_id` with its
//! `by_community` face, and a per-scope SEQUENCE row issues community ids.
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
use crate::embedding::Embedder;
use crate::render::RenderFormat;
use okm_core::obj_dynamic::DynamicValue;
use okm_core::{
    write_slot, Collection, Document, DocumentEncode, EdgeHead, FjallStore, Graph, GraphEncode,
    KeyEncode, Batch, NodeRef, Vector, VectorElem, VirtualStorage, Vocab, PRIMARY_SLOT,
};
use okm_vector::ExactIndex;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// Entity node collection (ADR-0010 §1's ns 36 — allocated at landing,
/// next free after the code store's 35).
pub const ENTITY_NS: u16 = 36;
/// The facts' edge collection (ns 37, one ns per graph).
pub const FACT_NS: u16 = 37;
/// The identity collection's ns — channel members, which are ALSO the graph's
/// user nodes (ADR-0014). It lives in the channel keyspace, not this one.
pub const MEMBER_NS: u16 = 42;
/// The reserved type token that names a user endpoint (ADR-0004's open type
/// vocabulary: `literal` first, `user` second). A value under it is the wire
/// reference `member-{channel}-{member}`, parsed into a node at ns 42 —
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
#[ok_name("entities")]
#[ok_layout(version = 3)]
#[ok_index(
    // Payload fields only: the key tail appends the primary key, so the
    // entries land as `[kind][scope][node_id]`,
    // `[name][scope][node_id]` and `[community_id][scope][node_id]`.
    // Text-first regime: an exact match re-verifies the row's name and the
    // decoded key's scope — a prefix scan for `alpha` also reaches
    // `alphabet`, which is what dictionary order means.
    by_kind { fields(kind) },
    by_name { fields(name) },
    by_community { fields(community_id) },
)]
pub struct EntityRow {
    pub kind: Vocab<u16>,
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
/// The scope-prefixed weight face: one probe yields a scope's keys in weight
/// order, so the progressive read decodes k rows for k answers.
use __OkmIndex_Facts_by_scope_weight as ByScopeWeight;
/// The temporal face: ascending in `since`, walked in reverse for newest-first.
use __OkmIndex_Facts_by_since as BySince;

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
    User { channel: u64, member: u64 },
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
            Endpoint::User { channel, member } => NodeRef::new(
                <MemberRow as Document>::NS_PREFIX,
                &MemberKey {
                    channel_id: channel,
                    member_id: member,
                }
                .encode(),
            ),
        }
    }
}

/// Parse the `user` type token's wire reference `member-{channel}-{member}`.
/// Numbers only: the syntax is a reference, never a name (a display name is a
/// row field and renaming must not move a fact).
fn parse_member_ref(value: &str) -> Option<(u64, u64)> {
    let rest = value.strip_prefix("member-")?;
    let (channel, member) = rest.split_once('-')?;
    Some((channel.parse().ok()?, member.parse().ok()?))
}

/// A member endpoint's key bytes back to `(channel, member)` — the decode side
/// of [`MemberKey`]'s `[channel 8B][member 8B]`.
fn member_from_pkey(pkey: &[u8]) -> Option<(u64, u64)> {
    if pkey.len() != 16 {
        return None;
    }
    let channel = u64::from_be_bytes(pkey[..8].try_into().ok()?);
    let member = u64::from_be_bytes(pkey[8..].try_into().ok()?);
    Some((channel, member))
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

/// The community sequence key: `[scope]`.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(38)]
pub struct CommunitySeqKey {
    pub scope: u64,
}

/// The scope's community high water: the last id issued, never lowered.
///
/// It is a COUNTER row, not "the maximum of the communities that exist":
/// a community's last member can leave, so an issuer reading live data
/// would hand that id out again after a restart — two different
/// communities sharing one id. Single writer, monotonic, never reused.
#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(CommunitySeqKey)]
#[ok_ns(38)]
#[ok_name("community_seq")]
pub struct CommunitySeqRow {
    #[ok_default(0)]
    pub high_water: u64,
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
    /// The fact was refused on its own terms (an unregistered `user`
    /// reference, or a `user` endpoint with no channel keyspace attached) —
    /// a caller error, never a storage failure.
    #[error("refused: {0}")]
    Refused(String),
}

/// A decoded fact, read back from storage.
#[derive(Clone, Debug, PartialEq)]
pub struct FactBody {
    pub edge_id: u64,
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
    User { channel: u64, member: u64 },
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
    pub fn user(self) -> Option<(u64, u64)> {
        match self {
            NodeId::User { channel, member } => Some((channel, member)),
            NodeId::Entity { .. } => None,
        }
    }

    /// `n12` / `u3_7` — what a rendered line prints and what a diagram uses as
    /// a node id (no punctuation a mermaid id would reject).
    pub fn label(self) -> String {
        match self {
            NodeId::Entity { node_id, .. } => format!("n{node_id}"),
            NodeId::User { channel, member } => format!("u{channel}_{member}"),
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
/// renderer needs (an edge id alone is not renderable).
#[derive(Clone, Debug, PartialEq)]
pub struct FactView {
    pub edge_id: u64,
    pub kind: String,
    pub src: ResolvedNode,
    pub dst: ResolvedNode,
    pub edge: Facts,
    /// The fact ids this fact POINTS AT (PLAN 4c): the reference links a writer
    /// left when the referenced fact is not central enough to sit in the head.
    /// One extra point read per shown fact — the price of the pointer being
    /// visible in the answer instead of discoverable by a second query.
    pub refs: Vec<u64>,
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

/// The prop name a fact uses to point at ANOTHER fact (PLAN 4c): one fact id, or
/// an array of them, in the same scope. It is a NAME the writer uses, not a
/// schema — a dangling id is not an error, it is a 404 the reader meets when it
/// chooses to follow (`fact_in_scope`). Nothing validates it on write: the reader
/// owns the decision to descend.
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
fn embedding_text(value: &str, props: &BTreeMap<String, DynamicValue>) -> String {
    let mut parts: Vec<&str> = vec![value];
    for name in [TITLE_FIELD, CONTENT_FIELD] {
        if let Some(DynamicValue::Str(text)) = props.get(name) {
            if !text.trim().is_empty() {
                parts.push(text.as_str());
            }
        }
    }
    parts.join("\n")
}

/// The ids in a `ref` value: one id or an array of ids. Anything else (a string,
/// a nested object) is not a reference and yields nothing — a writer's mistake
/// must not turn into a phantom target.
fn refs_of(value: &DynamicValue) -> Vec<u64> {
    match value {
        DynamicValue::UInt(id) => vec![*id],
        DynamicValue::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                DynamicValue::UInt(id) => Some(*id),
                _ => None,
            })
            .collect(),
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
    /// The identity collection (ns 42), on the CHANNEL keyspace — `None` when
    /// the caller attached only the memory side. Reads are point reads across
    /// handles; a fact's batch never spans the two keyspaces (okm's
    /// `commit_batch` writes into the handle it was called on), which is why
    /// this is a separate handle and not part of the graph's own tree.
    members: Option<Collection<FjallStore, MemberKey, MemberRow>>,
    /// One handle for the writes the collections cannot express in their
    /// own batch (the dictionary, and the edge's dynamic segment). The
    /// edge collection's own handle is built PER CALL — see `facts()`.
    store: FjallStore,
    next_node: u64,
    next_edge: u64,
    /// The embedding engine (ADR-0003). `None` = no engine attached:
    /// rows land with an EMPTY vector frame and retrieval has nothing to
    /// rank (a legal, empty result — never an error). WHICH engine (the built-in
    /// model, or a service the shell talks to) is behind the trait, not here.
    embedder: Option<Arc<dyn Embedder>>,
    /// Per-scope vector indexes, built from the KV on first use (ADR-0003).
    /// The scan is okm-vector's; this map only decides when to rebuild and how
    /// much derived state may be held — see `TABLE_BUDGET_BYTES`.
    tables: ScopeTables,
    /// Next community id per scope (ADR-0010 §1 — a fresh seed per landed
    /// node). The SEQUENCE row is the source of truth and only ever grows;
    /// this map is the in-process cache of it.
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
        edge_prefix[0..2].copy_from_slice(Facts::NS_PREFIX);
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
            members: None,
            tables: ScopeTables::default(),
            next_community: HashMap::new(),
        }
    }

    /// Attach the CHANNEL keyspace, which is where the identity collection
    /// lives: without it the graph cannot validate or render a `user`
    /// endpoint (ADR-0014), and an ingest that names one is refused.
    pub fn with_channel(mut self, channel_store: FjallStore) -> Self {
        self.members = Some(<MemberRow as Document>::collection(channel_store));
        self
    }

    /// Open both keyspaces of one database directory — the standalone form a
    /// test needs when it exercises user endpoints (a process opens the
    /// database once and attaches keyspaces; see `crate::service`).
    pub fn open_with_channel(path: &std::path::Path) -> Result<Self, fjall::Error> {
        let db = fjall::Database::create_or_recover(fjall::Config::new(path))?;
        let memory = FjallStore::from_db(db.clone(), crate::KEYSPACE_MEMORY)?;
        let channel = FjallStore::from_db(db, crate::KEYSPACE_CHANNEL)?;
        Ok(Self::wrap(memory).with_channel(channel))
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

    /// The entity's open attributes: the document's names the schema does not
    /// declare. `fetch` answers both halves (ADR-0035 — the document is one
    /// map), so the row's own fields are dropped here; `None` = no props, the
    /// same answer the retired segment-only read gave.
    pub fn entity_props(
        &mut self,
        scope: u64,
        node_id: u64,
    ) -> Option<BTreeMap<String, DynamicValue>> {
        let mut doc = self.entities.fetch(&EntityKey { scope, node_id })?;
        doc.retain(|name, _| !EntityRow::FIELDS.iter().any(|f| f.name == name));
        (!doc.is_empty()).then_some(doc)
    }

    /// One fact, read back by id.
    pub fn fact(&mut self, edge_id: u64) -> Option<FactBody> {
        let mut facts = self.facts();
        let body = facts.get_edge(edge_id)?;
        let kind = facts.predicate_vocab_name(body.predicate_vocab_id)?;
        Some(FactBody {
            edge_id,
            src: body.src,
            dst: body.dst,
            kind,
            edge: Facts::decode_payload(&body.payload),
        })
    }

    /// One fact, read back by id, with both endpoints resolved to their
    /// node rows (an absent endpoint renders as a node id and an empty
    /// name — never an error: the edge is truth even if its node row was
    /// removed).
    pub fn fact_view(&mut self, edge_id: u64) -> Option<FactView> {
        let body = self.facts().get_edge(edge_id)?;
        let kind = self.facts().predicate_vocab_name(body.predicate_vocab_id)?;
        let refs = self
            .fact_props(edge_id)
            .and_then(|props| props.get(REF_FIELD).map(refs_of))
            .unwrap_or_default();
        Some(FactView {
            edge_id,
            kind,
            src: self.resolve_ref(&body.src),
            dst: self.resolve_ref(&body.dst),
            edge: Facts::decode_payload(&body.payload),
            refs,
        })
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
            (
                <CommunitySeqRow as Document>::TABLE_NAME,
                crate::export::ns(CommunitySeqRow::NS_PREFIX),
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
    pub fn export(&mut self, name: &str, path: &std::path::Path) -> Option<Result<(), String>> {
        if name == <EntityRow as Document>::TABLE_NAME {
            Some(crate::export::table(&mut self.entities, path))
        } else if name == <Facts as Document>::TABLE_NAME {
            let mut facts = self.facts();
            let batch = facts.to_record_batch_table(crate::export::name_of);
            Some(crate::export::batch(&batch, path))
        } else if name == <CommunitySeqRow as Document>::TABLE_NAME {
            // An ISSUER row, not data: a restored graph without it re-issues
            // community ids the restored nodes already carry.
            let mut seq = <CommunitySeqRow as Document>::collection(self.store.clone());
            Some(crate::export::table(&mut seq, path))
        } else {
            None
        }
    }

    /// Restore one table by NAME from a Parquet file (the mirror of `export`).
    ///
    /// Every table lands through the SAME write contract the live path uses
    /// (`put` / `link_into`), so the derived faces and reduces follow; the
    /// ids, node ids and edge ids included, are the file's own. `None` = not
    /// this graph's table.
    pub fn import(&mut self, name: &str, path: &std::path::Path) -> Option<Result<usize, String>> {
        if name == <EntityRow as Document>::TABLE_NAME {
            let out = crate::export::import_table(&mut self.entities, path);
            Some(crate::export::finish_import(out, || self.reseed()))
        } else if name == <Facts as Document>::TABLE_NAME {
            let mut facts = self.facts();
            let out = facts.import_file(path, crate::export::ns_of);
            Some(crate::export::finish_import(out, || self.reseed()))
        } else if name == <CommunitySeqRow as Document>::TABLE_NAME {
            let mut seq = <CommunitySeqRow as Document>::collection(self.store.clone());
            let out = crate::export::import_table(&mut seq, path);
            // The cache is per scope and lazily read from the row; dropping it
            // is what makes a restored watermark visible.
            self.next_community.clear();
            Some(crate::export::finish_import(out, || {}))
        } else {
            None
        }
    }

    /// Recompute every in-process issuer from the store — the open path, reused
    /// after a restore wrote rows underneath. A restore that skips this hands
    /// out ids the restored rows already carry.
    pub fn reseed(&mut self) {
        let next_node = self
            .entities
            .scan_keys()
            .last()
            .map_or(1, |k| k.node_id.saturating_add(1));
        let mut pk_prefix = Vec::with_capacity(3);
        pk_prefix.extend_from_slice(<EntityRow as Document>::NS_PREFIX);
        write_slot(&mut pk_prefix, PRIMARY_SLOT);
        let mut edge_prefix = pk_prefix;
        edge_prefix[0..2].copy_from_slice(Facts::NS_PREFIX);
        let next_edge = self
            .store
            .scan_suffix(&edge_prefix)
            .iter()
            .last()
            .and_then(|sfx| sfx.as_slice().try_into().ok().map(u64::from_be_bytes))
            .map_or(1, |id| id.saturating_add(1));
        self.next_node = next_node;
        self.next_edge = next_edge;
        self.next_community.clear();
        self.tables.clear();
    }

    /// One fact by ITS ID — the deterministic read (ADR-0004's identity), the
    /// mirror of `ingest`'s `edge_id` answer: no search, no embedding, no
    /// ranking. The scope is checked against the edge's own source endpoint, so
    /// an id from another scope answers `None` rather than leaking.
    ///
    /// This is what a reference LINK resolves to: a fact can name a less central
    /// fact by id in its props, and the reader decides whether to follow it — the
    /// navigation shape skills use, one level down.
    pub fn fact_in_scope(&mut self, scope: u64, edge_id: u64) -> Option<FactView> {
        let body = self.facts().get_edge(edge_id)?;
        if EntityKey::decode(&body.src.pkey).scope != scope {
            return None;
        }
        self.fact_view(edge_id)
    }

    /// Withdraw one fact: its row and every derived face entry go in ONE batch
    /// (okm's `unlink` — the same path a functional rewrite takes for the
    /// incumbent). `false` = no such edge in this scope, so the caller can answer
    /// 404 without guessing.
    pub fn withdraw(&mut self, scope: u64, edge_id: u64) -> Result<bool, IngestError> {
        let Some(body) = self.facts().get_edge(edge_id) else {
            return Ok(false);
        };
        if EntityKey::decode(&body.src.pkey).scope != scope {
            return Ok(false);
        }
        self.facts().unlink(edge_id).map_err(IngestError::Storage)?;
        Ok(true)
    }

    /// Both directions out of one node — the symmetric neighbourhood. Cheap by
    /// construction: each side is a ref list on the node rows (okm's `out_edges`
    /// and `in_edges`), so the cost follows the NODE's degree, never the scope's
    /// size. This is the entity-anchored entrance (PLAN 4b): a caller holding an
    /// entity id gets its facts without a query, an embedding, or a landing.
    pub fn neighbours(&mut self, scope: u64, node_id: u64) -> Vec<FactView> {
        let node = self.node_ref(scope, node_id);
        let mut ids = self.facts().out_edges(&node);
        ids.extend(self.facts().in_edges(&node));
        self.views_of(ids)
    }

    /// One node's anchored memory (PLAN 4b): the facts that touch it, heaviest
    /// first, then the rest of its community — the same slice a recalled landing
    /// renders, entered by IDENTITY instead of by similarity. A node with no
    /// community yet (a fresh row) answers its own neighbourhood and nothing
    /// else: `community_id == 0` means "not binned", not "community zero".
    pub fn anchored(&mut self, scope: u64, node_id: u64) -> Option<Anchored> {
        let row = self.entity(scope, node_id)?;
        let own = self.neighbours(scope, node_id);
        let mut seen: HashSet<u64> = own.iter().map(|f| f.edge_id).collect();
        let mut facts = own;
        if row.community_id != 0 {
            let mut rest: Vec<FactView> = self
                .community_facts(scope, row.community_id)
                .into_iter()
                .filter(|view| seen.insert(view.edge_id))
                .collect();
            rest.sort_by(|a, b| {
                b.edge
                    .weight
                    .cmp(&a.edge.weight)
                    .then_with(|| a.edge_id.cmp(&b.edge_id))
            });
            facts.extend(rest);
        }
        Some(Anchored {
            node_id,
            community: row.community_id,
            facts,
        })
    }

    /// The scope's facts in the order they were LEARNED, newest first (PLAN 4b's
    /// temporal entrance).
    ///
    /// It walks the `by_since` face instead of the rows: one probe (empty — the
    /// whole face), reverse order, and only the rows it keeps are decoded and
    /// resolved. The face carries `since` + edge id, so the walk's order IS the
    /// answer's order, newest first, ties by edge id descending.
    ///
    /// The face is scope-blind, so the walk skips the keys of other scopes; that
    /// costs a row read per key until `limit` of this scope are found. A
    /// scope-prefixed time face (`by_scope_since`) would remove that, and it is
    /// the same declaration shape as `by_scope_weight` — worth it only when a
    /// scope carries enough facts for the skip to matter.
    pub fn timeline(&mut self, scope: u64, limit: usize) -> Vec<FactView> {
        if limit == 0 {
            return Vec::new();
        }
        let ids: Vec<u64> = {
            let facts = self.facts();
            okm_core::model::index::scan_index::<_, BySince>(facts.store(), Facts::NS_PREFIX, &[])
                .into_iter()
                .rev()
                .map(|key| key.decoded.0)
                .collect()
        };
        let mut out = Vec::new();
        for id in ids {
            let Some(fact) = self.fact_view(id) else {
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
        out
    }

    /// A causal chain (PLAN 4b): follow ONE predicate from `from`, hop by hop,
    /// breadth-first, cycle-safe by node id and bounded by `max_hops`. No search
    /// and no scoring — the order of the answer is the order of the walk, which is
    /// what makes "A leads to B leads to C" readable as a sequence rather than a
    /// pile of facts.
    pub fn chain(&mut self, scope: u64, from: u64, kind: &str, max_hops: usize) -> Vec<ChainHop> {
        let mut out: Vec<ChainHop> = Vec::new();
        let mut visited: HashSet<u64> = HashSet::new();
        visited.insert(from);
        let mut frontier = vec![from];
        for depth in 0..max_hops {
            let mut next: Vec<u64> = Vec::new();
            for node_id in frontier {
                let ids = self.typed_out(scope, node_id, kind);
                for edge_id in ids {
                    let Some(view) = self.fact_view(edge_id) else {
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
        out
    }

    /// Deduplicate edge ids and order them weight-descending, ties by edge id —
    /// the ordering every read that shows facts uses (ADR-0005).
    fn views_of(&mut self, ids: Vec<u64>) -> Vec<FactView> {
        let mut seen = HashSet::new();
        let mut facts: Vec<FactView> = ids
            .into_iter()
            .filter(|id| seen.insert(*id))
            .filter_map(|id| self.fact_view(id))
            .collect();
        facts.sort_by(|a, b| {
            b.edge
                .weight
                .cmp(&a.edge.weight)
                .then_with(|| a.edge_id.cmp(&b.edge_id))
        });
        facts
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
    pub fn top_by_weight(&mut self, scope: u64, k: usize) -> Vec<FactView> {
        if k == 0 {
            return Vec::new();
        }
        let ids: Vec<u64> = {
            let facts = self.facts();
            okm_core::model::index::scan_index::<_, ByScopeWeight>(
                facts.store(),
                Facts::NS_PREFIX,
                &scope.to_be_bytes(),
            )
            .into_iter()
            .rev()
            .take(k)
            .map(|key| key.decoded.0)
            .collect()
        };
        let mut facts: Vec<FactView> = ids
            .into_iter()
            .filter_map(|id| self.fact_view(id))
            .collect();
        // The face orders by (scope, weight, edge_id) ascending; reversed that is
        // weight-descending with the tie on edge id DESCENDING. The documented
        // order keeps ties ascending, so the k rows are put back in it here — k
        // rows, never the scope.
        facts.sort_by(|a, b| {
            b.edge
                .weight
                .cmp(&a.edge.weight)
                .then_with(|| a.edge_id.cmp(&b.edge_id))
        });
        facts
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
        self.load_vector_table(scope);
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
    /// seeds' communities are read as buckets (the `by_community` face plus
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
            if let Some(row) = self.entity(scope, seed.node_id) {
                if row.community_id != 0 && !communities.contains(&row.community_id) {
                    communities.push(row.community_id);
                }
            }
        }

        // One bucket per community, weight-descending inside each, every edge
        // once (the same edge is reachable from both endpoints).
        let mut seen = HashSet::new();
        let mut slices: Vec<CommunitySlice> = Vec::new();
        for &community in &communities {
            let members = self.community_members(scope, community).len();
            let mut facts: Vec<FactView> = self
                .community_facts(scope, community)
                .into_iter()
                .filter(|view| seen.insert(view.edge_id))
                .collect();
            facts.sort_by(|a, b| {
                b.edge
                    .weight
                    .cmp(&a.edge.weight)
                    .then_with(|| a.edge_id.cmp(&b.edge_id))
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
                    if let Some(served) = self.bump_view(view.edge_id, Touch::Read, now)? {
                        *view = served;
                    }
                }
                slice.facts.sort_by(|a, b| {
                    b.edge
                        .weight
                        .cmp(&a.edge.weight)
                        .then_with(|| a.edge_id.cmp(&b.edge_id))
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
        let Some(predicate) = facts.predicate_vocab_name(body.predicate_vocab_id) else {
            return Ok(None);
        };
        let edge = Facts::decode_payload(&body.payload).touched(kind, now);
        let head = EdgeHead {
            src: body.src.clone(),
            dst: body.dst.clone(),
            predicate: predicate.clone(),
        };
        let mut batch = Batch::default();
        facts
            .unlink_into(&mut batch, edge_id)
            .map_err(IngestError::Storage)?;
        facts
            .link_into(&mut batch, &head, &edge, edge_id)
            .map_err(IngestError::Storage)?;
        self.store
            .commit_batch(batch)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        // The dynamic segment is untouched by a rewrite, so the reference links a
        // writer left are still the ones this row carries.
        let refs = self
            .fact_props(edge_id)
            .and_then(|props| props.get(REF_FIELD).map(refs_of))
            .unwrap_or_default();
        Ok(Some(FactView {
            edge_id,
            kind: predicate,
            src: self.resolve_ref(&body.src),
            dst: self.resolve_ref(&body.dst),
            edge,
            refs,
        }))
    }

    /// Build (or refresh the LRU position of) one scope's table. Rows written
    /// without an engine carry an EMPTY frame and are skipped by construction.
    fn load_vector_table(&mut self, scope: u64) {
        if self.tables.contains(scope) {
            self.tables.touch(scope);
            return;
        }
        let rows = self.scope_entities(scope);
        // One dimension per scope: the first row with an engine-written frame
        // sets it (an empty frame is a legal vectorless row, and a width that
        // is not this scope's width cannot be ranked against the query).
        let dim = rows
            .iter()
            .find(|(_, row)| !row.embedding.is_empty())
            .map_or(0, |(_, row)| row.embedding.len());
        let mut index = ExactIndex::new(dim);
        let mut named = HashMap::new();
        let mut bytes = 0usize;
        if dim > 0 {
            for (node_id, row) in rows {
                if row.embedding.len() != dim {
                    continue;
                }
                let name_len = row.name.len();
                let key = node_id.to_be_bytes().to_vec();
                index.add(key.clone(), &row.embedding.elems);
                named.insert(
                    key,
                    (row.name, row.kind.as_str().unwrap_or_default().to_string()),
                );
                bytes += dim * std::mem::size_of::<f32>() + name_len;
            }
        }
        self.tables.insert(
            scope,
            ScopeTable {
                index,
                rows: named,
                bytes,
            },
        );
    }

    /// A `NodeRef` back to its node: the ref is `[ns 2B][len varint]
    /// [entity key]`, so the entity key decodes straight out of it and
    /// the scope rides along (nothing external is consulted).
    pub fn resolve_ref(&mut self, node: &NodeRef) -> ResolvedNode {
        if node.ns_id() == MEMBER_NS {
            let (channel, member) = member_from_pkey(&node.pkey).unwrap_or((0, 0));
            let name = self
                .members
                .as_ref()
                .and_then(|members| {
                    members
                        .get(&MemberKey {
                            channel_id: channel,
                            member_id: member,
                        })
                        .map(|row| row.display_name)
                })
                .unwrap_or_default();
            return ResolvedNode {
                id: NodeId::User { channel, member },
                kind: USER_KIND.to_string(),
                literal_num: 0,
                name,
            };
        }
        let key = EntityKey::decode(&node.pkey);
        let row = self.entity(key.scope, key.node_id);
        let (kind, literal_num, name) = match row {
            Some(row) => (
                row.kind.as_str().unwrap_or_default().to_string(),
                row.literal_num,
                row.name,
            ),
            None => (String::new(), 0, String::new()),
        };
        ResolvedNode {
            id: NodeId::Entity {
                scope: key.scope,
                node_id: key.node_id,
            },
            kind,
            literal_num,
            name,
        }
    }

    /// Outgoing edge ids of one node (the traversal currency).
    pub fn out_edges(&mut self, scope: u64, node_id: u64) -> Vec<u64> {
        self.facts().out_edges(&self.node_ref(scope, node_id))
    }

    /// Outgoing edge ids of ANY node reference — the form a user node needs:
    /// identity is not scoped to the corpus (ADR-0014).
    pub fn out_edges_of(&mut self, node: &NodeRef) -> Vec<u64> {
        self.facts().out_edges(node)
    }

    /// Incoming edge ids of any node reference; see
    /// [`out_edges_of`](Self::out_edges_of).
    pub fn in_edges_of(&mut self, node: &NodeRef) -> Vec<u64> {
        self.facts().in_edges(node)
    }

    /// Edge ids of one predicate out of any node reference (the 0x09 face).
    pub fn typed_out_of(&mut self, node: &NodeRef, kind: &str) -> Vec<u64> {
        self.facts().typed_out(kind, node)
    }

    /// One member's node reference (`[42][channel][member]`) — the entry a
    /// context head starts its traversal from.
    pub fn user_ref(&self, channel: u64, member: u64) -> NodeRef {
        NodeRef::new(
            <MemberRow as Document>::NS_PREFIX,
            &MemberKey {
                channel_id: channel,
                member_id: member,
            }
            .encode(),
        )
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
        self.facts().typed_out(kind, &self.node_ref(scope, node_id))
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
            if let Some(&incumbent) = self
                .typed_out_of(&src.to_ref(scope), &fact.rel.name)
                .first()
            {
                let body = self
                    .facts()
                    .get_edge(incumbent)
                    .ok_or_else(|| IngestError::Storage(format!("edge {incumbent} unreadable")))?;
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
                    // Rewrite in place: same edge_id, new endpoints and
                    // attributes, in ONE engine batch (okm's `unlink_into` +
                    // `link_into` — ADR-0010's owed item, paid 2026-10-19).
                    // The old props entry is deleted in the same batch, so a
                    // rewrite with no props leaves none behind.
                    let mut batch = Batch::default();
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
                    self.bin(scope, &[src, dst])?;
                    return Ok(IngestOutcome::Rewritten { edge_id: incumbent });
                }
                return Ok(IngestOutcome::Dropped { edge_id: incumbent });
            }
        }
        let edge = Facts {
            scope,
            since,
            confidence,
            ..Default::default()
        }
        .touched(Touch::Write, since);
        let edge_id = self.next_edge;
        self.next_edge += 1;
        self.write_edge(edge_id, &edge, scope, src, dst, &fact.rel.name, &props)?;
        self.bin(scope, &[src, dst])?;
        Ok(IngestOutcome::Linked { edge_id })
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
            let (channel, member) = parse_member_ref(&term.value).ok_or_else(|| {
                IngestError::Refused(format!(
                    "user reference `{}` is not member-{{channel}}-{{member}}",
                    term.value
                ))
            })?;
            let known = self
                .members
                .as_ref()
                .ok_or_else(|| {
                    IngestError::Refused(
                        "a `user` endpoint needs the channel keyspace attached".to_string(),
                    )
                })?
                .get(&MemberKey {
                    channel_id: channel,
                    member_id: member,
                })
                .is_some();
            if !known {
                return Err(IngestError::Refused(format!(
                    "`{}` is not a registered member",
                    term.value
                )));
            }
            return Ok(Endpoint::User { channel, member });
        }
        if let Some((node_id, _)) = self.entity_by_name(scope, &term.value) {
            self.merge_props(scope, node_id, &term.props);
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
        let community_id = self.next_community_id(scope);
        let row = EntityRow {
            kind: Vocab::of(term.type_label.clone()),
            literal_num: literal_num(term),
            community_id,
            name: term.value.clone(),
            embedding,
        };
        let key = EntityKey { scope, node_id };
        self.entities.put(&key, &row);
        // The seeded id is durable before anything can read it back: one
        // small row, written through rather than batched.
        self.persist_community_seq(scope);
        // The scope's in-memory vector table is derived state: drop it rather
        // than maintain it row by row (it reloads on the next retrieval).
        self.tables.forget(scope);
        if !term.props.is_empty() {
            // Non-declared names: `merge` writes the segment, the row stays as
            // written above.
            self.entities.merge(&key, &term.props);
        }
        Ok(Endpoint::Entity(node_id))
    }

    /// Merge props into an existing node's dynamic segment.
    fn merge_props(&mut self, scope: u64, node_id: u64, props: &BTreeMap<String, DynamicValue>) {
        if props.is_empty() {
            return;
        }
        // `merge` IS the delta write: it reads the stored segment, overwrites
        // the names given and leaves the rest — so the read-modify-write that
        // used to live here is the verb's own work now.
        self.entities.merge(&EntityKey { scope, node_id }, props);
    }

    /// Write one edge and its open props in ONE engine batch (ADR-0002:
    /// one WriteBatch = one transaction — kind allocation is the only
    /// thing okm commits separately, and an extra dictionary entry is
    /// harmless by its own rule).
    #[allow(clippy::too_many_arguments)]
    fn write_edge(
        &mut self,
        edge_id: u64,
        edge: &Facts,
        scope: u64,
        src: Endpoint,
        dst: Endpoint,
        kind: &str,
        props: &BTreeMap<String, DynamicValue>,
    ) -> Result<(), IngestError> {
        let mut batch = Batch::default();
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
        batch: &mut Batch,
        edge_id: u64,
        edge: &Facts,
        scope: u64,
        src: Endpoint,
        dst: Endpoint,
        kind: &str,
        props: &BTreeMap<String, DynamicValue>,
    ) -> Result<(), IngestError> {
        let head = EdgeHead {
            src: src.to_ref(scope),
            dst: dst.to_ref(scope),
            predicate: kind.to_string(),
        };
        let mut facts = self.facts();
        facts
            .link_into(batch, &head, edge, edge_id)
            .map_err(IngestError::Storage)?;
        // The edge's open attributes (okm ADR-0034 §4): the 0x01 segment is the
        // GRAPH's, written in the same batch as the row so one commit carries
        // both. It deletes the previous assertion's segment first — props belong
        // to the claim being written, never to the one it replaces.
        facts.replace_into(batch, edge_id, props);
        Ok(())
    }

    /// The fact's open props (its dynamic segment), when it has one — the
    /// graph's own read (okm ADR-0034 §4), not a second decoder beside it.
    pub fn fact_props(&self, edge_id: u64) -> Option<BTreeMap<String, DynamicValue>> {
        self.facts().attrs(edge_id)
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
    /// The sequence row's handle.
    fn seq(&self) -> Collection<FjallStore, CommunitySeqKey, CommunitySeqRow> {
        <CommunitySeqRow as Document>::collection(self.store.clone())
    }

    /// The next community id of one scope.
    ///
    /// The sequence row is the source of truth and it only ever grows, so an
    /// id is never reused — not even when a community's last member leaves,
    /// which is what reading "the maximum that exists" would get wrong. The
    /// in-memory copy is a cache; [`persist_community_seq`](Self::persist_community_seq)
    /// (or the batch variant) makes it durable.
    ///
    /// A corpus written before the sequence row existed is bootstrapped from
    /// the highest community id any of its nodes carries, so the first
    /// post-upgrade ingest cannot collide with a live bin.
    fn next_community_id(&mut self, scope: u64) -> u64 {
        if !self.next_community.contains_key(&scope) {
            let stored = self
                .seq()
                .get(&CommunitySeqKey { scope })
                .map_or(0, |row| row.high_water);
            let live = self
                .scope_entities(scope)
                .into_iter()
                .map(|(_, row)| row.community_id)
                .max()
                .unwrap_or(0);
            self.next_community.insert(scope, stored.max(live) + 1);
        }
        // Issuing ADVANCES the watermark — a cached "next" that did not move
        // hands the same id to every node landed after it, which silently
        // puts unrelated seeds in one community.
        let next = self.next_community.get_mut(&scope).expect("just seeded");
        let id = *next;
        *next = id.saturating_add(1);
        id
    }

    /// Flush the scope's issued high water on its own (the single-row write
    /// path: a node landing before any batch exists).
    fn persist_community_seq(&self, scope: u64) {
        let Some(next) = self.next_community.get(&scope) else {
            return;
        };
        self.seq().put(
            &CommunitySeqKey { scope },
            &CommunitySeqRow {
                high_water: next.saturating_sub(1),
            },
        );
    }

    /// The same flush into a caller-owned batch, so the ids and the rows
    /// that carry them commit together.
    fn write_community_seq_into(&self, batch: &mut Batch, scope: u64) {
        let Some(next) = self.next_community.get(&scope) else {
            return;
        };
        let key = CommunitySeqKey { scope };
        let mut pkey = Vec::with_capacity(3 + 8);
        pkey.extend_from_slice(CommunitySeqRow::NS_PREFIX);
        write_slot(&mut pkey, PRIMARY_SLOT);
        pkey.extend_from_slice(&key.encode());
        let row = CommunitySeqRow {
            high_water: next.saturating_sub(1),
        };
        batch.put(pkey, row.encode_payload());
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
        batch: &mut Batch,
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

    /// ADR-0010 §2's binning pass, run after a fact lands: load the 2-hop
    /// neighbourhood, run incremental LPA over it, and write the winning
    /// community on every node that moved. One engine batch covers the pass
    /// (the sequence row included). Nothing here is a summary — the read
    /// side renders the community when it is read (ADR-0010 §5).
    ///
    /// No LLM, no transport, no global graph: the vote is local and the
    /// weights are ADR-0005's — a fresh assertion weighs `ln(2)×0.5`, so a
    /// vote never degenerates to an all-zero tie.
    fn bin(&mut self, scope: u64, touched: &[Endpoint]) -> Result<(), IngestError> {
        // A user node never enters a community (it is not corpus knowledge):
        // the bin pass takes the entity endpoints of the fact and nothing else.
        let touched: Vec<u64> = touched.iter().filter_map(|e| e.entity_id()).collect();
        let local = self.local_subgraph(scope, &touched)?;
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
                // A fact may name a user endpoint; only entity endpoints take
                // part in the vote, so a ref from another collection decodes
                // to `None` rather than to a bogus node id.
                let (Some(src), Some(dst)) = (entity_node_of(&body.src), entity_node_of(&body.dst))
                else {
                    continue;
                };
                let other = if src == node { dst } else { src };
                if other == node || !members.contains(&other) {
                    continue;
                }
                let edge = Facts::decode_payload(&body.payload);
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

        let mut batch = Batch::default();
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

        // The ids this pass issued must be durable WITH the rows that use
        // them: the sequence row rides the same batch, so a restart can
        // never hand the same id to another community.
        self.write_community_seq_into(&mut batch, scope);
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
    /// resolved, each edge once — the read side's bucket (ADR-0010 §5).
    pub fn community_facts(&mut self, scope: u64, community: u64) -> Vec<FactView> {
        let in_community: HashSet<u64> = self
            .community_members(scope, community)
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        let mut seen = HashSet::new();
        let mut facts = Vec::new();
        for &node in &in_community {
            for view in self.out_facts(scope, node) {
                if view
                    .dst
                    .id
                    .entity_id()
                    .is_some_and(|id| in_community.contains(&id))
                    && seen.insert(view.edge_id)
                {
                    facts.push(view);
                }
            }
        }
        facts
    }
}
