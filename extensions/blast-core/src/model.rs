//! Plain-Rust versions of the types the WIT `tegmentum:bio/sequence-search`
//! interface used to define. Kept in this crate (not in the ducklink
//! shim) so the algorithm modules don't drag a wit-bindgen dependency.
//!
//! Shape matches the WIT record for record — a component that also wants
//! to expose the biology capability directly (via `sequence-search-only`)
//! can convert 1:1 in the shim boundary.

use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct Sequence {
    pub key: String,
    pub data: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strand {
    Plus,
    Minus,
}

/// One local alignment hit. Positions are 1-indexed and inclusive at
/// both ends, matching NCBI BLAST output. `strand` indicates whether
/// the subject was aligned as given (plus) or as its reverse complement
/// (minus); query coordinates are always on the given orientation.
#[derive(Clone, Debug)]
pub struct Hit {
    pub query_key: String,
    pub subject_key: String,
    pub query_start: u32,
    pub query_end: u32,
    pub subject_start: u32,
    pub subject_end: u32,
    pub strand: Strand,
    pub identity_count: u32,
    pub alignment_length: u32,
    pub percent_identity: f64,
    pub bit_score: f64,
    pub raw_score: f64,
    pub evalue: f64,
}

/// Scoring variant. Presets and tunable knobs — see the WIT
/// `sequence-search.wit` docstring for the K/lambda tables the tunable
/// forms are validated against.
#[derive(Clone, Debug)]
pub enum Scoring {
    BlastnDefault,
    Blastn(BlastnScoring),
    BlastpDefault,
    Blastp(BlastpScoring),
}

#[derive(Clone, Debug)]
pub struct BlastnScoring {
    pub match_reward: i32,
    pub mismatch_penalty: i32,
    pub gap_open: i32,
    pub gap_extend: i32,
    pub search_both_strands: bool,
}

#[derive(Clone, Debug)]
pub struct BlastpScoring {
    pub matrix: String,
    pub gap_open: i32,
    pub gap_extend: i32,
}

/// Filters applied at the algorithm boundary. Callers may set none,
/// one, or all; the SQL layer can filter further.
///
/// The last three fields are the recognition set the earlier DuckLink
/// `table-stream` filter-pushdown planner supported — folded into this
/// record because the row-major `runtime::TableRegistry` dispatch path
/// receives no filter channel.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SearchOptions {
    pub evalue_max: Option<f64>,
    pub max_target_seqs: Option<u32>,
    pub min_identity: Option<f64>,
    pub query_keys: Option<Vec<String>>,
    pub subject_keys: Option<Vec<String>>,
    /// One of `"plus"` or `"minus"`. Unknown labels are silently ignored
    /// (no filter applied).
    pub strand: Option<String>,
}

/// Failure modes distinct enough for a SQL error handler to branch on.
#[derive(Clone, Debug)]
pub enum SearchError {
    EmptyQueries,
    EmptySubjects,
    InvalidSequence(String),
    InvalidScoring(String),
    AlignmentFailed(String),
}
