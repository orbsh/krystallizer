//! The structured log's suite (ADR-0011): key order IS time order, `seq`
//! survives a reopen, the dynamic segment round-trips, and the table trims
//! its oldest rows at the ceiling instead of growing without limit.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use mem_core::log::{event, field, level, source};
use mem_core::{LogKey, LogStore, DEFAULT_MAX_ROWS};
use okm_core::obj_dynamic::DynamicValue;
use okm_core::KeyEncode;
use std::collections::BTreeMap;

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

fn fields(pairs: &[(&str, DynamicValue)]) -> BTreeMap<String, DynamicValue> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// The key layout is a contract: `[ts_ms 8B][seq 8B]`, BE. Byte order equalling
/// time order is what makes "the window around T" one range scan.
#[test]
fn log_key_layout_is_hex_locked() {
    let key = LogKey {
        ts_ms: 0x0102_0304_0506_0708,
        seq: 0x1112_1314_1516_1718,
    };
    let hex: String = key
        .encode()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(hex, "01020304050607081112131415161718");
}

/// One row carries the fixed fields and the variable part together, and the
/// store keeps what it was handed (no parsing, no policing).
#[test]
fn a_row_round_trips_with_its_dynamic_fields() {
    let dir = tempdir();
    let mut store = LogStore::open(dir.path()).expect("open");
    let key = store.append(
        1_700_000_000_000,
        level::WARN,
        event::COMPRESSION_VERDICT,
        source::COMPRESS,
        "",
        fields(&[
            (field::CHANNEL, DynamicValue::UInt(7)),
            (field::TRIGGER, DynamicValue::Str("cache_clock".into())),
            (field::IDLE_MS, DynamicValue::UInt(301_000)),
        ]),
    ).unwrap();

    let doc = store.get(&key).expect("row present").expect("row present");
    assert_eq!(
        doc.get("level"),
        Some(&DynamicValue::UInt(level::WARN as u64))
    );
    assert_eq!(
        doc.get("event"),
        Some(&DynamicValue::Str(event::COMPRESSION_VERDICT.into()))
    );
    assert_eq!(doc.get(field::CHANNEL), Some(&DynamicValue::UInt(7)));
    assert_eq!(
        doc.get(field::TRIGGER),
        Some(&DynamicValue::Str("cache_clock".into()))
    );
    assert_eq!(doc.get(field::IDLE_MS), Some(&DynamicValue::UInt(301_000)));
}

/// A bare event line needs no dynamic segment at all: `fields` empty is legal,
/// and the declared pair still round-trips.
#[test]
fn a_row_without_dynamic_fields_is_an_event_line() {
    let dir = tempdir();
    let mut store = LogStore::open(dir.path()).expect("open");
    let key = store.append(
        42,
        level::ERROR,
        event::ENGINE_ERROR,
        "",
        "",
        BTreeMap::new(),
    ).unwrap();
    let doc = store.get(&key).expect("row present").expect("row present");
    assert_eq!(doc.get(field::CHANNEL), None);
    assert_eq!(
        doc.get("event"),
        Some(&DynamicValue::Str(event::ENGINE_ERROR.into()))
    );
}

/// Rows come back in TIME order, and same-millisecond appends keep their own
/// order through `seq` — that is the whole reason the key has two segments.
#[test]
fn rows_read_in_time_order_and_same_millisecond_rows_keep_their_order() {
    let dir = tempdir();
    let mut store = LogStore::open(dir.path()).expect("open");
    store.append(2, level::INFO, "second_ts", "", "", BTreeMap::new()).unwrap();
    store.append(1, level::INFO, "first_ts", "", "", BTreeMap::new()).unwrap();
    store.append(1, level::INFO, "first_ts_again", "", "", BTreeMap::new()).unwrap();

    let events: Vec<String> = store
        .entries().unwrap()
        .into_iter()
        .filter_map(|(_, doc)| match doc.get("event") {
            Some(DynamicValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(events, vec!["first_ts", "first_ts_again", "second_ts"]);
}

/// `seq` is seeded from the last stored key, so a reopened store continues past
/// it instead of overwriting a row written in the same millisecond.
#[test]
fn seq_survives_a_reopen() {
    let dir = tempdir();
    let first = {
        let mut store = LogStore::open(dir.path()).expect("open");
        store.append(5, level::INFO, "before", "", "", BTreeMap::new()).unwrap()
    };
    assert_eq!(first.seq, 0);

    let mut store = LogStore::open(dir.path()).expect("reopen");
    let second = store.append(5, level::INFO, "after", "", "", BTreeMap::new()).unwrap();
    assert_eq!(second.ts_ms, 5, "same millisecond on purpose");
    assert_eq!(second.seq, 1, "the counter continued past the stored key");
    assert_eq!(store.len(), 2, "both rows survive");
}

/// The ceiling is the table's whole lifecycle contract: past it, the OLDEST
/// rows go, and the newest stay.
#[test]
fn the_ceiling_drops_the_oldest_rows() {
    let dir = tempdir();
    let mut store = LogStore::open(dir.path()).expect("open").with_max_rows(3);
    for i in 0..5u64 {
        store.append(i, level::INFO, &format!("e{i}"), "", "", BTreeMap::new()).unwrap();
    }
    assert_eq!(store.len(), 3);
    let events: Vec<String> = store
        .entries().unwrap()
        .into_iter()
        .filter_map(|(_, doc)| match doc.get("event") {
            Some(DynamicValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(events, vec!["e2", "e3", "e4"], "oldest two are gone");
}

/// The shipped config's ceiling and the built-in default must not drift — the
/// same rule the compression numbers follow.
#[test]
fn the_default_ceiling_is_the_one_the_config_ships() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/krystallizer.kdl");
    let cfg = mem_core::KrystallizerConfig::load(&path).expect("repo config decodes");
    assert_eq!(cfg.logs.max_rows, DEFAULT_MAX_ROWS);
}

/// ADR-0011's first writer: the verdict is a decision, so it becomes a row that
/// names the split it chose, the trigger that fired and the numbers it read —
/// which is what lets the policy be re-examined later without a second table.
#[test]
fn the_verdict_row_carries_the_decision_it_made() {
    use mem_core::compress::{Trigger, Verdict};

    let dir = tempdir();
    let mut store = LogStore::open(dir.path()).expect("open");
    let verdict = Verdict {
        checkpoint_ts: 1_500,
        checkpoint_sub: 2,
        trigger: Trigger::CacheClock,
        assembled_bytes: 12_345,
        context_bytes: 100_000,
        budget_percent: 60,
        cache_ttl_ms: 300_000,
        idle_ms: Some(400_000),
        max_gap_ms: None,
    };

    let key = store.record_verdict(9_000, 7, &verdict).unwrap();
    assert_eq!(key.ts_ms, 9_000, "the server's own clock keys the row");

    let row = store.get(&key).expect("the row lands").expect("the row lands");
    assert_eq!(row[field::CHANNEL], DynamicValue::UInt(7));
    assert_eq!(row[field::SPLIT_TS], DynamicValue::UInt(1_500));
    assert_eq!(row[field::SPLIT_SUB], DynamicValue::UInt(2));
    assert_eq!(
        row[field::TRIGGER],
        DynamicValue::Str("cache_clock".into()),
        "the trigger travels as a WORD: a log is read by people"
    );
    assert_eq!(row[field::ASSEMBLED_BYTES], DynamicValue::UInt(12_345));
    assert_eq!(row[field::IDLE_MS], DynamicValue::UInt(400_000));
    assert!(
        !row.contains_key(field::MAX_GAP_MS),
        "an absent signal writes no field — absence must not read as zero"
    );

    let entry = store.entries().unwrap().into_iter().next().expect("one entry");
    assert_eq!(entry.0, key);
    assert_eq!(store.since(9_000).unwrap().len(), 1);
    assert!(store.since(9_001).unwrap().is_empty(), "the window is a lower bound");
}

/// The four declared fields (ADR-0011 §8): a level, an event, a source and a
/// note. `event` and `source` are `Vocab<u16>` on disk — two bytes each — and
/// come back as the labels they were written as, because okm interns on write
/// and resolves on read. Nothing in this store touches the vocabulary.
#[test]
fn the_declared_fields_are_a_level_two_labels_and_a_note() {
    let dir = tempdir();
    let mut store = LogStore::open(dir.path()).expect("open");
    let key = store.append(
        7,
        level::ERROR,
        event::ENGINE_ERROR,
        source::ENGINE,
        "panic: boom",
        BTreeMap::new(),
    ).unwrap();

    let row = store.row(&key).expect("row present").expect("row present");
    assert_eq!(row.level, level::ERROR);
    assert_eq!(row.event.as_str(), Some(event::ENGINE_ERROR));
    assert_eq!(row.source.as_str(), Some(source::ENGINE));
    assert_eq!(row.note, "panic: boom");

    // A row whose fields already say everything writes no note; the empty label
    // is a legal source, not a sentinel.
    let bare = store.append(
        8,
        level::INFO,
        event::COMPRESSION_LANDED,
        "",
        "",
        BTreeMap::new(),
    ).unwrap();
    let bare = store.row(&bare).expect("row present").expect("row present");
    assert_eq!(bare.note, "");
    assert_eq!(bare.source.as_str(), Some(""));
    assert_eq!(
        bare.event.as_str(),
        Some(event::COMPRESSION_LANDED),
        "a second row in the same event resolves to the same label"
    );
}

/// The counter is a WATERMARK, not a count of the rows present (PLAN 2.9d): the
/// trim deletes from the front, and a counter derived from "the last stored key"
/// would hand out a number already used once the table had been trimmed back to
/// nothing. `HighWater(seq)` is maintained on every write and its unfold is a
/// NO-OP, so the number keeps climbing across trims and restarts.
#[test]
fn the_counter_is_a_watermark_that_the_trim_cannot_rewind() {
    let dir = tempdir();
    let last_seq = {
        let mut store = LogStore::open(dir.path()).expect("open").with_max_rows(1);
        store.append(1, level::INFO, "a", "", "", BTreeMap::new()).unwrap();
        let kept = store.append(2, level::INFO, "b", "", "", BTreeMap::new()).unwrap();
        assert_eq!(store.len(), 1, "the trim keeps the ceiling");
        kept.seq
    };

    let mut store = LogStore::open(dir.path()).expect("reopen");
    let next = store.append(3, level::INFO, "c", "", "", BTreeMap::new()).unwrap();
    assert_eq!(
        next.seq,
        last_seq + 1,
        "a trimmed row's number is not handed out again"
    );
}
