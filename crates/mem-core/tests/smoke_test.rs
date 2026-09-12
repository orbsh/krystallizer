//! Phase 0 smoke test: store + search over real Fjall on disk, plus key
//! layout stability (ADR-0002: hex stability test locks the layout).

use mem_core::{MemoryKey, MemoryStore};
use okm_core::KeyEncode;

#[test]
fn store_and_search_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut mem = MemoryStore::open(dir.path()).unwrap();

    let id1 = mem.store(101, "krystallizer uses Fjall for storage", 1_000);
    let _id2 = mem.store(101, "unrelated note about rust macros", 1_001);
    let _id3 = mem.store(202, "krystallizer note from another user", 1_002);

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
        mem.store(7, "persisted across reopen", 42);
        mem.persist().unwrap();
    }
    let mem = MemoryStore::open(dir.path()).unwrap();
    assert_eq!(mem.search(7, "persisted").len(), 1);
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
