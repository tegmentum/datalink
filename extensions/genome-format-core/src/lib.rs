//! Neutral core for the `genome-format` extension. A handwritten
//! GenBank parser exposed as two table-valued functions:
//!
//! - `genbank_scan(contents VARCHAR) -> (record_id, accession, version,
//!   organism, record_length, feature_index, feature_type, start_position,
//!   end_position, strand, qualifiers_json, sequence)`
//!   — parses raw GenBank text (possibly multi-record) and emits one
//!   "wide" row per feature.
//!
//! - `genbank_read_path(path VARCHAR) -> (same columns)`
//!   — reads the file at `path` inside the host's WASI filesystem
//!   view, then feeds it into the same parser. Carries a
//!   `replacement_scan = ["gb", "gbk"]` attribute so DuckDB rewrites
//!   `SELECT * FROM 'lambda.gb'` to `genbank_read_path('lambda.gb')`
//!   at parse time.
//!
//! Not `no_std` — the parser + JSON emission want alloc + std::fs.

// The declare! macro emits `alloc::vec!` / `alloc::string::String`.
extern crate alloc;

pub mod location;
pub mod model;
pub mod parser;

use std::collections::HashMap;

use datalink_extcore::NeutralValue;

use crate::model::{ParseError, Parsed, Qualifier};

datalink_extcore::declare! {
    core = Core;
    extension = "genome-format";
    version = env!("CARGO_PKG_VERSION");

    table genbank_scan(text) -> (
        record_id: text,
        accession: text,
        version: text,
        organism: text,
        record_length: int64,
        feature_index: int64,
        feature_type: text,
        start_position: int64,
        end_position: int64,
        strand: int64,
        qualifiers_json: text,
        sequence: text,
    ) [deterministic] = |args| {
        use datalink_extcore::ArgExt as _;
        let contents = args.arg_text(0, "genbank_scan")?;
        emit_from_bytes(contents.as_bytes())
    };

    table genbank_read_path(text) -> (
        record_id: text,
        accession: text,
        version: text,
        organism: text,
        record_length: int64,
        feature_index: int64,
        feature_type: text,
        start_position: int64,
        end_position: int64,
        strand: int64,
        qualifiers_json: text,
        sequence: text,
    ) [deterministic, replacement_scan = ["gb", "gbk"]] = |args| {
        use datalink_extcore::ArgExt as _;
        let arg = args.arg_text(0, "genbank_read_path")?;
        read_path_or_glob(&arg)
    };
}

/// Detect whether `s` contains a glob metacharacter that the `glob` crate
/// would expand. Cheap check — we don't want to pay the directory-walk
/// cost for the common case of a literal filesystem path.
fn is_glob(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '*' | '?' | '['))
}

/// Read one or many GenBank files. A path without glob metacharacters is
/// a single `std::fs::read`; a path WITH them is expanded via the `glob`
/// crate, each match is parsed, and the results are unioned into one row
/// stream. Matches DuckDB's own `read_csv('dir/*.csv')` / `read_parquet`
/// ergonomics so `SELECT * FROM 'phages/*.gb'` (via the replacement scan)
/// works the way users already expect from file-shaped table functions.
///
/// Sort order is `glob`'s lexicographic default so multi-file scans are
/// deterministic across runs. An empty match set is an error — matches
/// `read_csv`'s behaviour on an unmatched glob and avoids silently
/// returning zero rows when the user probably fat-fingered a path.
fn read_path_or_glob(arg: &str) -> Result<Vec<Vec<NeutralValue>>, String> {
    if !is_glob(arg) {
        let bytes = std::fs::read(arg).map_err(|e| {
            format!(
                "genbank_read_path: cannot read '{arg}': {e}. If this DuckLink build \
                 does not grant filesystem access to extensions, read the file with \
                 DuckDB's read_text and call genbank_scan(<contents>) instead."
            )
        })?;
        return emit_from_bytes(&bytes);
    }

    let entries: Vec<std::path::PathBuf> = glob::glob(arg)
        .map_err(|e| format!("genbank_read_path: bad glob '{arg}': {e}"))?
        .filter_map(|r| r.ok())
        .collect();
    if entries.is_empty() {
        return Err(format!(
            "genbank_read_path: glob '{arg}' matched no files"
        ));
    }

    // Parse + emit per file, unioning rows. The parser already tolerates
    // multi-record files, so a caller can also concatenate on the SQL
    // side if they prefer — the glob path is the ergonomic default.
    let mut rows: Vec<Vec<NeutralValue>> = Vec::new();
    for path in entries {
        let path_str = path.display().to_string();
        let bytes = std::fs::read(&path).map_err(|e| {
            format!("genbank_read_path: cannot read '{path_str}' (matched by '{arg}'): {e}")
        })?;
        let parsed = parser::parse_genbank(&bytes).map_err(|e| {
            let reason = match e {
                ParseError::Malformed(m) => m,
                ParseError::UnsupportedVersion(m) => format!("unsupported version: {m}"),
            };
            format!("genome-format: cannot parse '{path_str}': {reason}")
        })?;
        emit_rows(&parsed, &mut rows);
    }
    Ok(rows)
}

// ---- parse + wide-row emission ----------------------------------------

fn emit_from_bytes(bytes: &[u8]) -> Result<Vec<Vec<NeutralValue>>, String> {
    let parsed = parser::parse_genbank(bytes).map_err(|e| {
        let reason = match e {
            ParseError::Malformed(m) => m,
            ParseError::UnsupportedVersion(m) => format!("unsupported version: {m}"),
        };
        format!("genome-format: cannot parse GenBank: {reason}")
    })?;
    let mut rows: Vec<Vec<NeutralValue>> = Vec::new();
    emit_rows(&parsed, &mut rows);
    Ok(rows)
}

fn emit_rows(parsed: &Parsed, rows: &mut Vec<Vec<NeutralValue>>) {
    // Records may share record_ids across a multi-record file only if the
    // producer duplicated them — treat the first occurrence as authoritative
    // and let SQL callers deduplicate downstream if needed.
    let mut record_by_id: HashMap<&str, &crate::model::Record> = HashMap::new();
    for r in &parsed.records {
        record_by_id.entry(r.record_id.as_str()).or_insert(r);
    }

    let mut sequence_by_id: HashMap<&str, &str> = HashMap::new();
    for s in &parsed.sequences {
        sequence_by_id
            .entry(s.record_id.as_str())
            .or_insert(s.data.as_str());
    }

    let qual_ix = index_qualifiers(&parsed.qualifiers);

    for feat in &parsed.features {
        let rec = record_by_id.get(feat.record_id.as_str());
        let (accession, version, organism, record_length) = match rec {
            Some(r) => (
                r.accession.clone(),
                r.version.clone(),
                r.organism.clone(),
                r.length,
            ),
            None => (String::new(), String::new(), String::new(), 0u32),
        };

        let sequence = sequence_by_id
            .get(feat.record_id.as_str())
            .map(|s| {
                extract_feature_sequence(s, feat.start_position, feat.end_position, feat.strand)
            })
            .unwrap_or_default();

        // Skip our synthetic `_location` qualifier — parser escape hatch,
        // not a real GenBank qualifier, would pollute every JSON object.
        let quals: Vec<&Qualifier> = qual_ix
            .get(&(feat.record_id.clone(), feat.feature_index))
            .map(|v| v.iter().copied().filter(|q| q.name != "_location").collect())
            .unwrap_or_default();
        let qualifiers_json = qualifiers_to_json(&quals);

        rows.push(vec![
            NeutralValue::Text(feat.record_id.clone()),
            NeutralValue::Text(accession),
            NeutralValue::Text(version),
            NeutralValue::Text(organism),
            NeutralValue::Int64(record_length as i64),
            NeutralValue::Int64(feat.feature_index as i64),
            NeutralValue::Text(feat.feature_type.clone()),
            NeutralValue::Int64(feat.start_position as i64),
            NeutralValue::Int64(feat.end_position as i64),
            NeutralValue::Int64(feat.strand as i64),
            NeutralValue::Text(qualifiers_json),
            NeutralValue::Text(sequence),
        ]);
    }
}

fn index_qualifiers(qs: &[Qualifier]) -> HashMap<(String, u32), Vec<&Qualifier>> {
    let mut m: HashMap<(String, u32), Vec<&Qualifier>> = HashMap::new();
    for q in qs {
        m.entry((q.record_id.clone(), q.feature_index))
            .or_default()
            .push(q);
    }
    m
}

fn qualifiers_to_json(qs: &[&Qualifier]) -> String {
    if qs.is_empty() {
        return "{}".to_string();
    }
    let mut seen: HashMap<&str, ()> = HashMap::new();
    let mut buf = String::from("{");
    let mut first = true;
    for q in qs {
        if seen.contains_key(q.name.as_str()) {
            continue;
        }
        seen.insert(q.name.as_str(), ());
        if !first {
            buf.push(',');
        }
        first = false;
        buf.push_str(&serde_json::to_string(&q.name).unwrap_or_else(|_| "\"\"".into()));
        buf.push(':');
        buf.push_str(&serde_json::to_string(&q.value).unwrap_or_else(|_| "\"\"".into()));
    }
    buf.push('}');
    buf
}

/// Extract the DNA slice for one feature. 1-indexed inclusive positions
/// (matching NCBI convention); empty string when out-of-bounds or when a
/// fuzzy / joined location collapsed to (0, 0) during parsing.
fn extract_feature_sequence(seq: &str, start: u32, end: u32, strand: i8) -> String {
    let record_len = seq.len() as u32;
    if start < 1 || end < start || end > record_len {
        return String::new();
    }
    let s = (start - 1) as usize;
    let e = end as usize;
    let slice = &seq.as_bytes()[s..e];
    if strand == -1 {
        String::from_utf8(revcomp(slice)).unwrap_or_default()
    } else {
        String::from_utf8(slice.to_vec()).unwrap_or_default()
    }
}

fn revcomp(seq: &[u8]) -> Vec<u8> {
    seq.iter().rev().map(|&b| complement(b)).collect()
}

fn complement(b: u8) -> u8 {
    match b {
        b'A' => b'T',
        b'a' => b't',
        b'T' => b'A',
        b't' => b'a',
        b'C' => b'G',
        b'c' => b'g',
        b'G' => b'C',
        b'g' => b'c',
        b'U' => b'A',
        b'u' => b'a',
        b'N' | b'n' => b,
        _ => b'N',
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use datalink_extcore::{CapabilityKind, ExtCore};

    #[test]
    fn genbank_read_path_carries_replacement_scan_extensions() {
        let decls = <Core as ExtCore>::DECLS;
        let read_path = decls.iter().find(|d| d.name == "genbank_read_path").expect("decl present");
        assert_eq!(read_path.kind, CapabilityKind::Table);
        assert_eq!(read_path.replacement_scan_extensions, &["gb", "gbk"]);
    }

    #[test]
    fn genbank_scan_has_no_replacement_scan() {
        let decls = <Core as ExtCore>::DECLS;
        let scan = decls.iter().find(|d| d.name == "genbank_scan").expect("decl present");
        assert_eq!(scan.replacement_scan_extensions, &[] as &[&str]);
    }
}
