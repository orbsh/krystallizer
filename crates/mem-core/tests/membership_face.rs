//! The reverse membership read is a LEAD face (okm ADR-0054): the member id lives in
//! the key, not in a copied payload column.
//!
//! Locked at the declaration level rather than through behaviour, because behaviour was
//! already right before the change — what changed is where the bytes come from, and that
//! only shows up in the exported schema. A regression would either re-add the column
//! (two facts that can disagree) or quietly drop the face.

use mem_core::channel::{MembershipKey, MembershipRow};
use okm_core::model::faces::FaceKind;
use okm_core::schema::CollectionSchema;

#[test]
fn the_reverse_read_is_a_lead_face_over_the_key() {
    let schema = CollectionSchema::of::<MembershipKey, MembershipRow>();
    assert_eq!(
        schema.indexes.len(),
        1,
        "one declared face: the reverse read"
    );
    let face = &schema.indexes[0];
    assert_eq!(
        face.kind(),
        Some(FaceKind::Lead),
        "a payload index here would need a copied member column"
    );
    assert_eq!(face.slot, 0x5001, "the lead family, first declaration");
    assert_eq!(face.fields, vec!["member_id".to_string()]);
    assert!(
        face.includes.is_empty() && face.func.is_empty(),
        "a lead face has no payload half"
    );
    assert_eq!(
        schema.slots.declared_lead_base, 0x5001,
        "the export names the family base"
    );
}

#[test]
fn the_row_does_not_carry_a_copy_of_the_identity() {
    let schema = CollectionSchema::of::<MembershipKey, MembershipRow>();
    let payload: Vec<&str> = schema
        .hot_fields
        .iter()
        .chain(schema.cold_fields.iter())
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(payload, vec!["since_ms", "nickname"]);
    assert!(
        !payload.contains(&"member_id"),
        "the key already holds it; a column would be a second fact"
    );
}
