//! The unified channel log + the agent's projection (ADR-0008, minimal
//! Phase 2 slice serving gravity Phase 0): an append-only message log
//! keyed `(channel_id, ts_ms, sub)` with gravity's clock as the order
//! authority, plus the per-member checkpoint (the compression record).
//!
//! Deliberately NOT here yet (later Phase 2 slices, ADR-0008/0009):
//! threads (sub-channels), topic-tag index, the members table, read
//! cursors (the single-member shape derives unread from the checkpoint's
//! coverage point), and the compression policy engine (Phase 2.6).
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

use okm_core::{Document, DocumentEncode, FjallStore, KeyEncode, VirtualStorage};

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

/// The agent's private projection state: WHERE the summary covers up to
/// (coverage point) and the summary itself. Keyed per member — two
/// agents on one channel compress independently (ADR-0008 §2).
///
/// Overwrite (same-key RMW), not an append-only checkpoint log: the
/// projection reads only the LATEST checkpoint; audit history of
/// compression decisions belongs to Phase 2.6's decision log, which is
/// a different table. If re-segmentation later needs the checkpoint
/// HISTORY, this becomes an append-only log keyed by coverage point —
/// a pre-production schema change, zero migration (ADR-0018's
/// clean-break rule).
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(34)]
pub struct CheckpointKey {
    pub channel_id: u64,
    pub member_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(CheckpointKey)]
#[ok_ns(34)]
pub struct CheckpointRow {
    /// Everything at or before this timestamp is summarized into
    /// `summary`; the projection's increment starts strictly after.
    pub coverage_ts: u64,
    pub summary: String,
}

/// The agent's context projection (ADR-0008 §2): checkpoint + increment.
/// Structural data, NOT a serialized prompt — prompt assembly and
/// tool-call pruning are Phase 2.5's view layer, not storage's job.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionView {
    /// The coverage point the checkpoint carries (0 = no checkpoint:
    /// the increment IS the full log).
    pub coverage_ts: u64,
    pub summary: Option<String>,
    /// Log entries strictly after `coverage_ts`, key order (== time
    /// order, BE bytes).
    pub increment: Vec<(MessageKey, MessageRow)>,
}

/// The member whose projection the reads assemble. Single-member shape
/// (ADR-0008 §5: the single-user form needs nothing extra); the id is
/// fixed so callers need not thread it through Phase 0's turn loop.
pub const DEFAULT_MEMBER: u64 = 1;

/// Assembly point: one Fjall keyspace holding both tables. Shared
/// FjallStore = one atomicity domain (same discipline as MemoryStore).
/// This group's keyspace is `crate::KEYSPACE_CHANNEL` — its own tree,
/// because the log is queue-like and takes the store's highest write
/// rate.
pub struct ChannelStore {
    messages: okm_core::Collection<FjallStore, MessageKey, MessageRow>,
    checkpoints: okm_core::Collection<FjallStore, CheckpointKey, CheckpointRow>,
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
            checkpoints: <CheckpointRow as okm_core::Document>::collection(store),
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

    /// The agent's projection read: latest checkpoint (if any) + the log
    /// increment strictly after its coverage point. One prefix scan in
    /// key order; the coverage filter is a single compare per row.
    ///
    /// The scan is channel-prefixed — only the channel's key range is
    /// read, never the whole table (same discipline as MemoryStore's
    /// user-prefix scan).
    pub fn fetch_full(&self, channel_id: u64, member_id: u64) -> SessionView {
        let checkpoint = self.checkpoints.get(&CheckpointKey {
            channel_id,
            member_id,
        });
        let coverage_ts = checkpoint.as_ref().map_or(0, |c| c.coverage_ts);
        let increment = self.messages_after(channel_id, coverage_ts);
        SessionView {
            coverage_ts,
            summary: checkpoint.map(|c| c.summary),
            increment,
        }
    }

    /// Record (or advance) the member's checkpoint: `summary` now covers
    /// everything at or before `coverage_ts`. Same-key overwrite — see
    /// the struct doc for why this is not yet an append-only log.
    pub fn append_checkpoint(
        &mut self,
        channel_id: u64,
        member_id: u64,
        coverage_ts: u64,
        summary: &str,
    ) {
        self.checkpoints.put(
            &CheckpointKey {
                channel_id,
                member_id,
            },
            &CheckpointRow {
                coverage_ts,
                summary: summary.to_string(),
            },
        );
    }

    /// Log entries for `channel_id` strictly after `after_ts`, in key
    /// order. Prefix-scan over the channel's primary-key range: BE bytes
    /// make engine order == `(ts_ms, sub)` order, so no sort and no
    /// secondary index — the log IS the index (ADR-0008 §3: max-gap and
    /// increments ride the same physical order).
    fn messages_after(&self, channel_id: u64, after_ts: u64) -> Vec<(MessageKey, MessageRow)> {
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
                // reconstruction (MemoryStore's pattern).
                let mut full = prefix.clone();
                full.extend_from_slice(sfx);
                let key = MessageKey::decode(&full[3..]);
                if key.ts_ms <= after_ts {
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
