//! The unified channel log + the agent's projection (ADR-0008, minimal
//! Phase 2 slice serving gravity Phase 0): an append-only message log
//! keyed `(channel_id, ts_ms, sub)` with gravity's clock as the order
//! authority, plus the per-member checkpoint (the compression record).
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
//! layout makes byte order == time order: a prefix scan over
//! `channel_id` returns the log in conversation order, no sort step.

use okm_core::{
    write_slot, Document, DocumentEncode, FjallStore, KeyEncode, VirtualStorage, PRIMARY_SLOT,
};

/// Append-only message log (ns 33 — the channel-log band, next to flat
/// memory's 32; one entity class = one ns, per MODELING).
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(33)]
pub struct MessageKey {
    pub channel_id: u64,
    pub ts_ms: u64,
    pub sub: u16,
}

/// One log entry. `role` is the speaker class (the projection maps it to
/// LLM message roles); `kind` separates plain text from future record
/// shapes (tool_call / tool_result) — the VIEW prunes those, storage
/// always holds full rows (ADR-0006's view-layer pruning ruling).
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MessageKey)]
#[ok_ns(33)]
pub struct MessageRow {
    pub role: u8,
    pub kind: u8,
    pub content: String,
}

/// A checkpoint NODE (ns 34) — layer 1 of the channel, isomorphic to the log
/// below it (ADR-0008 §2). The KEY is a pointer into layer 0: the record's own
/// `(ts_ms, sub)`. The payload is the summary of the span ending at that
/// record, plus (dynamic segment) the head frozen at that compression.
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
    pub ts_ms: u64,
    pub sub: u16,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(CheckpointKey)]
#[ok_ns(34)]
pub struct CheckpointRow {
    /// Empty = the compression is in flight (a second pass treats the span as
    /// taken); otherwise the summary of everything up to this pointer.
    pub summary: String,
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
    pub member_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(CursorKey)]
#[ok_ns(39)]
pub struct CursorRow {
    /// The caller's clock at the read that stamped it (ADR-0008 §3: the
    /// caller owns the clock, and no clock means no stamp).
    pub ts_ms: u64,
}

/// The agent's context projection (ADR-0008 §2): checkpoint + increment.
/// Structural data, NOT a serialized prompt — prompt assembly and
/// tool-call pruning are Phase 2.5's view layer, not storage's job.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionView {
    /// The coverage point the checkpoint carries (`(0, 0)` = no checkpoint:
    /// the increment IS the full log). A POINTER — see [`CheckpointRow`].
    pub coverage_ts: u64,
    pub coverage_sub: u16,
    pub summary: Option<String>,
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
    /// When this member last read its projection (ADR-0008 §2's read
    /// cursor); `None` = never read with a clock.
    pub last_read: Option<u64>,
}

/// Dynamic-segment names the channel tables use.
pub mod checkpoint_field {
    /// The frozen head of a compression: an array of edge ids (u64 as UInt).
    pub const HEAD: &str = "head";
}

/// The member whose projection the reads assemble. Single-member shape
/// (ADR-0008 §5: the single-user form needs nothing extra); the id is
/// fixed so callers need not thread it through Phase 0's turn loop.
pub const DEFAULT_MEMBER: u64 = 1;

/// Assembly point: one Fjall keyspace holding every channel-side table.
/// Shared FjallStore = one atomicity domain (same discipline as the
/// graph's group). This group's keyspace is `crate::KEYSPACE_CHANNEL` —
/// its own tree, because the log is queue-like and takes the store's
/// highest write rate.
pub struct ChannelStore {
    messages: okm_core::Collection<FjallStore, MessageKey, MessageRow>,
    checkpoints: okm_core::Collection<FjallStore, CheckpointKey, CheckpointRow>,
    cursors: okm_core::Collection<FjallStore, CursorKey, CursorRow>,
}

impl ChannelStore {
    /// Open (or create) this group's keyspace at `path` and attach the
    /// channel tables — the standalone form (tests, single-table tools).
    pub fn open(path: &std::path::Path) -> Result<Self, fjall::Error> {
        Ok(Self::wrap(FjallStore::open(path, crate::KEYSPACE_CHANNEL)?))
    }

    /// Attach the channel tables to an already-open store handle — the
    /// attach point `crate::service::open_service` uses so every table
    /// group shares the process's one database.
    pub fn wrap(store: FjallStore) -> Self {
        Self {
            messages: <MessageRow as okm_core::Document>::collection(store.clone()),
            checkpoints: <CheckpointRow as okm_core::Document>::collection(store.clone()),
            cursors: <CursorRow as okm_core::Document>::collection(store.clone()),
        }
    }

    /// Append one entry; returns the full key (the caller's delta record
    /// carries it for idempotent replay). The caller assigns `ts_ms` AND
    /// `sub` — gravity's clock and gravity's tiebreaker (ADR-0008 §3:
    /// the single clock authority is the writer, never this store).
    pub fn append(
        &mut self,
        channel_id: u64,
        ts_ms: u64,
        sub: u16,
        role: u8,
        kind: u8,
        content: &str,
    ) -> MessageKey {
        let key = MessageKey {
            channel_id,
            ts_ms,
            sub,
        };
        let row = MessageRow {
            role,
            kind,
            content: content.to_string(),
        };
        self.messages.put(&key, &row);
        key
    }

    /// The agent's projection read (ADR-0008 §2): the NEWEST node that carries a
    /// summary decides three things at once — the summary, the head frozen with
    /// it, and the pointer the increment is cut at. Nodes below it are history
    /// (invisible to the read); a node above it with an empty summary is a
    /// compression in flight.
    pub fn fetch_full(&mut self, channel_id: u64, member_id: u64) -> SessionView {
        let nodes = self.checkpoints_of(channel_id);
        let complete = nodes.iter().rfind(|(_, row)| !row.summary.is_empty());
        let coverage = complete.map_or((0, 0), |(key, _)| (key.ts_ms, key.sub));
        let summary = complete.map(|(_, row)| row.summary.clone());
        let head = complete.and_then(|(key, _)| self.checkpoint_head(key));
        let compressing = nodes
            .last()
            .is_some_and(|(key, row)| row.summary.is_empty() && (key.ts_ms, key.sub) > coverage);
        SessionView {
            coverage_ts: coverage.0,
            coverage_sub: coverage.1,
            summary,
            head,
            compressing,
            increment: self.messages_after(channel_id, coverage),
            last_read: self.read_cursor(channel_id, member_id),
        }
    }

    /// Every checkpoint node of one channel, in key order (== pointer order).
    /// Prefix scan over the channel's range, same discipline as the log read.
    fn checkpoints_of(&mut self, channel_id: u64) -> Vec<(CheckpointKey, CheckpointRow)> {
        let mut prefix = Vec::with_capacity(3 + 8);
        prefix.extend_from_slice(CheckpointRow::NS_PREFIX);
        write_slot(&mut prefix, PRIMARY_SLOT);
        let probe = CheckpointKey {
            channel_id,
            ts_ms: 0,
            sub: 0,
        };
        probe.encode_prefix_named(&mut prefix, &["channel_id"]);
        let suffixes: Vec<Vec<u8>> = self
            .checkpoints
            .store()
            .scan_suffix(&prefix)
            .iter()
            .map(|sfx| sfx.to_vec())
            .collect();
        let mut out = Vec::with_capacity(suffixes.len());
        for sfx in suffixes {
            let mut full = prefix.clone();
            full.extend_from_slice(&sfx);
            let key = CheckpointKey::decode(&full[3..]);
            if let Some(row) = self.checkpoints.get(&key) {
                out.push((key, row));
            }
        }
        out
    }

    /// The frozen head stored with one node, if any (ADR-0008 §2: the
    /// initialization memory is snapshotted as a SET of edge ids, so the head
    /// stays byte-stable while `?format=` keeps choosing the representation).
    fn checkpoint_head(&mut self, key: &CheckpointKey) -> Option<Vec<u64>> {
        let doc = self.checkpoints.get_document(key)?;
        match doc.get(checkpoint_field::HEAD) {
            Some(okm_core::obj_dynamic::DynamicValue::Array(items)) => Some(
                items
                    .iter()
                    .filter_map(|item| match item {
                        okm_core::obj_dynamic::DynamicValue::UInt(id) => Some(*id),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Write the frozen head ids into a node's dynamic segment.
    fn write_head(&mut self, key: &CheckpointKey, head: &[u64]) {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            checkpoint_field::HEAD.to_string(),
            okm_core::obj_dynamic::DynamicValue::Array(
                head.iter()
                    .map(|id| okm_core::obj_dynamic::DynamicValue::UInt(*id))
                    .collect(),
            ),
        );
        self.checkpoints.put_fields(key, &fields);
    }

    /// Open a compression AT the pointer k10r served (ADR-0008 §4): the node
    /// exists with an EMPTY summary — it carries the frozen head and marks the
    /// span as taken, while the read keeps answering from the node below until
    /// this one lands. Re-opening the same pointer is a no-op by key.
    pub fn open_checkpoint(&mut self, channel_id: u64, pointer: (u64, u16), head: &[u64]) {
        let key = CheckpointKey {
            channel_id,
            ts_ms: pointer.0,
            sub: pointer.1,
        };
        self.checkpoints.put(
            &key,
            &CheckpointRow {
                summary: String::new(),
            },
        );
        self.write_head(&key, head);
    }

    /// Land a summary on the newest OPEN node — its pointer is the one k10r
    /// derived, never a caller's number (ADR-0008 §4). `None` when nothing is
    /// open: there is no position to land on, so the door answers 400 rather
    /// than inventing one.
    pub fn complete_checkpoint(&mut self, channel_id: u64, summary: &str) -> Option<(u64, u16)> {
        let nodes = self.checkpoints_of(channel_id);
        let (key, _) = nodes.iter().rfind(|(_, row)| row.summary.is_empty())?;
        let key = key.clone();
        let head = self.checkpoint_head(&key).unwrap_or_default();
        self.checkpoints.put(
            &key,
            &CheckpointRow {
                summary: summary.to_string(),
            },
        );
        // The head is rewritten with the summary: `put` replaces the whole
        // document, so the node's upward half has to ride along.
        self.write_head(&key, &head);
        Some((key.ts_ms, key.sub))
    }

    /// Write a complete node directly (tests, and the backfill path if one ever
    /// appears). The door uses `open_checkpoint` + `complete_checkpoint`.
    pub fn append_checkpoint(
        &mut self,
        channel_id: u64,
        pointer: (u64, u16),
        summary: &str,
        head: &[u64],
    ) {
        let key = CheckpointKey {
            channel_id,
            ts_ms: pointer.0,
            sub: pointer.1,
        };
        self.checkpoints.put(
            &key,
            &CheckpointRow {
                summary: summary.to_string(),
            },
        );
        self.write_head(&key, head);
    }

    /// Stamp the member's read cursor from the caller's clock at a
    /// projection read (ADR-0008 §2's cache clock; §3's "the caller owns
    /// the clock" — no ts means the read is served and nothing is
    /// stamped, never a server-invented instant). Never rewinds.
    pub fn note_read(&mut self, channel_id: u64, member_id: u64, ts_ms: u64) {
        let key = CursorKey {
            channel_id,
            member_id,
        };
        if self.cursors.get(&key).is_some_and(|row| row.ts_ms >= ts_ms) {
            return;
        }
        self.cursors.put(&key, &CursorRow { ts_ms });
    }

    /// The member's read cursor, if a read ever carried a clock.
    pub fn read_cursor(&self, channel_id: u64, member_id: u64) -> Option<u64> {
        self.cursors
            .get(&CursorKey {
                channel_id,
                member_id,
            })
            .map(|row| row.ts_ms)
    }

    /// Log entries for `channel_id` strictly after `after`, in key order.
    /// Prefix-scan over the channel's primary-key range: BE bytes make engine
    /// order == `(ts_ms, sub)` order, so no sort and no secondary index — the
    /// log IS the index (ADR-0008 §3: max-gap and increments ride the same
    /// physical order).
    ///
    /// The comparison is LEXICOGRAPHIC on the pointer: a record that shares
    /// the coverage point's millisecond but sorts after it is NOT covered
    /// (its `sub` is larger), which a ts-only compare would silently drop.
    fn messages_after(&self, channel_id: u64, after: (u64, u16)) -> Vec<(MessageKey, MessageRow)> {
        // Header is `[ns 2B][0x00]` — the primary slot is a fixed face, one
        // byte (okm ADR-0030).
        let mut prefix = Vec::with_capacity(3 + 8);
        prefix.extend_from_slice(MessageRow::NS_PREFIX);
        okm_core::index::write_slot(&mut prefix, okm_core::index::PRIMARY_SLOT);
        let probe = MessageKey {
            channel_id,
            ts_ms: 0,
            sub: 0,
        };
        probe.encode_prefix_named(&mut prefix, &["channel_id"]);
        self.messages
            .store()
            .scan_suffix(&prefix)
            .iter()
            .filter_map(|sfx| {
                // suffix = `[ts_ms 8B][sub 2B]` after
                // `[ns 2B][0x00][channel_id 8B]`; decode via full-key
                // reconstruction (the graph's pattern).
                let mut full = prefix.clone();
                full.extend_from_slice(sfx);
                let key = MessageKey::decode(&full[3..]);
                if (key.ts_ms, key.sub) <= after {
                    return None;
                }
                let row = self.messages.get(&key)?;
                Some((key, row))
            })
            .collect()
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
}
