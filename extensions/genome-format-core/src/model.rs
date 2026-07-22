//! Internal data model. Mirrors the four WIT records but lives outside the
//! generated `bindings` module so the parser can be exercised by `cargo
//! test` without dragging wit-bindgen scaffolding into the test build.
//!
//! `lib.rs` is the only place these types cross into the wit-generated
//! sibling structs — see `into_wit_*` there. Keeping the boundary narrow
//! means we can iterate on the parser (add fields, rename, restructure)
//! without touching the WIT contract until we're happy with the shape.

#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub records: Vec<Record>,
    pub features: Vec<Feature>,
    pub qualifiers: Vec<Qualifier>,
    pub sequences: Vec<Sequence>,
}

#[derive(Debug, Clone)]
pub struct Record {
    pub record_id: String,
    pub accession: String,
    pub version: String,
    pub organism: String,
    pub length: u32,
}

#[derive(Debug, Clone)]
pub struct Feature {
    pub record_id: String,
    pub feature_index: u32,
    pub feature_type: String,
    pub start_position: u32,
    pub end_position: u32,
    pub strand: i8,
}

#[derive(Debug, Clone)]
pub struct Qualifier {
    pub record_id: String,
    pub feature_index: u32,
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Sequence {
    pub record_id: String,
    pub data: String,
}

/// Mirror of the WIT `parse-error` variant. The Guest impl converts this
/// into the wit-generated sibling at the boundary. `UnsupportedVersion`
/// is unused today (there's no producer test that trips it yet), but the
/// WIT commits to it so it stays in the enum.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ParseError {
    Malformed(String),
    UnsupportedVersion(String),
}
