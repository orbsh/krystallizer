//! The code store's contract (gravity ADR-0001/0003): identity is a
//! surrogate id (no name, no hash), what the code IS (language,
//! description, parameter definition) rides the row with the source, ids
//! are assigned by the store and recovered on reopen, and the pool read
//! carries what a skill-to-code link is decided from. The query round trip
//! is locked, not just the bytes (okm junction lesson: byte-pass +
//! query-empty is the classic silent corruption shape).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use mem_core::code::{CodeRow, CodeStore};
use okm_core::KeyEncode;

/// Layout lock: the BARE key payload encodes as `[code_id 8B BE]` — a
/// fixed-width identity, no sort field. The ns (35) + primary slot live
/// in the ADDRESS header the Collection joins at entry time, not in the
/// key struct. Changing the layout breaks this test first, NOT stored
/// data.
#[test]
fn key_layout_is_hex_locked() {
    let key = mem_core::CodeKey { code_id: 7 };
    let buf = key.encode();
    assert_eq!(buf.len(), 8, "code_id 8B");
    assert_eq!(&buf[0..8], &7u64.to_be_bytes(), "code_id");
}

/// The write/read round trip: ids are assigned in order and the row comes
/// back field for field — language, description, parameter definition and
/// source. The source is re-read on every call (fidelity is the
/// contract); the description and the parameters are what a link is
/// decided from, so they must survive too.
#[test]
fn put_assigns_ids_and_get_round_trips_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = CodeStore::open(dir.path()).unwrap();

    assert_eq!(
        store.put(
            "nu",
            "log into a host via ssh",
            r#"{"host": "string", "user": "string"}"#,
            "ssh $args.host",
        ),
        1
    );
    assert_eq!(
        store.put("python", "sum two ints", r#"{"n": "int"}"#, "print(1)"),
        2
    );

    let row = store.get(1).expect("row 1 present");
    assert_eq!(
        row,
        CodeRow {
            language: "nu".into(),
            description: "log into a host via ssh".into(),
            parameters: r#"{"host": "string", "user": "string"}"#.into(),
            source: "ssh $args.host".into(),
        }
    );
    assert!(store.get(3).is_none(), "unknown id = absent, not an error");
}

/// Ids are recovered from storage: a fresh process must not reuse an id
/// (that would silently overwrite stored code).
#[test]
fn ids_are_recovered_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store = CodeStore::open(dir.path()).unwrap();
        assert_eq!(store.put("nu", "first", "{}", "first"), 1);
        assert_eq!(store.put("nu", "second", "{}", "second"), 2);
    }
    let mut store = CodeStore::open(dir.path()).unwrap();
    assert_eq!(
        store.put("python", "third", "{}", "third"),
        3,
        "continues past the max"
    );
    assert_eq!(
        store.get(1).unwrap().source,
        "first",
        "id 1 was not overwritten"
    );
}
