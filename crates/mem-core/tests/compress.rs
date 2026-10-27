//! Phase 2.6's compression policy: the three signals as PURE functions
//! (ADR-0008 §4). No store, no clock of its own — the caller states the
//! assembled size and the idle time, so every case below is a table
//! entry rather than a fixture.
//!
//! The cases lock the decisions the design made, not just "it fires":
//! the budget is a mid band (not 90%), the ceiling is the floor, the
//! silence that fired the cache clock can never be its own split, ties
//! resolve to the earlier gap so one corpus always answers identically,
//! and a span with no internal gap splits at its end.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use mem_core::channel::{MessageKey, MessageRow, SessionView};
use mem_core::compress::{evaluate, Params, Trigger};

const PARAMS: Params = Params {
    context_bytes: 1000,
    budget_percent: 50,
    cache_ttl_ms: 60_000,
    floor_bytes: 100,
};

fn entry(ts_ms: u64, content: &str) -> (MessageKey, MessageRow) {
    (
        MessageKey {
            channel_id: 1,
            ts_ms,
            sub: 0,
        },
        MessageRow {
            sender: 0,
            end_ts_ms: 0,
            role: 0,
            kind: 0,
            content: content.to_string(),
        },
    )
}

fn view(ts_list: &[u64]) -> SessionView {
    SessionView {
        coverage_ts: 0,
        coverage_sub: 0,
        summary: None,
        suggested_ts: 0,
        suggested_sub: 0,
        head: None,
        compressing: false,
        increment: ts_list.iter().map(|ts| entry(*ts, "x")).collect(),
        read_at: None,
    }
}

/// Budget fires at the mid band and the split is the LARGEST internal
/// gap — the span ends at the last record before it.
#[test]
fn budget_fires_and_splits_at_the_largest_gap() {
    let v = view(&[100, 200, 900]);
    let verdict = evaluate(&v, 600, None, &PARAMS).expect("budget fired");
    assert_eq!(verdict.trigger, Trigger::Budget);
    assert_eq!(verdict.checkpoint_ts, 200, "span ends before the 700ms gap");
    assert_eq!(verdict.max_gap_ms, Some(700));
    assert_eq!(verdict.assembled_bytes, 600);
}

/// Below the mid band nothing fires — 40% of the window is still room.
#[test]
fn budget_stays_silent_below_the_mid_band() {
    let v = view(&[100, 200, 900]);
    assert!(evaluate(&v, 400, None, &PARAMS).is_none());
}

/// The floor outranks every trigger: a short view is all signal, and a
/// summary of it costs more than it saves.
#[test]
fn the_floor_silences_every_trigger() {
    let v = view(&[100, 200, 900]);
    assert!(evaluate(&v, 50, Some(10 * PARAMS.cache_ttl_ms), &PARAMS).is_none());
}

/// The cache clock speaks only when a clock rode the read: no cursor (or
/// no ts on this read) means idle is unknown, and an unknown idle is not
/// an idle of zero.
#[test]
fn the_cache_clock_needs_a_clock() {
    let v = view(&[100, 200, 900]);
    assert!(evaluate(&v, 200, None, &PARAMS).is_none());
    let idle = evaluate(&v, 200, Some(59_999), &PARAMS);
    assert!(idle.is_none(), "just under the TTL");
    let fired = evaluate(&v, 200, Some(60_000), &PARAMS).expect("TTL reached");
    assert_eq!(fired.trigger, Trigger::CacheClock);
    assert_eq!(fired.idle_ms, Some(60_000));
}

/// Both signals reaching their mark is its own record — the tuning pass
/// needs to tell the two apart from the corroborated case.
#[test]
fn both_signals_are_recorded_as_both() {
    let v = view(&[100, 200, 900]);
    let verdict = evaluate(&v, 600, Some(90_000), &PARAMS).unwrap();
    assert_eq!(verdict.trigger, Trigger::Both);
}

/// A span with no internal gap (one record, or every record in one
/// millisecond) has no topic change to split on, so its END is the
/// split — not "no verdict".
#[test]
fn a_gapless_span_splits_at_its_end() {
    let single = view(&[900]);
    let verdict = evaluate(&single, 600, None, &PARAMS).expect("budget fired");
    assert_eq!(verdict.checkpoint_ts, 900);
    assert_eq!(verdict.max_gap_ms, None);

    let same_ms = view(&[900, 900, 900]);
    let verdict = evaluate(&same_ms, 600, None, &PARAMS).unwrap();
    assert_eq!(verdict.checkpoint_ts, 900, "the last record of the span");
    assert_eq!(verdict.max_gap_ms, Some(0), "a zero gap was still measured");
}

/// Ties go to the EARLIER gap: one corpus must answer identically, and a
/// later index would depend on iteration order for no benefit.
#[test]
fn gap_ties_resolve_to_the_earlier_split() {
    let v = view(&[100, 500, 900]);
    let verdict = evaluate(&v, 600, None, &PARAMS).unwrap();
    assert_eq!(verdict.checkpoint_ts, 100);
    assert_eq!(verdict.max_gap_ms, Some(400));
}

/// An empty increment has nothing to compress, whatever the size says.
#[test]
fn an_empty_increment_never_fires() {
    let v = view(&[]);
    assert!(evaluate(&v, 10_000, Some(10 * PARAMS.cache_ttl_ms), &PARAMS).is_none());
}

/// The verdict carries the parameter snapshot, so the decision log can be
/// read later without the config that produced it.
#[test]
fn the_verdict_carries_its_parameter_snapshot() {
    let v = view(&[100, 200, 900]);
    let verdict = evaluate(&v, 600, Some(70_000), &PARAMS).unwrap();
    assert_eq!(verdict.context_bytes, 1000);
    assert_eq!(verdict.budget_percent, 50);
    assert_eq!(verdict.cache_ttl_ms, 60_000);
}
