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
    write_slot, Document, DocumentEncode, FjallStore, KeyEncode, MemBatch, VirtualStorage,
    PRIMARY_SLOT,
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

/// The agent's private projection state: WHERE the summary covers up to
/// (coverage point) and the summary itself. Keyed per member — two
/// agents on one channel compress independently (ADR-0008 §2).
///
/// Overwrite (same-key RMW), not an append-only checkpoint log: the
/// projection reads only the LATEST checkpoint; audit history of
/// compression decisions lives in `DecisionRow` (ns 40) below. If
/// re-segmentation later needs the checkpoint HISTORY, this becomes an
/// append-only log keyed by coverage point — a pre-production schema
/// change, zero migration (ADR-0018's clean-break rule).
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

/// One compression decision (ns 40) — ADR-0008 §4's structured decision
/// log: the parameter snapshot and signal values beside the verdict, so
/// tuning later reads real records instead of fitting noise.
///
/// Keyed by the SPLIT it proposes, not by the read that proposed it:
/// repeated reads of one unchanged projection propose the same split,
/// so the key dedupes them for free, and the row's `state` answers "did
/// the caller's summary ever land".
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(40)]
pub struct DecisionKey {
    pub channel_id: u64,
    pub member_id: u64,
    /// The coverage point the verdict proposed (the span's last record).
    pub checkpoint_ts: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DecisionKey)]
#[ok_ns(40)]
pub struct DecisionRow {
    pub state: u8,
    /// 0 = budget, 1 = cache clock, 2 = both (see `compress::Trigger`).
    pub trigger: u8,
    pub assembled_bytes: u32,
    pub context_bytes: u32,
    pub budget_percent: u8,
    pub cache_ttl_ms: u64,
    /// Milliseconds since the read cursor; 0 when no clock rode the read.
    pub idle_ms: u64,
    /// The largest inter-message gap inside the span; 0 when the span
    /// held no internal gap (its end was the split).
    pub max_gap_ms: u64,
}

/// Decision states. PENDING = the verdict was served and no summary has
/// come back; DONE = a summary landed and the checkpoint covers it.
pub mod decision_state {
    pub const PENDING: u8 = 0;
    pub const DONE: u8 = 1;
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
    /// When this member last read its projection (ADR-0008 §2's read
    /// cursor); `None` = never read with a clock.
    pub last_read: Option<u64>,
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
    store: FjallStore,
    messages: okm_core::Collection<FjallStore, MessageKey, MessageRow>,
    checkpoints: okm_core::Collection<FjallStore, CheckpointKey, CheckpointRow>,
    cursors: okm_core::Collection<FjallStore, CursorKey, CursorRow>,
    decisions: okm_core::Collection<FjallStore, DecisionKey, DecisionRow>,
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
            decisions: <DecisionRow as okm_core::Document>::collection(store.clone()),
            store,
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
    /// increment strictly after its coverage point + the member's read
    /// cursor. One prefix scan in key order; the coverage filter is a
    /// single compare per row.
    ///
    /// The scan is channel-prefixed — only the channel's key range is
    /// read, never the whole table (same discipline as the graph's
    /// scope-prefixed scans).
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
            last_read: self.read_cursor(channel_id, member_id),
        }
    }

    /// Record (or advance) the member's checkpoint: `summary` now covers
    /// everything at or before `coverage_ts`. Same-key overwrite — see
    /// the struct doc for why this is not yet an append-only log.
    ///
    /// The door never calls this: ADR-0008 §4 has the caller state the
    /// SUMMARY only, with the coverage point derived from the verdict
    /// k10r itself served (`append_checkpoint_from_verdict`). This stays
    /// as the primitive the derivation uses, and for tests that need a
    /// checkpoint without a preceding verdict.
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

    /// Record a verdict the engine just served. Keyed by the proposed
    /// split, so re-reading an unchanged projection refreshes one row
    /// instead of growing the log.
    pub fn note_verdict(
        &mut self,
        channel_id: u64,
        member_id: u64,
        verdict: &crate::compress::Verdict,
    ) {
        let key = DecisionKey {
            channel_id,
            member_id,
            checkpoint_ts: verdict.checkpoint_ts,
        };
        let row = DecisionRow {
            state: decision_state::PENDING,
            trigger: verdict.trigger.code(),
            assembled_bytes: verdict.assembled_bytes.min(u32::MAX as usize) as u32,
            context_bytes: verdict.context_bytes.min(u32::MAX as usize) as u32,
            budget_percent: verdict.budget_percent,
            cache_ttl_ms: verdict.cache_ttl_ms,
            idle_ms: verdict.idle_ms.unwrap_or(0),
            max_gap_ms: verdict.max_gap_ms.unwrap_or(0),
        };
        self.decisions.put(&key, &row);
    }

    /// The latest PENDING verdict for this member (key order == split
    /// order, so the last pending row is the newest proposal).
    pub fn pending_verdict(
        &self,
        channel_id: u64,
        member_id: u64,
    ) -> Option<(DecisionKey, DecisionRow)> {
        let mut prefix = DecisionRow::NS_PREFIX.to_vec();
        write_slot(&mut prefix, PRIMARY_SLOT);
        let probe = DecisionKey {
            channel_id,
            member_id,
            checkpoint_ts: 0,
        };
        probe.encode_prefix_named(&mut prefix, &["channel_id", "member_id"]);
        let mut latest: Option<(DecisionKey, DecisionRow)> = None;
        for sfx in self.decisions.store().scan_suffix(&prefix) {
            let mut full = prefix.clone();
            full.extend_from_slice(&sfx);
            let key = DecisionKey::decode(&full[DecisionRow::NS_PREFIX.len() + 1..]);
            let Some(row) = self.decisions.get(&key) else {
                continue;
            };
            if row.state == decision_state::PENDING {
                latest = Some((key, row));
            }
        }
        latest
    }

    /// Land a summary against the verdict k10r served: the checkpoint
    /// takes the verdict's split as its coverage point (ADR-0008 §4 —
    /// the caller never computes or states a coverage timestamp), and
    /// the decision closes in the SAME batch.
    ///
    /// `None` = there is no pending verdict, so there is nothing to
    /// derive a coverage point from (the door turns that into a 400).
    pub fn append_checkpoint_from_verdict(
        &mut self,
        channel_id: u64,
        member_id: u64,
        summary: &str,
    ) -> Option<u64> {
        let (key, mut row) = self.pending_verdict(channel_id, member_id)?;
        row.state = decision_state::DONE;

        let mut batch = MemBatch::default();
        let checkpoint_key = CheckpointKey {
            channel_id,
            member_id,
        };
        let mut pkey = Vec::with_capacity(3 + 16);
        pkey.extend_from_slice(CheckpointRow::NS_PREFIX);
        write_slot(&mut pkey, PRIMARY_SLOT);
        pkey.extend_from_slice(&checkpoint_key.encode());
        batch.put(
            pkey,
            CheckpointRow {
                coverage_ts: key.checkpoint_ts,
                summary: summary.to_string(),
            }
            .encode_payload(),
        );

        let mut dkey = Vec::with_capacity(3 + 24);
        dkey.extend_from_slice(DecisionRow::NS_PREFIX);
        write_slot(&mut dkey, PRIMARY_SLOT);
        dkey.extend_from_slice(&key.encode());
        batch.put(dkey, row.encode_payload());

        self.store.commit_batch(batch).ok()?;
        Some(key.checkpoint_ts)
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
                // reconstruction (the graph's pattern).
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
