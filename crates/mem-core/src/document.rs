//! okm's document is a PAIR of slots, and a point read names the halves it
//! wants — there is no `fetch` that hands back the whole thing.
//!
//! This crate reads the whole document at four places (a checkpoint's head, a
//! log row, an entity's open props, and the log's entry list) and they all want
//! the same composition: the row's names laid over by the dynamic segment's. The
//! rule lives here once, so a caller says "the document at this key" instead of
//! re-deriving the slots.
//!
//! The answer is a [`BTreeMap`], not okm's [`Dict`]: the props bag is a map of
//! NAMES, and the crate's places that walk it — the embedding text, the renders
//! — want one canonical order rather than the order a document happened to be
//! assembled in. `Dict` is insertion-ordered by design (okm's own contract), so
//! the sorted type is the honest one here and the conversion happens at the okm
//! door, where the map type is not a choice.

use std::collections::BTreeMap;

use okm_core::engine::storage::VirtualStorage;
use okm_core::model::index::Document;
use okm_core::model::key::KeyEncode;
use okm_core::okm_value::{Dict, Variant};
use okm_core::Collection;

/// The props bag as okm's document map: what a `put` of open names takes.
pub(crate) fn dict(props: &BTreeMap<String, Variant>) -> Dict {
    props
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// The document at `key`, both slots composed into one name map.
///
/// `None` is a key that holds NOTHING (neither slot): absence is per slot, so
/// a key carrying only the dynamic segment answers `Some` with those names.
pub(crate) fn at<S, K, R>(
    col: &Collection<S, K, R>,
    key: &K,
) -> okm_core::error::Result<Option<BTreeMap<String, Variant>>>
where
    S: VirtualStorage,
    K: KeyEncode,
    R: Document<Key = K>,
{
    // One point read per half — `get::<(parts::Row, parts::Value)>` is exactly
    // these two doors — but taken as the types each half already is, so the
    // composition stays in the map domain instead of wrapping the segment in a
    // `Variant` only to unwrap it again.
    let row = col.get_row(key)?;
    let segment = col.slot1_map(key)?;
    if row.is_none() && segment.is_none() {
        return Ok(None);
    }
    let mut names: Dict = row.map(|row| row.to_map()).unwrap_or_default();
    // `Variant::merge`'s own rule for two objects: a name both halves carry
    // recurses (a nested object keeps both sides' fields), a name only the
    // segment carries is added, and the row's own names keep their value.
    for (name, value) in segment.unwrap_or_default() {
        match names.get_mut(&name) {
            Some(slot) => slot.merge(value),
            None => {
                names.insert(name, value);
            }
        }
    }
    Ok(Some(names.into_iter().collect()))
}
