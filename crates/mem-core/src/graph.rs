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
//!   node-kind face). A literal's numeric form is a declared FIELD, so a
//!   numeric read stays an ordered key scan rather than vector distance;
//!   the face over it lands with the read that scans it.
//! - **Facts** = an okm edge collection (ns 37, one ns per graph): the
//!   edge body carries the self-describing `NodeRef` endpoints plus
//!   `since` / `confidence` / `weight` as declared fields (ADR-0010 §1's
//!   three algorithm-queried fields, one 0x1 face each). Endpoints need
//!   no declaration — a `NodeRef` carries its own pkey width.
//! - **The relation's open props** (condition / cause / strength / the
//!   per-assertion `functional` flag) ride the edge's dynamic segment
//!   (slot 1 of the edge's ns), built by `put_frame_named` — the
//!   `put_fields`-style caller graph.rs's own docs anticipate.
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
//! `by_community` face, which are NOT declared until something reads
//! them) and the weight system's writer (ADR-0005, Phase 5).

use okm_core::obj_dict::DictCache;
use okm_core::obj_dynamic::{put_frame_named, DynamicValue};
use okm_core::{
    Collection, Document, DocumentEncode, EdgeEncode, EdgeFact, FjallStore, Graph, KeyEncode,
    KvGraph, MemBatch, NodeRef, VirtualStorage, PRIMARY_SLOT,
};
use std::collections::BTreeMap;

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
/// Dynamic-segment slot 1 (the obj's dynamic entry, okm ADR-0012) —
/// the edge's own open props live here, beside the graph's 0x0–0x8
/// entry kinds.
const FIELDS_SLOT: u16 = 0x0001;

/// Entity identity: `[scope][node_id]`. `scope` leads because the corpus
/// is per-user (ADR-0003) — the ownership boundary flat memory shares.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(36)]
pub struct EntityKey {
    pub scope: u64,
    pub node_id: u64,
}

/// One entity node. `kind_id` allocates through this collection's own
/// dictionary (the 0x2/0x3 vocabulary — the same one dynamic field names
/// use; kind tokens and prop names share it, which is harmless: an id is
/// only ever read back through the field it was written into).
///
/// `name` is an ATTRIBUTE (never key material): the human identity reaches
/// the exact entrance through the `by_name` face. `literal_num` is a
/// literal node's integer projection (0 for an ordinary entity) — a
/// declared, fixed-width, ordered field, so an interval or nearest read
/// stays a `scan_range` over a face rather than a vector-distance guess.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(EntityKey)]
#[ok_ns(36)]
#[ok_index(
    // Payload fields only: the key tail appends the primary key, so the
    // entries land as `[kind_id][scope][node_id]` and
    // `[name][scope][node_id]`. Text-first regime: an exact match
    // re-verifies the row's name and the decoded key's scope — a prefix
    // scan for `alpha` also reaches `alphabet`, which is what dictionary
    // order means.
    by_kind { fields(kind_id) },
    by_name { fields(name) },
)]
pub struct EntityRow {
    pub kind_id: u16,
    /// A literal node's integer projection (0 for an ordinary entity): a
    /// declared, fixed-width, ordered field, so a numeric read later stays
    /// a range scan rather than a vector-distance guess. No face is
    /// declared over it yet — the face lands with the read that scans it.
    #[ok_default(0)]
    pub literal_num: i64,
    pub name: String,
}

use __OkmIndex_EntityRow_by_name as ByName;

/// A fact: an edge of the graph. The three declared fields are ADR-0010
/// §1's algorithm-queried ones — `since` (the caller's clock, the
/// adjudication input), `confidence` (the write-side's, defaulting to full
/// confidence so a fact that states none is not penalized) and `weight`
/// (the weight system's, ADR-0005 — zero until that system writes it).
#[derive(Clone, Debug, Default, PartialEq, EdgeEncode)]
#[ok_edge(ns = 37)]
pub struct FactEdge {
    #[ok_default(0)]
    pub since: u64,
    #[ok_default(255)]
    pub confidence: u8,
    #[ok_default(0)]
    pub weight: u32,
}

impl FactEdge {
    /// The declared-attribute payload width: `[since u64][confidence u8]
    /// [weight u32]` — the derive's own encoding order, contiguous
    /// big-endian (okm's fixed-width field encoding; `attrs()` is the
    /// writer half, so this reader is locked against it by a test).
    pub const ATTRS_LEN: usize = 8 + 1 + 4;

    /// Decode the edge body's declared attributes (the adjudication input:
    /// an incumbent's `since` / `confidence` are what a new fact is
    /// compared against).
    pub fn from_attrs(attrs: &[u8]) -> Option<Self> {
        if attrs.len() < Self::ATTRS_LEN {
            return None;
        }
        Some(Self {
            since: u64::from_be_bytes(attrs[0..8].try_into().ok()?),
            confidence: attrs[8],
            weight: u32::from_be_bytes(attrs[9..13].try_into().ok()?),
        })
    }
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

/// The relation: its name IS its kind (the edge collection's dictionary
/// allocates it), so it carries no type label; its props are the
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
    let object = value.as_object().ok_or(IngestError::Slot {
        slot,
        why: "expected an object (empty props are `{}`)",
    })?;
    Ok(object
        .iter()
        .map(|(k, v)| (k.clone(), dynamic_value(v)))
        .collect())
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

/// JSON → okm's dynamic value. Objects recurse (the dictionary resolves
/// nested names through the same vocabulary).
fn dynamic_value(value: &serde_json::Value) -> DynamicValue {
    match value {
        serde_json::Value::Null => DynamicValue::Null,
        serde_json::Value::Bool(b) => DynamicValue::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) if i < 0 => DynamicValue::Int(i),
            Some(i) => DynamicValue::UInt(i as u64),
            None => DynamicValue::F64(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => DynamicValue::Str(s.clone()),
        serde_json::Value::Array(a) => DynamicValue::Array(a.iter().map(dynamic_value).collect()),
        serde_json::Value::Object(o) => DynamicValue::Obj(
            o.iter()
                .map(|(k, v)| (k.clone(), dynamic_value(v)))
                .collect(),
        ),
    }
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

/// The graph's tables, attached to one already-open store handle (the
/// `memory` keyspace: different entity class, different ns, one tree —
/// the same rule flat memory and the code store follow).
pub struct GraphStore {
    entities: Collection<FjallStore, EntityKey, EntityRow>,
    /// One handle for the writes the collections cannot express in their
    /// own batch (the dictionary, and the edge's dynamic segment). The
    /// edge collection's own handle is built PER CALL — see `facts()`.
    store: FjallStore,
    next_node: u64,
    next_edge: u64,
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
        let mut pk_prefix = Vec::with_capacity(4);
        pk_prefix.extend_from_slice(<EntityRow as Document>::NS_PREFIX);
        pk_prefix.extend_from_slice(&PRIMARY_SLOT.to_be_bytes());
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
        }
    }

    /// The edge collection, built PER CALL. okm's `Graph` caches the ns's
    /// kind dictionary inside the handle and allocates from that cache;
    /// k10r writes a SECOND vocabulary into the same ns (the edge's open
    /// prop names), so a long-lived handle would hand out an id that
    /// already names another entry — the frame then reads back under the
    /// wrong name. A fresh handle loads the dictionary from the engine on
    /// first use, and never outlives one call (the vocabulary is tiny and
    /// its ids only grow). The owed okm-side item this stands in for: a
    /// dictionary handle the collection exposes, or a rule that one ns
    /// holds one allocator.
    fn facts(&self) -> Graph<FjallStore, FactEdge> {
        Graph::new(self.store.clone())
    }

    /// A per-call edge handle with its kind dictionary LOADED. okm's
    /// `Graph` loads lazily and answers typed queries through the
    /// non-allocating `by_name_get` (no engine fallback), so a brand-new
    /// handle reports "unknown kind" until something touches the
    /// dictionary. `kind_name` is that touch: it loads the whole
    /// vocabulary, allocates nothing, and its result is discarded. The
    /// owed okm-side item: let `typed_*` / `edges_of_kind` fall back to
    /// the engine the way `name_for` already does, or expose the load.
    fn facts_primed(&self) -> Graph<FjallStore, FactEdge> {
        let mut facts = self.facts();
        let _ = facts.kind_name(0);
        facts
    }

    /// Allocate (or look up) a name in one ns's dictionary — with a FRESH
    /// cache per call, never a long-lived one. Two writers allocate in the
    /// same ns dictionary (this path for node kinds, okm's own internal
    /// caches inside `put_fields` / `Graph::link` for field and kind
    /// names), and a cache loaded before the other writer's allocation
    /// would hand out an id that already names another entry: the frame
    /// then reads back under the WRONG name, silently. Loading is one scan
    /// of a tiny vocabulary, and the ids only grow.
    fn dict_id(&mut self, ns_prefix: &[u8], name: &str) -> u16 {
        let mut dict = DictCache::default();
        dict.id_for(&mut self.store, ns_prefix, name)
    }

    /// The reverse direction, same discipline.
    fn dict_name(&self, ns_prefix: &[u8], id: u16) -> Option<String> {
        let mut dict = DictCache::default();
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
        let kind = facts.kind_name(body.kind_id)?;
        Some(FactBody {
            edge_id,
            src: body.src,
            dst: body.dst,
            kind,
            edge: FactEdge::from_attrs(&body.attrs).unwrap_or_default(),
        })
    }

    /// One fact, read back by id, with both endpoints resolved to their
    /// node rows (an absent endpoint renders as a node id and an empty
    /// name — never an error: the edge is truth even if its node row was
    /// removed).
    pub fn fact_view(&mut self, edge_id: u64) -> Option<FactView> {
        let body = self.facts().get_edge(edge_id)?;
        let kind = self.facts_primed().kind_name(body.kind_id)?;
        Some(FactView {
            edge_id,
            kind,
            src: self.resolve_ref(&body.src),
            dst: self.resolve_ref(&body.dst),
            edge: FactEdge::from_attrs(&body.attrs).unwrap_or_default(),
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

    /// Outgoing edge ids of one kind out of one node — the 0x7 face, and
    /// the resolution path's incumbent scan.
    pub fn typed_out(&mut self, scope: u64, node_id: u64, kind: &str) -> Vec<u64> {
        self.facts_primed()
            .typed_out(kind, &self.node_ref(scope, node_id))
    }

    /// Land one fact (ADR-0010 §1 steps 1–2): resolve both endpoints to
    /// nodes, then link the fact or adjudicate it against an incumbent.
    pub fn ingest(&mut self, scope: u64, fact: &FactTuple) -> Result<IngestOutcome, IngestError> {
        let src = self.resolve(scope, &fact.head);
        let dst = self.resolve(scope, &fact.tail);
        let functional = matches!(
            fact.rel.props.get(FUNCTIONAL_PROP),
            Some(DynamicValue::Bool(true))
        );
        let edge = FactEdge {
            since: match fact.rel.props.get("since") {
                Some(DynamicValue::UInt(v)) => *v,
                Some(DynamicValue::Int(v)) => *v as u64,
                _ => 0,
            },
            confidence: match fact.rel.props.get("confidence") {
                Some(DynamicValue::UInt(v)) => (*v).min(255) as u8,
                Some(DynamicValue::Int(v)) => (*v).clamp(0, 255) as u8,
                _ => 255,
            },
            weight: 0,
        };
        // The rel props that ride the edge's dynamic segment: the open
        // modifiers. The three declared fields are projections of their
        // props — kept out of the dynamic entry so one datum is not
        // stored twice; `functional` is the caller's declaration, the
        // adjudication input, and is kept (the assertion's own record).
        let props: BTreeMap<String, DynamicValue> = fact
            .rel
            .props
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "since" | "confidence" | "weight"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        if functional {
            if let Some(&incumbent) = self.typed_out(scope, src, &fact.rel.name).first() {
                let body = self
                    .facts()
                    .get_edge(incumbent)
                    .ok_or_else(|| IngestError::Storage(format!("edge {incumbent} unreadable")))?;
                let old = FactEdge::from_attrs(&body.attrs).unwrap_or_default();
                if edge.since > old.since || edge.confidence >= old.confidence {
                    // Rewrite in place: same edge_id, new endpoints and
                    // attributes. Unlink then link — two engine commits,
                    // because okm's `unlink` has no into-batch form (the
                    // atomicity gap is recorded in ADR-0010's owed list;
                    // a crash between them is recovered by re-ingesting).
                    self.facts()
                        .unlink(incumbent)
                        .map_err(IngestError::Storage)?;
                    self.write_edge(incumbent, &edge, scope, src, dst, &fact.rel.name, &props)?;
                    return Ok(IngestOutcome::Rewritten { edge_id: incumbent });
                }
                return Ok(IngestOutcome::Dropped { edge_id: incumbent });
            }
        }
        let edge_id = self.next_edge;
        self.next_edge += 1;
        self.write_edge(edge_id, &edge, scope, src, dst, &fact.rel.name, &props)?;
        Ok(IngestOutcome::Linked { edge_id })
    }

    /// Resolve a term to a node id: the exact name entrance first, then a
    /// new node (kind allocated through the collection's dictionary).
    /// Props land on the node row — an entity's attributes are shared by
    /// every fact about it, so they MERGE into the node's dynamic
    /// segment (a same-named attribute takes the newer value; the rest
    /// survive, unlike a wholesale replace).
    fn resolve(&mut self, scope: u64, term: &Term) -> u64 {
        if let Some((node_id, _)) = self.entity_by_name(scope, &term.value) {
            self.merge_props(scope, node_id, &term.props);
            return node_id;
        }
        let kind_id = self.dict_id(<EntityRow as Document>::NS_PREFIX, &term.type_label);
        let node_id = self.next_node;
        self.next_node += 1;
        let row = EntityRow {
            kind_id,
            literal_num: literal_num(term),
            name: term.value.clone(),
        };
        let key = EntityKey { scope, node_id };
        self.entities.put(&key, &row);
        if !term.props.is_empty() {
            self.entities.put_fields(&key, &term.props);
        }
        node_id
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
        let fact = EdgeFact {
            src: self.node_ref(scope, src),
            dst: self.node_ref(scope, dst),
            kind: kind.to_string(),
            attrs: edge.attrs(),
        };
        let mut batch = MemBatch::default();
        let mut facts = self.facts();
        facts
            .link_into(&mut batch, &fact, edge, edge_id)
            .map_err(IngestError::Storage)?;
        if !props.is_empty() {
            let mut body = Vec::new();
            // One fresh cache for this write's prop names, AFTER `link_into`
            // committed the kind name: the two allocations then share one
            // accurate view of the ns vocabulary.
            let mut dict = DictCache::default();
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
        self.store
            .commit_batch(batch)
            .map_err(|e| IngestError::Storage(e.to_string()))
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

    /// A dynamic-segment key: `[ns 2B][slot 1 2B][key payload]`.
    fn fields_key(&self, ns_prefix: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut key = Vec::with_capacity(4 + payload.len());
        key.extend_from_slice(ns_prefix);
        key.extend_from_slice(&FIELDS_SLOT.to_be_bytes());
        key.extend_from_slice(payload);
        key
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

/// Decode a dynamic-segment entry to its name-keyed values (the field-name
/// dictionary resolves the ids).
fn decode_props(
    raw: &[u8],
    store: &FjallStore,
    ns_prefix: &[u8],
) -> Option<BTreeMap<String, DynamicValue>> {
    let mut dict = DictCache::default();
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
