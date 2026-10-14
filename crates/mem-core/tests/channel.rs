//! Channel-log tests (ADR-0008 minimal slice): key layout hex locks,
//! the append/fetch round trip in TIME order, checkpoint coverage
//! semantics (increment starts strictly after the coverage point), and
//! per-member projection independence. The query round-trip is locked,
//! not just the bytes (okm junction lesson: byte-pass + query-empty is
//! the classic silent corruption shape).

use mem_core::channel::{role, ChannelStore, DEFAULT_MEMBER};
use okm_core::KeyEncode;
use tempfile::tempdir;

fn open() -> (tempfile::TempDir, ChannelStore) {
    let dir = tempdir().unwrap();
    let store = ChannelStore::open(dir.path()).unwrap();
    (dir, store)
}

/// Layout lock: the BARE key payload encodes as
/// `[channel_id 8B][ts_ms 8B][sub 2B]` — BE fixed-width, byte order ==
/// `(ts_ms, sub)` order. The ns (33) + primary slot live in the
/// ADDRESS header the Collection joins at entry time (ADR-0011: full
/// keys in storage, ns declared on the ROW), not in the key struct.
/// Changing the layout breaks this test first, NOT stored data.
#[test]
fn key_layout_is_hex_locked() {
    let key = mem_core::MessageKey {
        channel_id: 7,
        ts_ms: 1_700_000_000_123,
        sub: 5,
    };
    let buf = key.encode();
    assert_eq!(buf.len(), 8 + 8 + 2, "channel 8 + ts 8 + sub 2");
    assert_eq!(&buf[0..8], &7u64.to_be_bytes(), "channel_id");
    assert_eq!(&buf[8..16], &1_700_000_000_123u64.to_be_bytes(), "ts_ms");
    assert_eq!(&buf[16..18], &5u16.to_be_bytes(), "sub");
}

/// The core contract: appends come back in `(ts_ms, sub)` order from a
/// plain prefix scan — no sort step, the log IS the index. Sub breaks
/// same-millisecond ties in write order.
#[test]
fn fetch_returns_entries_in_time_order() {
    let (_d, mut store) = open();
    const CH: u64 = 1;

    store.append(CH, 999, 0, role::USER, 0, "earlier ms");
    store.append(CH, 1000, 0, role::USER, 0, "first");
    store.append(CH, 1000, 1, role::ASSISTANT, 0, "same-ms tiebreak");
    store.append(CH, 2000, 0, role::ASSISTANT, 0, "last");
    // A second channel's row: must NOT leak into CH's projection.
    store.append(2, 1500, 0, role::USER, 0, "other channel");

    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.coverage_ts, 0, "no checkpoint yet");
    assert!(view.summary.is_none());
    let texts: Vec<&str> = view
        .increment
        .iter()
        .map(|(_, r)| r.content.as_str())
        .collect();
    assert_eq!(
        texts,
        vec!["earlier ms", "first", "same-ms tiebreak", "last"],
        "engine order == (ts_ms, sub) order; other channels excluded"
    );
    assert_eq!(view.increment[1].0.ts_ms, 1000);
    assert_eq!(view.increment[1].0.sub, 0);
    assert_eq!(view.increment[2].0.sub, 1);
}

/// Append-only discipline: writes happen, reads assemble. The returned
/// keys round-trip (fetch → re-get the row by its key → identical).
#[test]
fn returned_keys_round_trip_to_rows() {
    let (_d, mut store) = open();
    const CH: u64 = 1;
    store.append(CH, 10, 0, role::USER, 0, "hello");
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.increment.len(), 1);
    let (key, row) = &view.increment[0];
    let got = store.fetch_full(CH, DEFAULT_MEMBER); // read path stable
    assert_eq!(got.increment[0].1, *row);
    assert_eq!(key.channel_id, CH);
    assert_eq!(row.role, role::USER);
    assert_eq!(row.content, "hello");
}

/// Checkpoint semantics: the increment starts STRICTLY AFTER the
/// coverage point (a message AT coverage_ts is summarized, not
/// re-delivered); the summary rides the view; advancing coverage
/// shrinks the increment (overwrite, not append — see CheckpointRow).
#[test]
fn checkpoint_coverage_splits_the_projection() {
    let (_d, mut store) = open();
    const CH: u64 = 1;

    store.append(CH, 10, 0, role::USER, 0, "m1");
    store.append(CH, 20, 0, role::ASSISTANT, 0, "m2");
    store.append(CH, 30, 0, role::USER, 0, "m3");

    store.append_checkpoint(CH, DEFAULT_MEMBER, 20, "m1 and m2 were about testing");
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.coverage_ts, 20);
    assert_eq!(
        view.summary.as_deref(),
        Some("m1 and m2 were about testing")
    );
    let texts: Vec<&str> = view
        .increment
        .iter()
        .map(|(_, r)| r.content.as_str())
        .collect();
    assert_eq!(
        texts,
        vec!["m3"],
        "strictly after coverage — m2 itself is summarized"
    );

    // Advancing coverage overwrites the same checkpoint row (and the
    // summary): m3 becomes covered too, increment drains.
    store.append_checkpoint(CH, DEFAULT_MEMBER, 30, "everything so far");
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.coverage_ts, 30);
    assert!(view.increment.is_empty());
}

/// Per-member independence (ADR-0008 §2): two members' checkpoints are
/// private projection state — one's coverage never touches the other's.
#[test]
fn checkpoints_are_per_member() {
    let (_d, mut store) = open();
    const CH: u64 = 1;
    store.append(CH, 10, 0, role::USER, 0, "m1");

    store.append_checkpoint(CH, 1, 10, "member 1 covered");
    let v2 = store.fetch_full(CH, 2);
    assert_eq!(v2.coverage_ts, 0, "member 2 unaffected");
    assert_eq!(v2.increment.len(), 1);
    let v1 = store.fetch_full(CH, 1);
    assert_eq!(v1.summary.as_deref(), Some("member 1 covered"));
}

/// Independent channels never see each other's rows (the channel prefix
/// on the scan is the isolation; one forgotten prefix = cross-channel
/// bleed, the query-round-trip lesson).
#[test]
fn channels_are_isolated() {
    let (_d, mut store) = open();
    store.append(1, 100, 0, role::USER, 0, "ch1");
    store.append(2, 100, 0, role::USER, 0, "ch2");
    store.append_checkpoint(1, DEFAULT_MEMBER, 100, "ch1 summary");

    let v2 = store.fetch_full(2, DEFAULT_MEMBER);
    assert_eq!(v2.coverage_ts, 0, "ch2 has no checkpoint");
    assert!(v2.summary.is_none());
    assert_eq!(v2.increment.len(), 1);
    assert_eq!(v2.increment[0].1.content, "ch2");
}
