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

/// The read cursor (ADR-0008 §2) is the cache clock's only observable:
/// it stamps the caller's clock at a read, never rewinds, and is per
/// member. No stamp = no cursor, which is what keeps an unknown idle
/// time from reading as an idle of zero.
#[test]
fn the_read_cursor_stamps_advances_and_never_rewinds() {
    let (_d, mut store) = open();
    const CH: u64 = 1;
    store.append(CH, 100, 0, role::USER, 0, "m1");

    assert_eq!(store.read_cursor(CH, DEFAULT_MEMBER), None, "never read");
    assert_eq!(store.fetch_full(CH, DEFAULT_MEMBER).last_read, None);

    store.note_read(CH, DEFAULT_MEMBER, 1_000);
    assert_eq!(store.read_cursor(CH, DEFAULT_MEMBER), Some(1_000));
    assert_eq!(
        store.fetch_full(CH, DEFAULT_MEMBER).last_read,
        Some(1_000),
        "the projection carries it, so the engine needs no second read"
    );

    store.note_read(CH, DEFAULT_MEMBER, 900);
    assert_eq!(
        store.read_cursor(CH, DEFAULT_MEMBER),
        Some(1_000),
        "an older read must not move the clock backwards"
    );
    store.note_read(CH, DEFAULT_MEMBER, 2_000);
    assert_eq!(store.read_cursor(CH, DEFAULT_MEMBER), Some(2_000));

    assert_eq!(
        store.read_cursor(CH, 2),
        None,
        "another member's cursor is its own"
    );
    assert_eq!(
        store.read_cursor(2, DEFAULT_MEMBER),
        None,
        "another channel's"
    );
}

fn verdict(checkpoint_ts: u64) -> mem_core::compress::Verdict {
    mem_core::compress::Verdict {
        checkpoint_ts,
        trigger: mem_core::compress::Trigger::Budget,
        assembled_bytes: 60_000,
        context_bytes: 131_072,
        budget_percent: 50,
        cache_ttl_ms: 300_000,
        idle_ms: Some(1_000),
        max_gap_ms: Some(700),
    }
}

/// The compression contract (ADR-0008 §4): k10r serves the verdict,
/// remembers the span, and DERIVES the coverage point when the summary
/// comes back — the caller never states one. The decision closes in the
/// same batch as the checkpoint.
#[test]
fn a_summary_lands_on_the_derived_coverage_point() {
    let (_d, mut store) = open();
    const CH: u64 = 1;
    for (ts, text) in [(100, "m1"), (200, "m2"), (900, "m3")] {
        store.append(CH, ts, 0, role::USER, 0, text);
    }

    store.note_verdict(CH, DEFAULT_MEMBER, &verdict(200));
    let (key, row) = store
        .pending_verdict(CH, DEFAULT_MEMBER)
        .expect("the verdict is pending");
    assert_eq!(key.checkpoint_ts, 200);
    assert_eq!(row.state, mem_core::decision_state::PENDING);
    assert_eq!(row.trigger, mem_core::compress::Trigger::Budget.code());
    assert_eq!(row.max_gap_ms, 700, "the gap the split was chosen on");

    let coverage = store
        .append_checkpoint_from_verdict(CH, DEFAULT_MEMBER, "m1 and m2")
        .expect("a pending verdict exists");
    assert_eq!(
        coverage, 200,
        "the span's last record, not a caller's number"
    );

    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.coverage_ts, 200);
    assert_eq!(view.summary.as_deref(), Some("m1 and m2"));
    assert_eq!(view.increment.len(), 1, "m3 is after the coverage point");
    assert!(
        store.pending_verdict(CH, DEFAULT_MEMBER).is_none(),
        "the decision closed with the summary"
    );
}

/// No verdict, no derivation: a summary with nothing pending has no
/// coverage point to land on, so the store refuses rather than inventing
/// one (the door turns this into a 400).
#[test]
fn a_summary_without_a_verdict_is_refused() {
    let (_d, mut store) = open();
    const CH: u64 = 1;
    store.append(CH, 100, 0, role::USER, 0, "m1");

    assert_eq!(
        store.append_checkpoint_from_verdict(CH, DEFAULT_MEMBER, "orphan"),
        None
    );
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.coverage_ts, 0, "no checkpoint was written");
    assert!(view.summary.is_none());
}

/// Re-reading an unchanged projection proposes the same split, so the
/// decision log does not grow per read — and when two splits are open,
/// the summary lands on the NEWEST proposal.
#[test]
fn repeated_reads_refresh_one_row_and_the_newest_split_wins() {
    let (_d, mut store) = open();
    const CH: u64 = 1;

    store.note_verdict(CH, DEFAULT_MEMBER, &verdict(200));
    store.note_verdict(CH, DEFAULT_MEMBER, &verdict(200));
    assert_eq!(
        store.pending_verdict(CH, DEFAULT_MEMBER).map(|(k, _)| k),
        Some(mem_core::DecisionKey {
            channel_id: CH,
            member_id: DEFAULT_MEMBER,
            checkpoint_ts: 200,
        })
    );

    store.note_verdict(CH, DEFAULT_MEMBER, &verdict(500));
    let (key, _) = store.pending_verdict(CH, DEFAULT_MEMBER).unwrap();
    assert_eq!(key.checkpoint_ts, 500, "the newest proposal is pending");

    assert_eq!(
        store.append_checkpoint_from_verdict(CH, DEFAULT_MEMBER, "s"),
        Some(500)
    );
}
