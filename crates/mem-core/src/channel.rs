//! The unified channel log + the agent's projection (ADR-0008, minimal
//! Phase 2 slice serving gravity Phase 0): a message log keyed
//! `(channel_id, turn_id, ts_ms, sub)` with gravity's clock as the order
//! authority, plus the per-member checkpoint (the compression record).
//!
//! The TURN axis is ADR-0024: the store mints a turn id when a turn opens,
//! every round is written as the turn runs, and the turn closes with one
//! `kind::FINAL` record that names the rows to keep — the rest of the turn
//! is purged in the same write. A turn with rows and no closing record is
//! OPEN: it is working state, and the projection leaves it out.
//!
//! Phase 2.6 adds this group's two pieces of compression STATE — the
//! member's read cursor (the cache clock's only observable, ADR-0008 §2)
//! and the decision log (what the engine proposed, and whether the
//! caller's summary ever landed). Both are per-member projection state,
//! so they sit beside the checkpoint: same keyspace, because a summary
//! landing and its decision closing must commit in ONE batch.
//!
//! Deliberately NOT here yet (later Phase 2 slices, ADR-0008/0009):
//! threads (sub-channels), topic-tag index, and the members table.
//!
//! Ordering (ADR-0008 §3): timestamps are assigned by GRAVITY at write
//! time — a reply provably postdates what it answers (the replier saw
//! it), so a physical clock yields a causally consistent total order;
//! a shared seq has no such property. `sub` breaks ties within one
//! millisecond (single writer, sequential appends — one u16 counter on
//! the caller side; u16 overflows at 65 536 messages in one ms, which
//! is a pathological signal, not a re-keying need). KeyEncode's BE
//! layout makes byte order == key order, and the key's field order is
//! what that order MEANS: turn creation first, then the record's own
//! time inside that turn, so a prefix scan over `channel_id` returns the
//! log in conversation order with no sort step (ADR-0024).

use okm_core::{Document, DocumentEncode, FjallStore, KeyEncode, Uuid7};

/// Append-only message log (ns 33 — the channel-log band, next to flat
/// memory's 32; one entity class = one ns, per MODELING).
///
/// `turn_id` sits after `channel_id` because a turn is one contiguous prefix
/// range of the log (ADR-0024): reading a turn, closing a turn and the
/// compression span are each one prefix scan in the engine's own order.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(33)]
pub struct MessageKey {
    pub channel_id: u64,
    pub turn_id: u64,
    pub ts_ms: u64,
    pub sub: u16,
}

/// A position in the log: a record's key minus the channel (ADR-0024).
///
/// It is a RECORD, not a millisecond — two records can share a millisecond, and
/// after ADR-0024 two records of different turns can share both `ts_ms` and
/// `sub`. Comparisons are field-wise in key order, which is exactly the order
/// the increment walks in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Pointer {
    pub turn_id: u64,
    pub ts_ms: u64,
    pub sub: u16,
}

impl Pointer {
    /// The position of `(0, 0, 0)`: before every record of a channel. What "no
    /// checkpoint yet" means for a coverage point.
    pub const NONE: Pointer = Pointer {
        turn_id: 0,
        ts_ms: 0,
        sub: 0,
    };

    /// The position a log key names.
    pub fn of(key: &MessageKey) -> Self {
        Self {
            turn_id: key.turn_id,
            ts_ms: key.ts_ms,
            sub: key.sub,
        }
    }

    /// The position a checkpoint key names — the record that compression's
    /// summary ends at.
    pub fn of_checkpoint(key: &CheckpointKey) -> Self {
        Self {
            turn_id: key.turn_id,
            ts_ms: key.ts_ms,
            sub: key.sub,
        }
    }
}

/// One log entry. `role` is the speaker class (the projection maps it to
/// LLM message roles); `kind` separates plain text from future record
/// shapes (tool_call / tool_result) — the VIEW prunes those, storage
/// always holds full rows (ADR-0006's view-layer pruning ruling).
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MessageKey)]
#[ok_ns(33)]
#[ok_name("messages")]
pub struct MessageRow {
    /// Who said it: the member id — which IS the user id (ADR-0025 §4), a `Uuid7`. ALWAYS set:
    /// a single-user channel still has its user, seeded as the default member (ADR-0009 §1).
    pub sender: Uuid7,
    pub role: u8,
    pub kind: u8,
    /// The record's END: the completion clock. `0` = instant (a user message
    /// is written when it was sent). The agent's row carries its START as the
    /// key's `ts_ms` — so a user message typed DURING a generation sorts before
    /// the reply while the reply still says when it finished.
    pub end_ts_ms: u64,
    pub content: String,
}

/// A checkpoint NODE (ns 34) — layer 1 of the channel, isomorphic to the log
/// below it (ADR-0008 §2). The KEY is a pointer into layer 0: the record's own
/// `(turn_id, ts_ms, sub)`. The payload is the summary of the span ending at
/// that record, plus (dynamic segment) the head frozen at that compression.
///
/// The turn is IN the key (ADR-0024 §7): the pointer has to name one record, and
/// two turns can produce records that share both a millisecond and a `sub`. Key
/// order then runs along the coverage axis, which is monotone — a compression
/// always advances past the previous node, so "newest node = largest key" still
/// holds and the same-millisecond pair can never overwrite each other.
///
/// Nodes accumulate; there is no "current row" to overwrite. The state is "the
/// newest node that carries a summary", exactly as the channel's state is its
/// last record. A node with an EMPTY summary is a compression that was opened
/// and has not landed: it marks its position and nothing else. Above this layer
/// is not another layer — it is the fact graph, and this node holds both ends
/// (pointer down, frozen edge ids up): it IS the seam.
#[derive(KeyEncode, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[ok_ns(34)]
pub struct CheckpointKey {
    pub channel_id: u64,
    pub turn_id: u64,
    pub ts_ms: u64,
    pub sub: u16,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(CheckpointKey)]
#[ok_ns(34)]
#[ok_name("checkpoints")]
pub struct CheckpointRow {
    /// Empty = the compression is in flight (a second pass treats the span as
    /// taken); otherwise the summary of everything up to this pointer.
    pub summary: String,
    /// The cut the compression pass reported as BETTER than the mechanical one,
    /// as a pointer into the same layer (`(0, 0)` = none reported).
    ///
    /// It changes NOTHING about this node's coverage: the summary still covers
    /// up to this node's key and the increment is still cut there. It is a
    /// LABEL, and it answers two later questions — (a) training data, a
    /// mechanical cut beside the cut a model would choose over the same span;
    /// (b) the range a tail operation reads, so an analysis pass starts where a
    /// model saw the conversation break rather than where a byte budget did.
    ///
    /// Two segments because a position in the log is a RECORD, not a
    /// millisecond, and after ADR-0024 the turn comes before the millisecond:
    /// without it a suggestion made in turn 3 would read as a position inside
    /// turn 5 when both wrote in the same millisecond.
    #[ok_default(0)]
    pub suggested_turn_id: u64,
    #[ok_default(0)]
    pub suggested_ts: u64,
    #[ok_default(0)]
    pub suggested_sub: u16,
}

/// A span of the log, as the checkpoint layer states it: `start` is EXCLUSIVE
/// (what the previous summary already covers) and `end` is INCLUSIVE (the record
/// the span closes at). Two pointers, because a position in the log is a record
/// — see [`CheckpointRow::suggested_ts`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Span {
    pub start: Pointer,
    pub end: Pointer,
}

impl Span {
    /// A span that covers nothing is not a span: both ends must advance.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// The member's read cursor `(channel, member) → last-read ts`
/// (ADR-0008 §2). It is the cache clock's only observable: the prompt
/// cache is idle exactly while that member's projection has not been
/// read, so "how long since this ts" is the signal §4 spends.
///
/// Latest-wins, and it never rewinds: a repeated or out-of-order read
/// with an older ts leaves the stored one alone.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(39)]
pub struct CursorKey {
    pub channel_id: u64,
    pub member_id: Uuid7,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(CursorKey)]
#[ok_ns(39)]
#[ok_name("cursors")]
pub struct CursorRow {
    /// The CALLER's clock at the read that stamped this row (ADR-0008 §3).
    ///
    /// Only the clock. The model's read needs no stored POSITION: the projection
    /// always answers up to the newest record, so "how far I read" is the log's
    /// own end, and the checkpoint (not a cursor) is what cuts the increment
    /// (ADR-0008 §2).
    pub read_at: u64,
}

/// A member (ns 42) — the IDENTITY layer (ADR-0009 §1, ADR-0025 §4): the
/// deterministic mapping `member_id → display name + role tag` that the
/// serialization path uses to prepend `[display-name]` to each message. The
/// SEMANTIC layer (preferences, history, relationship) lives in the graph as
/// facts about a member node; there is no profile table.
#[derive(KeyEncode, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[ok_ns(42)]
pub struct MemberKey {
    pub member_id: Uuid7,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MemberKey)]
#[ok_ns(42)]
#[ok_name("members")]
pub struct MemberRow {
    /// What the model sees in the attribution bracket. Changing it rewrites
    /// nothing in the log: the log stays raw (ADR-0009 §1).
    pub display_name: String,
    /// HUMAN or AGENT — the ONE axis serialization needs (who is the
    /// assistant). Business roles (support / buyer) are semantics and belong to
    /// the graph, not to this row.
    pub role: u8,
}

/// A channel itself (ns 45). Until this row exists a channel is only a number
/// in other tables' key prefixes: nothing says what it IS. The row is the
/// application's own annotation (`scene`) plus when it was opened — the facts a
/// caller needs to decide "this is that person's private line" without reading
/// the log.
#[derive(KeyEncode, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[ok_ns(45)]
pub struct ChannelKey {
    pub channel_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(ChannelKey)]
#[ok_ns(45)]
#[ok_name("channels")]
#[ok_index(by_app { fields(app_id) })]
pub struct ChannelRow {
    /// WHICH APPLICATION owns this channel — the OIDC `client_id` value, not
    /// "which user": `gravity` opens one channel per person and must find exactly
    /// one of its own when it asks, so this is a filter key the deployment writes
    /// and nobody edits by hand.
    ///
    /// Kept apart from `scene` on purpose: a filter key that a human relabels
    /// ("private" one day, "1:1" the next) would make "exactly one" depend on
    /// spelling.
    pub app_id: Uuid7,
    /// What the channel is FOR, in the writer's own words ("private", "group",
    /// a scenario tag). Free text on purpose: the storage layer has no opinion
    /// about scenes, and a closed enum here would make every new scene a
    /// migration.
    pub scene: String,
    /// The caller's clock when the channel was opened.
    pub created_ms: u64,
}

/// The OTHER half of the many-to-many (ns 46): `member → channel`.
///
/// `members` answers "who is in this channel" (its key starts with the channel);
/// this table answers the question `members` structurally cannot — "which
/// channels does this member have" — as a prefix scan instead of a full read.
/// Two tables instead of a junction with an empty value because a member row
/// carries identity (`display_name`, `role`) and a junction's entries carry
/// nothing; the mirror is what makes both directions O(prefix).
#[derive(KeyEncode, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[ok_ns(46)]
pub struct MembershipKey {
    pub channel_id: u64,
    pub member_id: Uuid7,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MembershipKey)]
#[ok_ns(46)]
#[ok_name("memberships")]
#[ok_layout(version = 2)]
// A LEAD face (okm ADR-0054): its segment is sliced out of the key's own encoding, so the
// member id is NOT copied into the payload. One fact, one place — a `merge` cannot set the
// column to something the key disagrees with, and the reverse read rebuilds without a decode.
#[ok_lead(by_member { fields(member_id) })]
pub struct MembershipRow {
    /// The name this person goes by IN THIS CHANNEL. Empty = use the identity's
    /// `display_name` (the same "empty means absent" rule the checkpoint summary follows);
    /// `DocumentEncode` has no `Option<T>`, and inventing one for a display detail would be a
    /// codec change for a cosmetic need.
    pub nickname: String,
    /// When this member joined (the caller's clock, from the same write that
    /// registered them).
    pub since_ms: u64,
}

/// Member kinds (ADR-0009 §1). `AGENT` is the assistant side of the
/// conversation; everyone else is a human participant.
pub mod member_role {
    pub const HUMAN: u8 = 0;
    pub const AGENT: u8 = 1;
}

/// The agent's context projection (ADR-0008 §2): checkpoint + increment.
/// Structural data, NOT a serialized prompt — prompt assembly and
/// tool-call pruning are Phase 2.5's view layer, not storage's job.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionView {
    /// The coverage point the checkpoint carries ([`Pointer::NONE`] = no
    /// checkpoint: the increment IS the full log). A POINTER into the log's own
    /// key space — see [`CheckpointRow`].
    pub coverage: Pointer,
    pub summary: Option<String>,
    /// The cut that compression considered better than the mechanical one
    /// ([`Pointer::NONE`] = none). A LABEL, not a coverage point: the increment
    /// is still cut at `coverage`. A tail operation reads from here so its range
    /// follows a model's view of the conversation's structure.
    pub suggested: Pointer,
    /// The FROZEN initialization memory of that compression: the edge ids the
    /// head was built from (ADR-0008 §2). Frozen as a SET and rendered per
    /// request, so the head's bytes stay stable while `?format=` still picks
    /// the representation. `None` = no checkpoint yet, so the head is the
    /// live traversal.
    pub head: Option<Vec<u64>>,
    /// A newer node without a summary exists above the coverage point: a
    /// compression is in flight, so a second pass over the same span would be
    /// a wasted model call.
    pub compressing: bool,
    /// Log entries strictly after `coverage_ts`, key order (== time
    /// order, BE bytes).
    pub increment: Vec<(MessageKey, MessageRow)>,
    /// When that member last READ (the caller's clock, ADR-0008 §3) — the
    /// cache clock's observable. `None` = never read with a clock.
    pub read_at: Option<u64>,
}

/// Dynamic-segment names the channel tables use.
pub mod checkpoint_field {
    /// The frozen head of a compression: an array of edge ids (u64 as UInt).
    pub const HEAD: &str = "head";
}

/// The graph node name a channel member's facts hang on (ADR-0009 §1).
///
/// Identity is `member_id → node`, and the mapping is a NAMING RULE, not a table:
/// deterministic, derived from the ids, stable across renames (a display name is
/// presentation — renaming a member must not move a single fact). The channel is
/// in the name because member 7 of channel 2 is a different person from member 7
/// of channel 3, and a scope can hold many channels.
pub fn member_node(member_id: Uuid7) -> String {
    format!("member-{member_id}")
}

/// The member whose projection the reads assemble. Single-member shape
/// (ADR-0008 §5: the single-user form needs nothing extra); the id is
/// fixed so callers need not thread it through Phase 0's turn loop.
pub fn default_member() -> Uuid7 {
    Uuid7::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
}

/// The name member 1 is seeded with on a channel's first touch. One constant so
/// the attribution bracket reads the same everywhere; change it and every
/// channel without its own member-1 row follows on the next write.
pub const DEFAULT_MEMBER_NAME: &str = "master";

/// Assembly point: one Fjall keyspace holding every channel-side table.
/// Shared FjallStore = one atomicity domain (same discipline as the
/// graph's group). This group's keyspace is `crate::KEYSPACE_CHANNEL` —
/// its own tree, because the log is queue-like and takes the store's
/// highest write rate.
pub struct ChannelStore {
    messages: okm_core::Collection<FjallStore, MessageKey, MessageRow>,
    checkpoints: okm_core::Collection<FjallStore, CheckpointKey, CheckpointRow>,
    cursors: okm_core::Collection<FjallStore, CursorKey, CursorRow>,
    members: okm_core::Collection<FjallStore, MemberKey, MemberRow>,
    channels: okm_core::Collection<FjallStore, ChannelKey, ChannelRow>,
    memberships: okm_core::Collection<FjallStore, MembershipKey, MembershipRow>,
}

impl ChannelStore {
    /// Open (or create) this group's keyspace at `path` and attach the
    /// channel tables — the standalone form (tests, single-table tools).
    pub fn open(path: &std::path::Path) -> Result<Self, String> {
        let store = FjallStore::open(path, crate::KEYSPACE_CHANNEL).map_err(|e| e.to_string())?;
        Self::wrap(store)
    }

    /// Attach the channel tables to an already-open store handle — the
    /// attach point `crate::service::open_service` uses so every table
    /// group shares the process's one database.
    pub fn wrap(store: FjallStore) -> Result<Self, String> {
        Ok(Self {
            messages: <MessageRow as okm_core::Document>::collection(store.clone()),
            checkpoints: <CheckpointRow as okm_core::Document>::collection(store.clone()),
            cursors: <CursorRow as okm_core::Document>::collection(store.clone()),
            members: <MemberRow as okm_core::Document>::collection(store.clone()),
            channels: <ChannelRow as okm_core::Document>::collection(store.clone()),
            memberships: <MembershipRow as okm_core::Document>::collection(store),
        })
    }

    /// This group's tables, `(name, ns)` — each name from the row that declares
    /// it, so the catalog cannot drift from the tables it names.
    pub fn tables(&self) -> Vec<(&'static str, u16)> {
        vec![
            (
                <MessageRow as Document>::TABLE_NAME,
                crate::export::ns(MessageRow::NS_PREFIX),
            ),
            (
                <CheckpointRow as Document>::TABLE_NAME,
                crate::export::ns(CheckpointRow::NS_PREFIX),
            ),
            (
                <CursorRow as Document>::TABLE_NAME,
                crate::export::ns(CursorRow::NS_PREFIX),
            ),
            (
                <MemberRow as Document>::TABLE_NAME,
                crate::export::ns(MemberRow::NS_PREFIX),
            ),
            (
                <ChannelRow as Document>::TABLE_NAME,
                crate::export::ns(ChannelRow::NS_PREFIX),
            ),
            (
                <MembershipRow as Document>::TABLE_NAME,
                crate::export::ns(MembershipRow::NS_PREFIX),
            ),
        ]
    }

    /// Land one table's rows at `ns` — the restore's per-table door (ADR-0015).
    /// `None` = this group holds no such table.
    pub fn restore(
        &mut self,
        table: &str,
        ns: u16,
        batches: &[arrow::array::RecordBatch],
    ) -> Option<Result<usize, String>> {
        if table == <MessageRow as Document>::TABLE_NAME {
            Some(crate::export::land_into(
                &mut self.messages.rebased(ns),
                batches,
            ))
        } else if table == <CheckpointRow as Document>::TABLE_NAME {
            Some(crate::export::land_into(
                &mut self.checkpoints.rebased(ns),
                batches,
            ))
        } else if table == <CursorRow as Document>::TABLE_NAME {
            Some(crate::export::land_into(
                &mut self.cursors.rebased(ns),
                batches,
            ))
        } else if table == <MemberRow as Document>::TABLE_NAME {
            Some(crate::export::land_into(
                &mut self.members.rebased(ns),
                batches,
            ))
        } else if table == <ChannelRow as Document>::TABLE_NAME {
            Some(crate::export::land_into(
                &mut self.channels.rebased(ns),
                batches,
            ))
        } else if table == <MembershipRow as Document>::TABLE_NAME {
            Some(crate::export::land_into(
                &mut self.memberships.rebased(ns),
                batches,
            ))
        } else {
            None
        }
    }

    /// Whether a table's namespace is untouched (the restore's refusal, ADR-0015).
    pub fn ns_is_empty(&self, table: &str, ns: u16) -> Option<Result<bool, String>> {
        if table == <MessageRow as Document>::TABLE_NAME {
            Some(crate::export::ns_is_empty_into(&self.messages.rebased(ns)))
        } else if table == <CheckpointRow as Document>::TABLE_NAME {
            Some(crate::export::ns_is_empty_into(
                &self.checkpoints.rebased(ns),
            ))
        } else if table == <CursorRow as Document>::TABLE_NAME {
            Some(crate::export::ns_is_empty_into(&self.cursors.rebased(ns)))
        } else if table == <MemberRow as Document>::TABLE_NAME {
            Some(crate::export::ns_is_empty_into(&self.members.rebased(ns)))
        } else if table == <ChannelRow as Document>::TABLE_NAME {
            Some(crate::export::ns_is_empty_into(&self.channels.rebased(ns)))
        } else if table == <MembershipRow as Document>::TABLE_NAME {
            Some(crate::export::ns_is_empty_into(
                &self.memberships.rebased(ns),
            ))
        } else {
            None
        }
    }

    /// Export one table by NAME as Parquet bytes, with the columns that identify a row.
    /// `None` = this group holds no such table.
    pub fn export(&mut self, name: &str) -> Option<Result<crate::export::Export, String>> {
        if name == <MessageRow as Document>::TABLE_NAME {
            Some(crate::export::table_bytes(&mut self.messages))
        } else if name == <CheckpointRow as Document>::TABLE_NAME {
            Some(crate::export::table_bytes(&mut self.checkpoints))
        } else if name == <CursorRow as Document>::TABLE_NAME {
            Some(crate::export::table_bytes(&mut self.cursors))
        } else if name == <MemberRow as Document>::TABLE_NAME {
            Some(crate::export::table_bytes(&mut self.members))
        } else if name == <ChannelRow as Document>::TABLE_NAME {
            Some(crate::export::table_bytes(&mut self.channels))
        } else if name == <MembershipRow as Document>::TABLE_NAME {
            Some(crate::export::table_bytes(&mut self.memberships))
        } else {
            None
        }
    }

    /// Mint the next turn id for a channel (ADR-0024 §2). Derived from the last
    /// row in key order — key order IS turn creation order — so there is no
    /// counter to keep, nothing to repair after an import, and a restart picks up
    /// where the log left off.
    ///
    /// An id that is minted and never written is reused by the next open, which
    /// is what you want: a turn that wrote nothing never happened.
    /// Write the FIRST row of a NEW turn; the store mints the id as part of that
    /// write (ADR-0024 §2). A turn exists because a user said something, and the
    /// id is read off the log inside the same call that writes the row making it
    /// evidence — so two turns starting in one channel cannot be handed one id,
    /// and a turn abandoned before its first write consumes nothing (the log
    /// grows gaps, never repeats). Returns the full key, whose `turn_id` the
    /// caller carries for every later row of that turn.
    pub fn append_opening(
        &mut self,
        channel_id: u64,
        ts_ms: u64,
        sub: u16,
        role: u8,
        kind: u8,
        content: &str,
    ) -> Result<MessageKey, String> {
        self.ensure_member(channel_id)?;
        let turn_id = self.last_turn(channel_id)? + 1;
        self.append(channel_id, turn_id, ts_ms, sub, role, kind, content)
    }

    /// [`append_opening`](Self::append_opening) with an explicit speaker — the
    /// shape a group conversation writes its first row in.
    #[allow(clippy::too_many_arguments)] // a log record IS this wide
    pub fn append_opening_from(
        &mut self,
        channel_id: u64,
        ts_ms: u64,
        sub: u16,
        sender: Uuid7,
        end_ts_ms: u64,
        role: u8,
        kind: u8,
        content: &str,
    ) -> Result<MessageKey, String> {
        self.ensure_member(channel_id)?;
        let turn_id = self.last_turn(channel_id)? + 1;
        self.append_from(
            channel_id, turn_id, ts_ms, sub, sender, end_ts_ms, role, kind, content,
        )
    }

    /// The highest turn id in a channel's log, `0` when it has no rows. One
    /// reverse seek: the prefix's last key carries the newest turn. This IS the
    /// mint's only input — correct because the mint happens while the new turn's
    /// first row is being written, at which point the previous turn is already in
    /// the log.
    /// reverse seek: the prefix's last key carries the newest turn.
    fn last_turn(&self, channel_id: u64) -> Result<u64, String> {
        let probe = MessageKey::prefix().channel_id(channel_id).finish();
        match self.messages.scan_keys(&probe).next_back() {
            None => Ok(0),
            Some(key) => Ok(key.map_err(|e| e.to_string())?.turn_id),
        }
    }

    /// Append one entry; returns the full key (the caller's delta record
    /// carries it for idempotent replay). The caller assigns `ts_ms` AND
    /// `sub` — gravity's clock and gravity's tiebreaker (ADR-0008 §3:
    /// the single clock authority is the writer, never this store) — and names
    /// the TURN it is writing into (ADR-0024: the store mints the id at the
    /// turn's open, the caller carries it round to round).
    #[allow(clippy::too_many_arguments)] // a log record IS this wide
    pub fn append(
        &mut self,
        channel_id: u64,
        turn_id: u64,
        ts_ms: u64,
        sub: u16,
        role: u8,
        kind: u8,
        content: &str,
    ) -> Result<MessageKey, String> {
        // The single-party shape writes its one user — there is no "nobody
        // said it" (ADR-0009 §1).
        self.append_from(
            channel_id,
            turn_id,
            ts_ms,
            sub,
            default_member(),
            0,
            role,
            kind,
            content,
        )
    }

    /// Append one entry attributed to `sender` (a member id, ns 42), ending at
    /// `end_ts_ms` (`0` = instant).
    #[allow(clippy::too_many_arguments)] // a log record IS this wide
    pub fn append_from(
        &mut self,
        channel_id: u64,
        turn_id: u64,
        ts_ms: u64,
        sub: u16,
        sender: Uuid7,
        end_ts_ms: u64,
        role: u8,
        kind: u8,
        content: &str,
    ) -> Result<MessageKey, String> {
        self.ensure_member(channel_id)?;
        if self.turn_is_closed(channel_id, turn_id)? {
            return Err(format!(
                "turn {turn_id} of channel {channel_id} is closed: its rows were collapsed, \
                 and a row appended after the closing record would never be purged"
            ));
        }
        let key = MessageKey {
            channel_id,
            turn_id,
            ts_ms,
            sub,
        };
        let row = MessageRow {
            sender,
            role,
            kind,
            end_ts_ms,
            content: content.to_string(),
        };
        self.messages.put(&key, &row).map_err(|e| e.to_string())?;
        Ok(key)
    }

    /// Close a turn (ADR-0024 §4/§5): write the ONE `kind::FINAL` record, then
    /// keep the rows it names and purge the rest of that turn.
    ///
    /// `keep` names RECORDS of this turn — `(ts_ms, sub)` pairs, i.e. what the
    /// caller asked to survive (the question, one row per successful call, the
    /// answer). It is an argument and not stored data: the columns that survive
    /// are their own answer, and nothing reads the list afterwards.
    ///
    /// The purge is a `Collection::purge` per unkept row inside the turn's own
    /// prefix range — one scan of one turn, once per turn. It is an internal
    /// write-path action: there is no door for it, and the storage layer gains
    /// nothing.
    #[allow(clippy::too_many_arguments)] // the closing record plus the keep set
    pub fn close_turn(
        &mut self,
        channel_id: u64,
        turn_id: u64,
        ts_ms: u64,
        sub: u16,
        role: u8,
        content: &str,
        keep: &[(u64, u16)],
    ) -> Result<MessageKey, String> {
        self.ensure_member(channel_id)?;
        let closing = MessageKey {
            channel_id,
            turn_id,
            ts_ms,
            sub,
        };
        // The closing record must be the turn's LAST record: the read decides
        // "closed" from these rows alone, so a closing record that sorted before
        // a later row would read as an open turn with a stray FINAL in it.
        let existing = self.turn_keys(channel_id, turn_id)?;
        if let Some(last) = existing.last() {
            if Pointer::of(last) >= Pointer::of(&closing) {
                return Err(format!(
                    "the closing record {turn_id}:{ts_ms}:{sub} does not follow the turn's last \
                     record {}:{}:{}",
                    last.turn_id, last.ts_ms, last.sub
                ));
            }
        }
        self.messages
            .put(
                &closing,
                &MessageRow {
                    sender: default_member(),
                    role,
                    kind: kind::FINAL,
                    end_ts_ms: 0,
                    content: content.to_string(),
                },
            )
            .map_err(|e| e.to_string())?;
        for key in self.turn_keys(channel_id, turn_id)? {
            if key.ts_ms == ts_ms && key.sub == sub {
                continue; // the closing record itself
            }
            if keep.contains(&(key.ts_ms, key.sub)) {
                continue;
            }
            self.messages.purge(&key).map_err(|e| e.to_string())?;
        }
        Ok(closing)
    }

    /// Whether the turn already carries its closing record. One reverse seek:
    /// the closing record is a turn's last row, so its presence is the answer.
    fn turn_is_closed(&self, channel_id: u64, turn_id: u64) -> Result<bool, String> {
        let probe = MessageKey::prefix()
            .channel_id(channel_id)
            .turn_id(turn_id)
            .finish();
        let Some(key) = self.messages.scan_keys(&probe).next_back() else {
            return Ok(false);
        };
        let key = key.map_err(|e| e.to_string())?;
        Ok(self
            .messages
            .get(&key)
            .map_err(|e| e.to_string())?
            .is_some_and(|row| row.kind == kind::FINAL))
    }

    /// Every key of one turn, in key order — one prefix scan over the turn's own
    /// range (`[channel][turn]`), which is why the turn sits in the key.
    fn turn_keys(&self, channel_id: u64, turn_id: u64) -> Result<Vec<MessageKey>, String> {
        let probe = MessageKey::prefix()
            .channel_id(channel_id)
            .turn_id(turn_id)
            .finish();
        self.messages
            .scan_keys(&probe)
            .map(|key| key.map_err(|e| e.to_string()))
            .collect()
    }

    /// Member 1 always exists (ADR-0009 §1): a channel's first touch seeds it
    /// with the default name, so a log never carries a message nobody owns.
    fn ensure_member(&mut self, channel_id: u64) -> Result<(), String> {
        let key = MemberKey {
            member_id: default_member(),
        };
        if self.members.get(&key).map_err(|e| e.to_string())?.is_none() {
            self.put_member(default_member(), DEFAULT_MEMBER_NAME, member_role::HUMAN)?;
        }
        // The channel still needs its own relationship row: identity is global, membership is
        // not (ADR-0051).
        let joined = MembershipKey {
            channel_id,
            member_id: default_member(),
        };
        if self
            .memberships
            .get(&joined)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            self.memberships
                .put(
                    &joined,
                    &MembershipRow {
                        nickname: String::new(),
                        since_ms: 0,
                    },
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// The agent's projection read (ADR-0008 §2): the NEWEST node that carries a
    /// summary decides three things at once — the summary, the head frozen with
    /// it, and the pointer the increment is cut at. Nodes below it are history
    /// (invisible to the read); a node above it with an empty summary is a
    /// compression in flight.
    pub fn fetch_full(&mut self, channel_id: u64, member_id: Uuid7) -> Result<SessionView, String> {
        self.ensure_member(channel_id)?;
        let nodes = self.checkpoints_of(channel_id)?;
        let complete = nodes.iter().rfind(|(_, row)| !row.summary.is_empty());
        let coverage = complete.map_or(Pointer::NONE, |(key, _)| Pointer::of_checkpoint(key));
        let summary = complete.map(|(_, row)| row.summary.clone());
        let suggested = complete
            .map(|(_, row)| Pointer {
                turn_id: row.suggested_turn_id,
                ts_ms: row.suggested_ts,
                sub: row.suggested_sub,
            })
            .unwrap_or(Pointer::NONE);
        let head = match complete {
            Some((key, _)) => self.checkpoint_head(key)?,
            None => None,
        };
        let compressing = nodes.last().is_some_and(|(key, row)| {
            row.summary.is_empty() && Pointer::of_checkpoint(key) > coverage
        });
        Ok(SessionView {
            coverage,
            summary,
            suggested,
            head,
            compressing,
            increment: self.increment(channel_id, coverage)?,
            read_at: self.read_at(channel_id, member_id)?,
        })
    }

    /// Every checkpoint node of one channel, in key order (== pointer order).
    /// Prefix scan over the channel's range, same discipline as the log read.
    fn checkpoints_of(
        &mut self,
        channel_id: u64,
    ) -> Result<Vec<(CheckpointKey, CheckpointRow)>, String> {
        let probe = CheckpointKey::prefix().channel_id(channel_id).finish();
        let mut out = Vec::new();
        for key in self.checkpoints.scan_keys(&probe) {
            let key = key.map_err(|e| e.to_string())?;
            // A node that vanished between the scan and the read is absence; a
            // store that cannot answer is not, and it propagates.
            if let Some(row) = self.checkpoints.get(&key).map_err(|e| e.to_string())? {
                out.push((key, row));
            }
        }
        Ok(out)
    }

    /// The frozen head stored with one node, if any (ADR-0008 §2: the
    /// initialization memory is snapshotted as a SET of edge ids, so the head
    /// stays byte-stable while `?format=` keeps choosing the representation).
    fn checkpoint_head(&mut self, key: &CheckpointKey) -> Result<Option<Vec<u64>>, String> {
        // `None` here is a node with no stored head; a store that cannot answer
        // is not that, and it propagates.
        let Some(doc) = self.checkpoints.fetch(key).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        Ok(match doc.get(checkpoint_field::HEAD) {
            Some(okm_core::okm_value::DynamicValue::Array(items)) => Some(
                items
                    .iter()
                    .filter_map(|item| match item {
                        okm_core::okm_value::DynamicValue::U64(id) => Some(*id),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
    }

    /// Write the frozen head ids into a node's dynamic segment.
    fn write_head(&mut self, key: &CheckpointKey, head: &[u64]) -> Result<(), String> {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            checkpoint_field::HEAD.to_string(),
            okm_core::okm_value::DynamicValue::Array(
                head.iter()
                    .map(|id| okm_core::okm_value::DynamicValue::U64(*id))
                    .collect(),
            ),
        );
        // The head is a dynamic name, so `merge` routes it to the segment and
        // leaves the row alone (ADR-0035: names decide the arm, not the verb).
        self.checkpoints
            .merge(key, &fields)
            .map_err(|e| e.to_string())
    }

    /// Open a compression AT the pointer k10r served (ADR-0008 §4): the node
    /// exists with an EMPTY summary — it carries the frozen head and marks the
    /// span as taken, while the read keeps answering from the node below until
    /// this one lands. Re-opening the same pointer is a no-op by key.
    pub fn open_checkpoint(
        &mut self,
        channel_id: u64,
        pointer: Pointer,
        head: &[u64],
    ) -> Result<(), String> {
        let key = CheckpointKey {
            channel_id,
            turn_id: pointer.turn_id,
            ts_ms: pointer.ts_ms,
            sub: pointer.sub,
        };
        self.checkpoints
            .put(
                &key,
                &CheckpointRow {
                    summary: String::new(),
                    suggested_turn_id: 0,
                    suggested_ts: 0,
                    suggested_sub: 0,
                },
            )
            .map_err(|e| e.to_string())?;
        self.write_head(&key, head)
    }

    /// The span the newest OPEN node closes: `(start, end)` where `start` is
    /// the pointer of the node BELOW it (exclusive — that is what the caller's
    /// summary already covers) and `end` is the open node's own pointer
    /// (inclusive). `None` = nothing is open.
    ///
    /// It exists so the door can answer a caller's bad suggestion with a 400
    /// that names the range, instead of the store's own invariant firing as a
    /// 500 later.
    pub fn open_span(&mut self, channel_id: u64) -> Result<Option<Span>, String> {
        let nodes = self.checkpoints_of(channel_id)?;
        let Some(open_at) = nodes.iter().rposition(|(_, row)| row.summary.is_empty()) else {
            return Ok(None);
        };
        let end = Pointer::of_checkpoint(&nodes[open_at].0);
        let start = if open_at == 0 {
            Pointer::NONE
        } else {
            Pointer::of_checkpoint(&nodes[open_at - 1].0)
        };
        Ok(Some(Span { start, end }))
    }

    /// Land a summary on the newest OPEN node — its pointer is the one k10r
    /// derived, never a caller's number (ADR-0008 §4). `None` when nothing is
    /// open: there is no position to land on, so the door answers 400 rather
    /// than inventing one.
    ///
    /// `suggested` is the cut the compression pass considered better than the
    /// mechanical one. It is VALIDATED against the span this node closes —
    /// strictly above the previous node's pointer (the summary it replaces) and
    /// at or below this node's own — because a suggestion outside the span it
    /// was made over is a caller's mistake, and storing it would make a later
    /// analysis pass read a range that never existed. A suggestion equal to the
    /// node's own pointer is the mechanical cut restated: stored as none.
    pub fn complete_checkpoint(
        &mut self,
        channel_id: u64,
        summary: &str,
        suggested: Option<Pointer>,
    ) -> Result<Option<Pointer>, String> {
        let nodes = self.checkpoints_of(channel_id)?;
        let Some(open_at) = nodes.iter().rposition(|(_, row)| row.summary.is_empty()) else {
            return Ok(None);
        };
        let key = nodes[open_at].0.clone();
        let span_start = if open_at == 0 {
            Pointer::NONE
        } else {
            Pointer::of_checkpoint(&nodes[open_at - 1].0)
        };
        let span_end = Pointer::of_checkpoint(&key);
        let suggested = match suggested {
            None => None,
            Some(point) if point == span_end => None,
            Some(point) if point > span_start && point < span_end => Some(point),
            Some(point) => {
                return Err(format!(
                    "the suggested cut {}:{}:{} is outside the span this checkpoint closes \
                     ({}:{}:{} exclusive .. {}:{}:{} inclusive)",
                    point.turn_id,
                    point.ts_ms,
                    point.sub,
                    span_start.turn_id,
                    span_start.ts_ms,
                    span_start.sub,
                    span_end.turn_id,
                    span_end.ts_ms,
                    span_end.sub
                ))
            }
        };
        // A label names a RECORD that existed: a pointer inside the span but on
        // no record (a turn nothing was written under, a millisecond with no
        // row) would send a later pass to a range that was never there, which is
        // the same defect as one outside the span.
        if suggested.is_some_and(|point| {
            self.messages
                .get(&MessageKey {
                    channel_id,
                    turn_id: point.turn_id,
                    ts_ms: point.ts_ms,
                    sub: point.sub,
                })
                .is_ok_and(|row| row.is_none())
        }) {
            return Err(format!(
                "the suggested cut {}:{}:{} names no record of this channel",
                suggested.map_or(0, |p| p.turn_id),
                suggested.map_or(0, |p| p.ts_ms),
                suggested.map_or(0, |p| p.sub),
            ));
        }
        let suggested = suggested.unwrap_or(Pointer::NONE);
        let head = self.checkpoint_head(&key)?.unwrap_or_default();
        self.checkpoints
            .put(
                &key,
                &CheckpointRow {
                    summary: summary.to_string(),
                    suggested_turn_id: suggested.turn_id,
                    suggested_ts: suggested.ts_ms,
                    suggested_sub: suggested.sub,
                },
            )
            .map_err(|e| e.to_string())?;
        // The head is rewritten with the summary: `put` replaces the whole
        // document, so the node's upward half has to ride along.
        self.write_head(&key, &head)?;
        Ok(Some(span_end))
    }

    /// Write a complete node directly (tests, and the backfill path if one ever
    /// appears). The door uses `open_checkpoint` + `complete_checkpoint`.
    pub fn append_checkpoint(
        &mut self,
        channel_id: u64,
        pointer: Pointer,
        summary: &str,
        head: &[u64],
    ) -> Result<(), String> {
        let key = CheckpointKey {
            channel_id,
            turn_id: pointer.turn_id,
            ts_ms: pointer.ts_ms,
            sub: pointer.sub,
        };
        self.checkpoints
            .put(
                &key,
                &CheckpointRow {
                    summary: summary.to_string(),
                    suggested_turn_id: 0,
                    suggested_ts: 0,
                    suggested_sub: 0,
                },
            )
            .map_err(|e| e.to_string())?;
        self.write_head(&key, head)
    }

    /// Stamp the member's read CLOCK (the caller's clock at this read). The
    /// model's read always reaches the log's end, so no position is stored — the
    /// checkpoint is what cuts the increment (ADR-0008 §2). Never rewinds: an
    /// earlier clock leaves the stored one alone.
    pub fn note_read(
        &mut self,
        channel_id: u64,
        member_id: Uuid7,
        read_at: u64,
    ) -> Result<(), String> {
        let key = CursorKey {
            channel_id,
            member_id,
        };
        // Never rewinds: an earlier clock leaves the stored one alone.
        if self
            .cursors
            .get(&key)
            .map_err(|e| e.to_string())?
            .is_some_and(|row| row.read_at >= read_at)
        {
            return Ok(());
        }
        self.cursors
            .put(&key, &CursorRow { read_at })
            .map_err(|e| e.to_string())
    }

    /// The caller's clock at that member's last read — the cache clock's
    /// observable (ADR-0008 §3).
    pub fn read_at(&self, channel_id: u64, member_id: Uuid7) -> Result<Option<u64>, String> {
        Ok(self
            .cursors
            .get(&CursorKey {
                channel_id,
                member_id,
            })
            .map_err(|e| e.to_string())?
            .map(|row| row.read_at))
    }

    /// Register (or rename) a member — the identity layer's only write
    /// (ADR-0009 §1). Idempotent by key: the log stores raw sender ids, so a
    /// rename re-attributes every past message without rewriting one.
    pub fn put_member(
        &mut self,
        member_id: Uuid7,
        display_name: &str,
        role: u8,
    ) -> Result<(), String> {
        self.members
            .put(
                &MemberKey { member_id },
                &MemberRow {
                    display_name: display_name.to_string(),
                    role,
                },
            )
            .map_err(|e| e.to_string())
    }

    /// Open (or re-annotate) a channel: the row that makes the id a THING. Idempotent by key —
    /// the scene is the application's label, so writing it twice with a new scene is a rename,
    /// not a second channel.
    pub fn put_channel(
        &self,
        channel_id: u64,
        app_id: Uuid7,
        scene: &str,
        created_ms: u64,
    ) -> Result<(), String> {
        self.channels
            .put(
                &ChannelKey { channel_id },
                &ChannelRow {
                    app_id,
                    scene: scene.to_string(),
                    created_ms,
                },
            )
            .map_err(|e| e.to_string())
    }

    /// One channel's row, if it was opened.
    pub fn channel(&self, channel_id: u64) -> Result<Option<ChannelRow>, String> {
        self.channels
            .get(&ChannelKey { channel_id })
            .map_err(|e| e.to_string())
    }

    /// Register a member AND their membership in ONE commit.
    ///
    /// Two tables, one fact: `members` is the channel's view of who is in it, `memberships` is the
    /// member's view of where they are. A commit that landed one and not the other would answer
    /// "yes" from one side and "no" from the other — the same failure a half-written junction has,
    /// so it is written the same way (one batch, both rows).
    pub fn join(
        &self,
        channel_id: u64,
        member_id: Uuid7,
        display_name: &str,
        role: u8,
        since_ms: u64,
    ) -> Result<(), String> {
        let mut batch = okm_core::engine::storage::Batch::default();
        self.members
            .save_with_prev_into(
                &mut batch,
                &MemberKey { member_id },
                &MemberRow {
                    display_name: display_name.to_string(),
                    role,
                },
                None,
            )
            .map_err(|e| e.to_string())?;
        self.memberships
            .save_with_prev_into(
                &mut batch,
                &MembershipKey {
                    channel_id,
                    member_id,
                },
                &MembershipRow {
                    nickname: String::new(),
                    since_ms,
                },
                None,
            )
            .map_err(|e| e.to_string())?;
        self.members.commit_batch(batch).map_err(|e| e.to_string())
    }

    /// Which channels this MEMBER is in — the direction the primary key cannot answer (it starts
    /// with the channel). Answered by the declared `by_member` index: a prefix scan over the
    /// index, a fetch-back per hit. The reverse of a container read costs one hop, which is what
    /// a secondary index is (ADR-0051).
    pub fn channels_of(&mut self, member_id: Uuid7) -> Result<Vec<(u64, MembershipRow)>, String> {
        // A typed probe over the lead face: the segment is the key's own bytes, so the
        // builder takes the id and the entry comes out already ordered by it.
        let probe = __OkmLead_MembershipRow_by_member::prefix()
            .member_id(member_id)
            .finish();
        let hits = self
            .memberships
            .scan::<__OkmLead_MembershipRow_by_member>(&probe)
            .map_err(|e| e.to_string())?;
        let mut out: Vec<(u64, MembershipRow)> = hits
            .into_iter()
            .filter_map(|(key, row)| row.map(|r| (key.decoded.channel_id, r)))
            .collect();
        out.sort_by_key(|(channel_id, _)| *channel_id);
        Ok(out)
    }

    /// Every channel this APP owns, in id order — the read that answers "which channel is this
    /// person's line" once the caller has narrowed by membership. Filtering here rather than in
    /// the caller keeps the "exactly one" invariant at the storage line that states it.
    pub fn channels_of_app(
        &mut self,
        app_id: Uuid7,
    ) -> Result<Vec<(ChannelKey, ChannelRow)>, String> {
        // The filter is equality on the app id, not on a label: the deployment writes the id
        // once and nothing relabels it, so "exactly one channel for this app" cannot become a
        // spelling question. `by_app` is declared on the row; the set of channels is small by
        // nature (one per line across all apps), so the equality pass is the honest read.
        Ok(self
            .channels()?
            .into_iter()
            .filter(|(_, row)| row.app_id == app_id)
            .collect())
    }

    /// Every channel row, in id order. The set of channels is small by nature (one per line), and
    /// this is the call a deployment makes to find "the channel for this person's line".
    pub fn channels(&mut self) -> Result<Vec<(ChannelKey, ChannelRow)>, String> {
        let mut out = Vec::new();
        for key in self.channels.scan_keys(&okm_core::PrefixProbe::empty()) {
            let key = key.map_err(|e| e.to_string())?;
            if let Some(row) = self.channels.get(&key).map_err(|e| e.to_string())? {
                out.push((key, row));
            }
        }
        Ok(out)
    }

    /// One member's identity, if registered.
    pub fn member(&self, member_id: Uuid7) -> Result<Option<MemberRow>, String> {
        self.members
            .get(&MemberKey { member_id })
            .map_err(|e| e.to_string())
    }

    /// Every member of one channel, in member-id order. The RELATIONSHIP table answers this now:
    /// the channel's key prefix lists its rows, and each row's member id fetches the identity.
    pub fn members(&mut self, channel_id: u64) -> Result<Vec<(MemberKey, MemberRow)>, String> {
        let probe = MembershipKey::prefix().channel_id(channel_id).finish();
        let mut out = Vec::new();
        for key in self.memberships.scan_keys(&probe) {
            let key = key.map_err(|e| e.to_string())?;
            let identity = MemberKey {
                member_id: key.member_id,
            };
            if let Some(row) = self.members.get(&identity).map_err(|e| e.to_string())? {
                out.push((identity, row));
            }
        }
        Ok(out)
    }

    /// The channel's WHOLE log, key order — the human view (ADR-0009 §3: people
    /// read the channel; no checkpoint, no merging, every record as written).
    ///
    /// The one row it leaves out is a closing record: `kind::FINAL` is the
    /// store's own bookkeeping for the turn axis, carries no conversation, and
    /// would otherwise appear to a person as an empty line between turns.
    pub fn history(&mut self, channel_id: u64) -> Result<Vec<(MessageKey, MessageRow)>, String> {
        Ok(self
            .rows_after(channel_id, Pointer::NONE)?
            .into_iter()
            .filter(|(_, row)| row.kind != kind::FINAL)
            .collect())
    }

    /// The INCREMENT (ADR-0008 §2 + ADR-0024 §6): the rows a caller may read
    /// after the coverage point — closed turns only, and never their closing
    /// record.
    ///
    /// A turn is closed exactly when its closing record is among these rows. The
    /// closing record is always a turn's LAST row (that is enforced by
    /// [`Self::close_turn`]), so a turn whose closing record sits at or before
    /// the coverage point has no rows in this window at all: any turn with rows
    /// here and no closing record here is still running, and is left out.
    fn increment(
        &self,
        channel_id: u64,
        after: Pointer,
    ) -> Result<Vec<(MessageKey, MessageRow)>, String> {
        let rows = self.rows_after(channel_id, after)?;
        let closed: std::collections::BTreeSet<u64> = rows
            .iter()
            .filter(|(_, row)| row.kind == kind::FINAL)
            .map(|(key, _)| key.turn_id)
            .collect();
        Ok(rows
            .into_iter()
            .filter(|(key, row)| row.kind != kind::FINAL && closed.contains(&key.turn_id))
            .collect())
    }

    /// Log entries for `channel_id` strictly after `after`, in key order.
    /// Prefix-scan over the channel's primary-key range: BE bytes make engine
    /// order == key order, so no sort and no secondary index — the log IS the
    /// index (ADR-0008 §3: max-gap and increments ride the same physical order,
    /// ADR-0024: turn first, then the record's own time).
    ///
    /// The comparison is on the whole pointer: a record that shares the coverage
    /// point's millisecond but sorts after it is NOT covered, which a ts-only
    /// compare would silently drop — and the same holds across turns.
    fn rows_after(
        &self,
        channel_id: u64,
        after: Pointer,
    ) -> Result<Vec<(MessageKey, MessageRow)>, String> {
        let probe = MessageKey::prefix().channel_id(channel_id).finish();
        let mut out = Vec::new();
        for key in self.messages.scan_keys(&probe) {
            let key = key.map_err(|e| e.to_string())?;
            if Pointer::of(&key) <= after {
                continue;
            }
            // A row that vanished between the scan and the read is absence; a
            // store that cannot answer is not, and it propagates.
            if let Some(row) = self.messages.get(&key).map_err(|e| e.to_string())? {
                out.push((key, row));
            }
        }
        Ok(out)
    }
}

/// Speaker classes. Closed set (the projection maps these onto LLM
/// roles); unknown values are a caller bug — the store does not police
/// them (storage stores, the view interprets).
pub mod role {
    pub const USER: u8 = 0;
    pub const ASSISTANT: u8 = 1;
    pub const TOOL: u8 = 2;
    pub const SYSTEM: u8 = 3;
}

/// Record shapes. `TEXT` is Phase 0's only writer; the others are the
/// view-layer pruning's future subjects (storage always full).
pub mod kind {
    pub const TEXT: u8 = 0;
    pub const TOOL_CALL: u8 = 1;
    pub const TOOL_RESULT: u8 = 2;
    /// The turn's closing record (ADR-0024 §4): its presence is what makes the
    /// turn CLOSED, and the write that puts it there is the write that purges the
    /// rows the closing record did not name. Never projected, never shown to a
    /// person — it is the store's bookkeeping for the turn axis.
    pub const FINAL: u8 = 3;
}
