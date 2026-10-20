//! Channel-log tests (ADR-0008 minimal slice): key layout hex locks,
//! the append/fetch round trip in TIME order, checkpoint coverage
//! semantics (increment starts strictly after the coverage POINTER), and
//! projection independence. The query round-trip is locked, not just the
//! bytes (okm junction lesson: byte-pass + query-empty is the classic
//! silent corruption shape).

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
    assert_eq!(view.coverage_sub, 0);
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
/// coverage POINTER (a message AT the pointer is summarized, not
/// re-delivered); the summary rides the view; advancing coverage
/// overwrites the same checkpoint row.
#[test]
fn checkpoint_coverage_splits_the_projection() {
    let (_d, mut store) = open();
    const CH: u64 = 1;

    store.append(CH, 10, 0, role::USER, 0, "m1");
    store.append(CH, 20, 0, role::ASSISTANT, 0, "m2");
    store.append(CH, 30, 0, role::USER, 0, "m3");

    store.append_checkpoint(CH, (20, 0), "m1 and m2 were about testing", &[]);
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.coverage_ts, 20);
    assert_eq!(view.coverage_sub, 0);
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
    store.append_checkpoint(CH, (30, 0), "everything so far", &[]);
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.coverage_ts, 30);
    assert!(view.increment.is_empty());
}

/// The POINTER is a record, not a millisecond (ADR-0008 §2): a message that
/// shares the coverage point's millisecond but sorts after it is NOT covered.
/// A ts-only compare would silently drop it from both the increment and the
/// summarized span — the failure that has no error anywhere.
#[test]
fn a_same_millisecond_record_after_the_pointer_stays_in_the_increment() {
    let (_d, mut store) = open();
    const CH: u64 = 1;

    store.append(CH, 1000, 0, role::USER, 0, "the split record");
    store.append(CH, 1000, 1, role::TOOL, 2, "tool_result, same ms");
    store.append(CH, 1500, 0, role::ASSISTANT, 0, "later");

    // Coverage lands ON (1000, 0) — the split record itself.
    store.append_checkpoint(CH, (1000, 0), "up to the split", &[]);
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    let texts: Vec<&str> = view
        .increment
        .iter()
        .map(|(_, r)| r.content.as_str())
        .collect();
    assert_eq!(
        texts,
        vec!["tool_result, same ms", "later"],
        "same-ms records after the pointer are still raw"
    );

    // Advancing to (1000, 1) covers that one too, and nothing else.
    store.append_checkpoint(CH, (1000, 1), "now the burst is in", &[]);
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.coverage_sub, 1);
    let texts: Vec<&str> = view
        .increment
        .iter()
        .map(|(_, r)| r.content.as_str())
        .collect();
    assert_eq!(texts, vec!["later"]);
}

/// Independent channels never see each other's rows (the channel prefix
/// on the scan is the isolation; one forgotten prefix = cross-channel
/// bleed, the query-round-trip lesson).
#[test]
fn channels_are_isolated() {
    let (_d, mut store) = open();
    store.append(1, 100, 0, role::USER, 0, "ch1");
    store.append(2, 100, 0, role::USER, 0, "ch2");
    store.append_checkpoint(1, (100, 0), "ch1 summary", &[]);

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
fn the_read_at_stamps_advances_and_never_rewinds() {
    let (_d, mut store) = open();
    const CH: u64 = 1;
    store.append(CH, 100, 0, role::USER, 0, "m1");

    assert_eq!(store.read_at(CH, DEFAULT_MEMBER), None, "never read");
    assert_eq!(store.fetch_full(CH, DEFAULT_MEMBER).read_at, None);

    store.note_read(CH, DEFAULT_MEMBER, 1_000);
    assert_eq!(store.read_at(CH, DEFAULT_MEMBER), Some(1_000));
    assert_eq!(
        store.fetch_full(CH, DEFAULT_MEMBER).read_at,
        Some(1_000),
        "the projection carries it, so the engine needs no second read"
    );

    store.note_read(CH, DEFAULT_MEMBER, 900);
    assert_eq!(
        store.read_at(CH, DEFAULT_MEMBER),
        Some(1_000),
        "an older read must not move the clock backwards"
    );
    store.note_read(CH, DEFAULT_MEMBER, 2_000);
    assert_eq!(store.read_at(CH, DEFAULT_MEMBER), Some(2_000));

    assert_eq!(
        store.read_at(CH, 2),
        None,
        "another member's cursor is its own"
    );
    assert_eq!(
        store.read_at(2, DEFAULT_MEMBER),
        None,
        "another channel's"
    );
}

/// A compression OPENS a node at the pointer k10r served and COMPLETES it with
/// the summary (ADR-0008 §2/§4). The key is the pointer, so nothing is
/// overwritten: the previous node stays readable while this one is in flight.
#[test]
fn a_compression_opens_a_node_and_completes_it() {
    let (_d, mut store) = open();
    const CH: u64 = 1;
    for (ts, text) in [(100, "m1"), (200, "m2"), (900, "m3")] {
        store.append(CH, ts, 0, role::USER, 0, text);
    }

    store.open_checkpoint(CH, (200, 0), &[7, 8]);
    let open = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(
        open.coverage_ts, 0,
        "an open node is not a coverage point yet"
    );
    assert_eq!(open.summary, None);
    assert!(
        open.compressing,
        "the read reports the compression in flight"
    );
    assert_eq!(open.increment.len(), 3, "the whole log is still raw");

    assert_eq!(store.complete_checkpoint(CH, "m1 and m2"), Some((200, 0)));
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!((view.coverage_ts, view.coverage_sub), (200, 0));
    assert_eq!(view.summary.as_deref(), Some("m1 and m2"));
    assert!(!view.compressing);
    assert_eq!(view.head, Some(vec![7, 8]), "the frozen head rode the node");
    assert_eq!(view.increment.len(), 1, "m3 alone is after the pointer");
}

/// Nodes accumulate and the read takes the NEWEST COMPLETE one: opening above
/// it never disturbs the summary the caller is still reading, and completing
/// moves the read up one node.
#[test]
fn nodes_accumulate_and_the_read_follows_the_newest_complete_one() {
    let (_d, mut store) = open();
    const CH: u64 = 1;
    store.append(CH, 100, 0, role::USER, 0, "m1");
    store.append(CH, 200, 0, role::USER, 0, "m2");
    store.append(CH, 900, 0, role::USER, 0, "m3");

    store.append_checkpoint(CH, (200, 0), "first summary", &[1]);
    store.open_checkpoint(CH, (900, 0), &[2]);

    let v = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(v.coverage_ts, 200, "the open node does not move the read");
    assert_eq!(v.summary.as_deref(), Some("first summary"));
    assert!(v.compressing);
    assert_eq!(v.increment.len(), 1, "m3 is still raw");

    assert_eq!(
        store.complete_checkpoint(CH, "second summary"),
        Some((900, 0))
    );
    let v = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(v.coverage_ts, 900);
    assert_eq!(v.summary.as_deref(), Some("second summary"));
    assert_eq!(v.head, Some(vec![2]), "the new node carries its own head");
    assert!(v.increment.is_empty());
    assert!(!v.compressing);
}

/// Re-opening the same pointer is the same node (idempotent by key), so a
/// repeated read at an unchanged split does not grow the layer.
#[test]
fn re_opening_the_same_pointer_is_the_same_node() {
    let (_d, mut store) = open();
    const CH: u64 = 1;
    store.append(CH, 100, 0, role::USER, 0, "m1");
    store.open_checkpoint(CH, (100, 0), &[5]);
    store.open_checkpoint(CH, (100, 0), &[5]);
    assert_eq!(store.complete_checkpoint(CH, "once"), Some((100, 0)));
    let v = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(v.summary.as_deref(), Some("once"));
    assert!(!v.compressing, "one node, completed once");
}

/// A summary with nothing open has no position to land on: the store refuses
/// rather than inventing a pointer (the door turns this into a 400).
#[test]
fn a_summary_without_an_open_node_is_refused() {
    let (_d, mut store) = open();
    const CH: u64 = 1;
    store.append(CH, 100, 0, role::USER, 0, "m1");
    assert_eq!(store.complete_checkpoint(CH, "orphan"), None);
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.coverage_ts, 0);
    assert!(view.summary.is_none());
}

/// The identity layer (ADR-0009 §1): a member row maps an id to the name the
/// model sees, and messages carry the SENDER ID — so renaming a member
/// re-attributes every past message without rewriting one row of the log.
#[test]
fn members_name_their_senders_and_a_rename_rewrites_nothing() {
    let (_d, mut store) = open();
    const CH: u64 = 1;
    store.put_member(CH, 1, "小王", mem_core::member_role::HUMAN);
    store.put_member(CH, 2, "k10r", mem_core::member_role::AGENT);
    store.put_member(2, 1, "别人", mem_core::member_role::HUMAN);

    assert_eq!(store.member(CH, 1).unwrap().display_name, "小王");
    assert_eq!(
        store.member(CH, 2).unwrap().role,
        mem_core::member_role::AGENT
    );
    assert!(store.member(CH, 9).is_none());

    let members = store.members(CH);
    assert_eq!(
        members.len(),
        2,
        "channel-scoped: the other channel's row is not here"
    );
    assert_eq!(members[0].0.member_id, 1);
    assert_eq!(members[1].0.member_id, 2);

    store.append_from(CH, 100, 0, 1, 0, role::USER, 0, "你好");
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.increment[0].1.sender, 1);
    assert_eq!(
        view.increment[0].1.content, "你好",
        "the log stores raw text"
    );

    store.put_member(CH, 1, "王先生", mem_core::member_role::HUMAN);
    let view = store.fetch_full(CH, DEFAULT_MEMBER);
    assert_eq!(view.increment[0].1.sender, 1, "the id is the stable part");
    assert_eq!(store.member(CH, 1).unwrap().display_name, "王先生");
}
