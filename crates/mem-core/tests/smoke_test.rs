//! Phase 0 smoke test: store + search over real Fjall on disk, plus key
//! layout stability (ADR-0002: hex stability test locks the layout).

use mem_core::{MemoryKey, MemoryStore};
use okm_core::KeyEncode;

#[test]
fn store_and_search_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut mem = MemoryStore::open(dir.path()).unwrap();

    let id1 = mem.store(101, "krystallizer uses Fjall for storage", 1_000, &[]);
    let _id2 = mem.store(101, "unrelated note about rust macros", 1_001, &[]);
    let _id3 = mem.store(202, "krystallizer note from another user", 1_002, &[]);

    let hits = mem.search(101, "Fjall");
    assert_eq!(hits.len(), 1, "user-scoped: other users' rows excluded");
    assert_eq!(hits[0].0, id1);
    assert!(hits[0].1.contains("Fjall"));

    assert!(mem.search(101, "krystallizer").len() == 1);
    assert!(mem.search(202, "nonexistent").is_empty());
    assert!(mem.search(303, "krystallizer").is_empty());
    assert!(mem.search(101, "").is_empty());
}

#[test]
fn reopens_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut mem = MemoryStore::open(dir.path()).unwrap();
        mem.store(7, "persisted across reopen", 42, &[]);
        mem.persist().unwrap();
    }
    let mem = MemoryStore::open(dir.path()).unwrap();
    assert_eq!(mem.search(7, "persisted").len(), 1);
}

#[test]
fn next_id_recovered_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut mem = MemoryStore::open(dir.path()).unwrap();
    let id1 = mem.store(101, "first", 1, &[]);
    mem.store(202, "second", 2, &[]);
    mem.persist().unwrap();
    drop(mem);

    // Reopen: the recovered counter must continue past the highest
    // stored id — no reuse, no silent overwrite.
    let mut mem = MemoryStore::open(dir.path()).unwrap();
    let id3 = mem.store(101, "after reopen", 3, &[]);
    assert!(id3 > id1);
    mem.persist().unwrap();
    drop(mem);

    let mem = MemoryStore::open(dir.path()).unwrap();
    assert_eq!(mem.search(101, "first").len(), 1, "id1 not overwritten");
    assert_eq!(mem.search(101, "after reopen").len(), 1);
}

#[test]
fn forget_list_stats() {
    let dir = tempfile::tempdir().unwrap();
    let mut mem = MemoryStore::open(dir.path()).unwrap();
    let id1 = mem.store(101, "first memory", 1, &[]);
    let id2 = mem.store(101, "second memory", 2, &[]);
    mem.store(202, "other user memory", 3, &[]);

    // list: user-scoped, insert order
    let listed = mem.list(101);
    assert_eq!(
        listed,
        vec![(id1, "first memory".into()), (id2, "second memory".into())]
    );
    assert_eq!(mem.list(303), Vec::new());

    // stats: per-user counts + total
    let stats = mem.stats();
    assert_eq!(stats.total, 3);
    assert_eq!(stats.per_user.get(&101), Some(&2));
    assert_eq!(stats.per_user.get(&202), Some(&1));

    // forget: removes exactly one row, other users untouched
    assert!(mem.forget(101, id1));
    assert!(!mem.forget(101, id1), "second forget finds nothing");
    assert!(
        !mem.forget(303, id1),
        "another user cannot forget someone else's row"
    );
    assert_eq!(mem.list(101), vec![(id2, "second memory".into())]);
    assert_eq!(mem.stats().total, 2);
}

/// The row carries the vector the writer stated: it is a FIELD (it
/// survives a reopen), any length, and an empty slice is a legal "no
/// vector" — distinct from a zero vector by its frame count.
#[test]
fn the_row_carries_the_vector_the_writer_stated() {
    let dir = tempfile::tempdir().unwrap();
    let mut mem = MemoryStore::open(dir.path()).unwrap();

    let id = mem.store(9, "embedded row", 5, &[0.25, -0.5, 0.75]);
    let row = mem.get(9, id).expect("row reads back");
    assert_eq!(row.text, "embedded row");
    assert_eq!(row.embedding.len(), 3);
    assert_eq!(row.embedding.elems, vec![0.25, -0.5, 0.75]);

    let bare = mem.store(9, "no vector", 6, &[]);
    let bare_row = mem.get(9, bare).unwrap();
    assert!(bare_row.embedding.is_empty());

    // Same length as the width an engine emits, so the frame really is
    // count-then-elements rather than one byte per float.
    let wide = mem.store(9, "wide", 7, &[1.0; 512]);
    assert_eq!(mem.get(9, wide).unwrap().embedding.len(), 512);

    mem.persist().unwrap();
    drop(mem);
    let mem = MemoryStore::open(dir.path()).unwrap();
    assert_eq!(
        mem.get(9, id).unwrap().embedding.elems,
        vec![0.25, -0.5, 0.75]
    );
    assert_eq!(mem.get(9, wide).unwrap().embedding.len(), 512);
}

#[test]
fn key_layout_hex_stability() {
    let key = MemoryKey {
        user_id: 0x0102_0304_0506_0708,
        id: 0x1122_3344_5566_7788,
    };
    let hex: String = key.encode().iter().map(|b| format!("{b:02x}")).collect();
    // Payload only (ns header excluded by KeyEncode contract): [user_id 8B be][id 8B be].
    assert_eq!(hex, "01020304050607081122334455667788");
}
