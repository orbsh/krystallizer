//! The structured log (ADR-0011): k10r's own diagnostics as disposable rows.
//!
//! This table holds what is neither knowledge nor conversation — why the
//! compression cut where it did, when a derived index was rebuilt, what an
//! engine error said. It has no domain identity (no entity, no edge, no
//! message), so it belongs neither in the graph nor in the channel log, and
//! it is not one table per kind of record either: all of it shares one
//! property — uncertain, potentially useful — so it shares one table and the
//! variable part rides the dynamic segment.
//!
//! Shape (ADR-0011): key `[ts_ms 8B][seq 8B]`, so byte order IS time order and
//! "the window around T" is a range scan. `ts_ms` is k10r's OWN clock: the
//! writer of this table is the process itself, so ADR-0008 §3's rule (the
//! caller owns the clock) does not reach here — that rule governs caller
//! data, not the process's observations. `seq` is a process-local monotonic
//! counter seeded from the `seq` WATERMARK at open (a declared `HighWater`
//! reduce), so a restart continues instead of overwriting a row written in the
//! same millisecond — and trimming rows does not rewind it.
//!
//! `level` and `event` are DECLARED fields — low cardinality, so a value is
//! cheap and a face could be added later — and neither carries a 0x1 face:
//! a face is one extra index write per row for a query nobody issues yet.
//! Everything variable (the locator: `channel` / `scope` / `entity` / `edge` /
//! `msg`; the payload: signal values, parameter snapshots, error text) rides
//! slot 1's dynamic segment under names from [`field`].
//!
//! The table is BOUNDED: `logs { max-rows }` in the config, and every append
//! that crosses the ceiling deletes the oldest keys. The trim is never
//! logged — trimming would emit rows, and rows would trigger trimming. Loss is
//! the accepted price: a diagnostic table, not a ledger.

use okm_core::obj_dynamic::DynamicValue;
use okm_core::{Collection, Document, DocumentEncode, FjallStore, KeyEncode, ReduceCodec, Vocab};
use std::collections::{BTreeMap, VecDeque};

/// Severity, as a stored code. k10r never ACTS on it — a level is not a
/// policy, it triggers no alert and changes no behaviour; it exists so a scan
/// can look at errors first.
pub mod level {
    pub const TRACE: u8 = 0;
    pub const DEBUG: u8 = 1;
    pub const INFO: u8 = 2;
    pub const WARN: u8 = 3;
    pub const ERROR: u8 = 4;
}

/// Event names, as a vocabulary rather than free text: a typo becomes a
/// compile-time problem instead of a data problem (the same discipline as
/// `role` / `decision_state` before it).
pub mod event {
    /// The policy fired: trigger, the three signals' measured values, the
    /// parameter snapshot and the split it proposes.
    pub const COMPRESSION_VERDICT: &str = "compression_verdict";
    /// A summary landed and the checkpoint advanced.
    pub const COMPRESSION_LANDED: &str = "compression_landed";
    /// A derived retrieval table was rebuilt or evicted.
    pub const RETRIEVAL_INDEX: &str = "retrieval_index";
    /// An engine-level failure (the shell passes the original text through).
    pub const ENGINE_ERROR: &str = "engine_error";
}

/// Sources, as a vocabulary: the module or access path that emitted the row.
/// An empty label means "none", which is legal — not a sentinel.
pub mod source {
    /// The compression policy (`compress::decide`).
    pub const COMPRESS: &str = "compress";
    /// The channel log's own writes (checkpoint, read clock).
    pub const CHANNEL: &str = "channel";
    /// The fact graph.
    pub const GRAPH: &str = "graph";
    /// The derived retrieval tables.
    pub const RETRIEVAL: &str = "retrieval";
    /// The storage engine and its batches.
    pub const ENGINE: &str = "engine";
}

/// Dynamic-segment names this table uses. Not enforced by the store — listed
/// so call sites stop inventing a third spelling for "channel".
pub mod field {
    pub const CHANNEL: &str = "channel";
    pub const SCOPE: &str = "scope";
    pub const ENTITY: &str = "entity";
    pub const EDGE: &str = "edge";
    pub const MSG: &str = "msg";
    pub const TRIGGER: &str = "trigger";
    pub const SPLIT_TS: &str = "split_ts";
    /// The second half of the split pointer (same-millisecond tiebreaker).
    pub const SPLIT_SUB: &str = "split_sub";
    pub const IDLE_MS: &str = "idle_ms";
    pub const MAX_GAP_MS: &str = "max_gap_ms";
    pub const ASSEMBLED_BYTES: &str = "assembled_bytes";
}

/// `[ts_ms 8B][seq 8B]` — BE fixed width, so byte order IS `(ts_ms, seq)`
/// order: replay, "the last N" and "the window around T" are one range scan,
/// never a sort.
#[derive(KeyEncode, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[ok_ns(41)]
pub struct LogKey {
    pub ts_ms: u64,
    /// The process-local counter that separates rows written in one
    /// millisecond. Seeded from the last stored key, never rewinds.
    pub seq: u64,
}

/// The fixed part of a log row: four declared fields (ADR-0011 §8), everything
/// else in the dynamic segment.
///
/// `event` and `source` are `Vocab<u16>` because each is a CLOSED, small,
/// endlessly repeated set — a label in memory, two bytes on disk. Neither k10r
/// nor this store interns anything: okm's write path does it (`Collection::put`
/// → `__okm_intern`) and its read path resolves the label back (`get` →
/// `__okm_deref_vocab`), so `append` takes `&str` and the call sites stay
/// readable. The price is one document clone per write, the cost of `put(&R)`'s
/// immutable signature — paid only because this row declares a Vocab field.
///
/// `note` is the ONE free-text field: a sentence about this row for a human (an
/// engine error's original text, why an index was rebuilt). A value with a name
/// of its own belongs in the dynamic segment, not here.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(LogKey)]
#[ok_ns(41)]
#[ok_name("log")]
// `seq` is a watermark (ADR-0023/0025, PLAN 2.9d), not a count of the rows
// present: the trim deletes from the front, and a counter derived from "the last
// key" would reuse a number after the whole table had been trimmed. The fold is
// the highest `seq` ever written and the unfold is a NO-OP, which is precisely
// the contract an id allocator needs.
#[ok_reduce(HighWater(seq))]
pub struct LogRow {
    pub level: u8,
    pub event: Vocab<u16>,
    /// The module or access path that emitted the row; the empty label = none.
    pub source: Vocab<u16>,
    pub note: String,
}

/// One read row: the whole document, fixed fields and dynamic ones together.
pub type LogEntry = (LogKey, BTreeMap<String, DynamicValue>);

/// Default ceiling. Big enough for a long debugging session, small enough that
/// the table never becomes a second storage problem.
pub const DEFAULT_MAX_ROWS: usize = 10_000;

/// Assembly point: the log table on a store handle.
///
/// Lives in `crate::KEYSPACE_CHANNEL` — its write shape is the log's (append,
/// high rate, its own tree), not the memory side's (ADR-0002's split).
pub struct LogStore {
    table: Collection<FjallStore, LogKey, LogRow>,
    /// The next `seq` to hand out.
    seq: u64,
    max_rows: usize,
    /// The keys currently in the table, oldest first — the trim's own index.
    /// Bounded by `max_rows`, so this is `max_rows × 16` bytes and never a
    /// second scan per write.
    recent: VecDeque<LogKey>,
}

impl LogStore {
    /// Open (or create) the keyspace at `path` and attach the log table — the
    /// standalone form. A service process assembles every group through
    /// `crate::service::open_service`.
    pub fn open(path: &std::path::Path) -> Result<Self, fjall::Error> {
        Ok(Self::wrap(FjallStore::open(path, crate::KEYSPACE_CHANNEL)?))
    }

    /// Attach the log table to an already-open store handle.
    pub fn wrap(store: FjallStore) -> Self {
        // The watermark answers "where did the counter stop" in one point read,
        // and it does not fall when the trim deletes the newest row — a scan of
        // the last key would hand out a number already used.
        let water = okm_core::reduce::scan_reduces::<_, __OkmReduce_LogRow_0>(
            &store,
            <LogRow as Document>::NS_PREFIX,
        )
        .into_iter()
        .next()
        .map(|(_, acc)| acc);
        let table = <LogRow as Document>::collection(store);
        let keys = table.scan_keys();
        let seq = water.map_or(0, |w| w.saturating_add(1));
        let recent = keys
            .into_iter()
            .rev()
            .take(DEFAULT_MAX_ROWS)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Self {
            table,
            seq,
            max_rows: DEFAULT_MAX_ROWS,
            recent,
        }
    }

    /// Set the ceiling (the config's `logs { max-rows }`). The trim reads it
    /// on every append; an existing over-large table is trimmed by the next
    /// append, not by this call.
    pub fn with_max_rows(mut self, max_rows: usize) -> Self {
        self.max_rows = max_rows;
        self
    }

    /// How many rows the table holds right now.
    pub fn len(&self) -> usize {
        self.recent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.recent.is_empty()
    }

    /// Append one diagnostic row. `fields` is the variable part — locator and
    /// payload — and may be empty (a bare event line). `source` and `note` are
    /// the two declared fields a caller may leave empty (`""`), which is the
    /// honest default for a row whose fields already say everything.
    pub fn append(
        &mut self,
        ts_ms: u64,
        level: u8,
        event: &str,
        source: &str,
        note: &str,
        fields: BTreeMap<String, DynamicValue>,
    ) -> LogKey {
        let key = LogKey {
            ts_ms,
            seq: self.seq,
        };
        self.seq += 1;
        self.table.put(
            &key,
            &LogRow {
                level,
                event: Vocab::of(event),
                source: Vocab::of(source),
                note: note.to_string(),
            },
        );
        if !fields.is_empty() {
            // The vocabulary-resolving write: names allocate on first sight.
            self.table.put_fields(&key, &fields);
        }
        // Keep the trim index in KEY order, not arrival order: a clock step
        // backwards makes the two differ, and "oldest" must mean the smallest
        // key (the table's own order) or the trim would drop the wrong row.
        // A monotone clock inserts at the end — O(1) — and a step back pays
        // one memmove inside a bounded vec.
        let pos = self.recent.partition_point(|k| k < &key);
        self.recent.insert(pos, key.clone());
        self.trim();
        key
    }

    /// One diagnostic row for the compression verdict (ADR-0011's first
    /// writer): the decision, its trigger, the split it named and the numbers it
    /// was made from — so the policy can be re-examined without having kept a
    /// second table for it.
    pub fn record_verdict(
        &mut self,
        now_ms: u64,
        channel_id: u64,
        verdict: &crate::compress::Verdict,
    ) -> LogKey {
        let mut fields = BTreeMap::new();
        fields.insert(field::CHANNEL.to_string(), DynamicValue::UInt(channel_id));
        fields.insert(
            field::SPLIT_TS.to_string(),
            DynamicValue::UInt(verdict.checkpoint_ts),
        );
        fields.insert(
            field::SPLIT_SUB.to_string(),
            DynamicValue::UInt(u64::from(verdict.checkpoint_sub)),
        );
        fields.insert(
            field::TRIGGER.to_string(),
            DynamicValue::Str(verdict.trigger.name().to_string()),
        );
        fields.insert(
            field::ASSEMBLED_BYTES.to_string(),
            DynamicValue::UInt(verdict.assembled_bytes as u64),
        );
        if let Some(idle) = verdict.idle_ms {
            fields.insert(field::IDLE_MS.to_string(), DynamicValue::UInt(idle));
        }
        if let Some(gap) = verdict.max_gap_ms {
            fields.insert(field::MAX_GAP_MS.to_string(), DynamicValue::UInt(gap));
        }
        // No `note`: the fields above already say what happened, and a sentence
        // repeating them is the kind of data that rots. `note` is for the rows
        // the fields cannot carry.
        self.append(
            now_ms,
            level::INFO,
            event::COMPRESSION_VERDICT,
            source::COMPRESS,
            "",
            fields,
        )
    }

    /// Every row at or after `ts_ms`, in key order — a window over the table,
    /// not a query language. `max_rows` bounds it, so this is never unbounded.
    pub fn since(&mut self, ts_ms: u64) -> Vec<LogEntry> {
        let mut rows = self.entries();
        rows.retain(|(key, _)| key.ts_ms >= ts_ms);
        rows
    }

    /// The row's DECLARED fields, as stored (`event` and `source` resolve back to
    /// their labels here). `get` returns the dynamic segment; this returns what
    /// the row itself declares, which is what a test or an analysis reads when it
    /// wants the level or the event without going through the map.
    pub fn row(&mut self, key: &LogKey) -> Option<LogRow> {
        self.table.get(key)
    }

    /// One row, fixed fields and dynamic ones together. `None` = absent.
    ///
    /// `&mut` because the document read resolves field names through the
    /// table's vocabulary (the same reason okm's own `get_document` is `&mut`).
    pub fn get(&mut self, key: &LogKey) -> Option<BTreeMap<String, DynamicValue>> {
        self.table.get_document(key)
    }

    /// Rows in time order, oldest first — the whole table as it stands. The
    /// table is bounded, so this is the analysis read; a windowed read is a
    /// range scan over the same key order (not built yet: no consumer).
    pub fn entries(&mut self) -> Vec<LogEntry> {
        let keys: Vec<LogKey> = self.recent.iter().cloned().collect();
        keys.iter()
            .filter_map(|key| self.table.get_document(key).map(|doc| (key.clone(), doc)))
            .collect()
    }

    /// This store's table, `(name, ns)` — the name from the row declaration.
    pub fn tables(&self) -> Vec<(&'static str, u16)> {
        vec![(
            <LogRow as Document>::TABLE_NAME,
            crate::export::ns(LogRow::NS_PREFIX),
        )]
    }

    /// Export the table by NAME as Parquet (ADR-0007's backup boundary) — the
    /// table where "bounded and trimmed" is normal, so the export is also the only
    /// long-term record of what the trim drops. `None` = not this store's table.
    pub fn export(&mut self, name: &str, path: &std::path::Path) -> Option<Result<(), String>> {
        (name == <LogRow as Document>::TABLE_NAME)
            .then(|| crate::export::table(&mut self.table, path))
    }

    /// Restore one table by NAME from a Parquet file — the mirror of `export`.
    /// Rows land through the normal write contract, the reduce table pushes the
    /// `seq` watermark, then the issuer and the trim index are recomputed from
    /// the store.
    pub fn import(&mut self, name: &str, path: &std::path::Path) -> Option<Result<usize, String>> {
        if name == <LogRow as Document>::TABLE_NAME {
            let out = crate::export::import_table(&mut self.table, path);
            Some(crate::export::finish_import(out, || self.reseed()))
        } else if name == crate::export::LOG_REDUCES {
            let out =
                okm_core::bridge::tooling::parquet_io::import_reduces::<_, __OkmReduce_LogRow_0>(
                    self.table.store(),
                    <LogRow as Document>::NS_PREFIX,
                    <LogRow as Document>::TABLE_NAME,
                    path,
                )
                .map_err(|e| format!("reduce import: {e}"));
            Some(crate::export::finish_import(out, || self.reseed()))
        } else {
            None
        }
    }

    /// Recompute the `seq` issuer and the trim index from the store — the open
    /// path, reused after a restore. The watermark is a point read and never a
    /// scan of the last key: the trim deletes the NEWEST row when the ceiling
    /// holds, so a scan would hand out a number already used.
    pub fn reseed(&mut self) {
        let water = okm_core::reduce::scan_reduces::<_, __OkmReduce_LogRow_0>(
            self.table.store(),
            <LogRow as Document>::NS_PREFIX,
        )
        .into_iter()
        .next()
        .map(|(_, acc)| acc);
        self.seq = water.map_or(0, |w| w.saturating_add(1));
        let keys = self.table.scan_keys();
        self.recent = keys
            .into_iter()
            .rev()
            .take(self.max_rows)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
    }

    /// The `seq` watermark as the derived reduce table (ADR-0034 §5): the trim
    /// deletes from the front, so a restored log whose watermark is missing would
    /// reissue numbers the table already holds.
    pub fn export_reduces(&self, path: &std::path::Path) -> Result<(), String> {
        crate::export::reduces::<__OkmReduce_LogRow_0>(
            self.table.store(),
            crate::export::ns(LogRow::NS_PREFIX),
            <LogRow as Document>::TABLE_NAME,
            path,
        )
    }

    /// Drop the oldest rows until the ceiling holds. Not logged, by design.
    fn trim(&mut self) {
        while self.recent.len() > self.max_rows {
            let Some(oldest) = self.recent.pop_front() else {
                return;
            };
            // `delete` needs the row as well: the collection derives the face
            // set it removes from that row, so read the real one back (one
            // point read per trimmed row — bounded and rare) instead of
            // handing it a stand-in.
            match self.table.get(&oldest) {
                Some(row) => self.table.delete(&oldest, &row),
                None => continue,
            }
        }
    }
}
