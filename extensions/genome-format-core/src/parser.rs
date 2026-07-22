//! Handwritten GenBank parser — flat NCBI flat-file format into four
//! relations (records / features / qualifiers / sequences).
//!
//! Scope: cover the shape Synphage's inputs actually take (linear or circular
//! DNA, single or multi-record files, standard qualifiers, no HTGS / draft
//! phase-1 shenanigans). The GenBank spec has many corners; this parser is
//! deliberately not one of them yet.
//!
//! # What it handles
//! - LOCUS line: `record_id` (name) and `length` (bp before the "bp" token).
//! - ACCESSION, VERSION (first token on the line — the "primary" values).
//! - ORGANISM (first line of the ORGANISM block; the taxonomy lineage is
//!   dropped because SQL rarely wants a semicolon-joined blob).
//! - FEATURES table:
//!   - feature type is columns 6..21 of the header line;
//!   - location is columns 22..end plus any continuation lines that don't
//!     start with `/`;
//!   - qualifiers begin with `/name=value` (or `/name` alone for pseudo);
//!   - quoted qualifier values may span multiple lines — quotes are joined
//!     with a single space between fragments (matches Biopython behaviour).
//! - ORIGIN block: strips positions and whitespace, keeps [A-Za-z] as bytes.
//! - Records are separated by `//` on its own line.
//!
//! # What it does NOT handle (yet)
//! - Multi-line DEFINITION / COMMENT / REFERENCE — those aren't in the four
//!   relations the WIT exposes, so they're skipped, not preserved.
//! - CONTIG assemblies (records whose sequence is defined as a join of
//!   other accessions) — the ORIGIN block will simply be absent and the
//!   record's `data` string will be empty.
//! - Non-ASCII sequence characters (IUPAC ambiguity codes ARE ASCII so
//!   they're fine). Any non-ASCII input surfaces as a `Malformed` error.

use crate::location;
use crate::model::{Feature, ParseError, Parsed, Qualifier, Record, Sequence};

/// Entry point. Parses zero, one, or many `LOCUS…//` records.
pub fn parse_genbank(data: &[u8]) -> Result<Parsed, ParseError> {
    let text =
        std::str::from_utf8(data).map_err(|e| ParseError::Malformed(format!("not utf-8: {e}")))?;

    let lines: Vec<&str> = text.lines().collect();
    let mut out = Parsed::default();
    let mut i = 0usize;

    while i < lines.len() {
        // Skip anything until the next LOCUS. Multi-record files sometimes
        // have blank lines or stray whitespace between `//` and the next
        // record — that's fine.
        while i < lines.len() && !starts_field(lines[i], "LOCUS") {
            i += 1;
        }
        if i >= lines.len() {
            break;
        }
        i = parse_one_record(&lines, i, &mut out)?;
    }

    Ok(out)
}

/// Parse one LOCUS..// block starting at `start`. Appends into `out`.
/// Returns the index of the line AFTER the terminating `//` (or lines.len()
/// if the file ended without one — a truncated final record is tolerated).
fn parse_one_record(lines: &[&str], start: usize, out: &mut Parsed) -> Result<usize, ParseError> {
    let (record_id, length) = parse_locus(lines[start])?;
    let mut accession = String::new();
    let mut version = String::new();
    let mut organism = String::new();

    let mut i = start + 1;
    while i < lines.len() {
        let line = lines[i];

        if line == "//" {
            i += 1;
            break;
        }

        if starts_field(line, "ACCESSION") {
            if accession.is_empty() {
                accession = first_token_after_key(line, "ACCESSION").to_string();
            }
            i += 1;
        } else if starts_field(line, "VERSION") {
            if version.is_empty() {
                version = first_token_after_key(line, "VERSION").to_string();
            }
            i += 1;
        } else if starts_field(line, "SOURCE") {
            // The ORGANISM sub-field lives on the next line, indented by
            // two spaces (`  ORGANISM  ...`). It's the value we actually
            // want — SOURCE itself is a shorter display name.
            i += 1;
            while i < lines.len() && lines[i].starts_with(' ') {
                if lines[i].starts_with("  ORGANISM") && organism.is_empty() {
                    organism = lines[i]["  ORGANISM".len()..].trim().to_string();
                }
                i += 1;
            }
        } else if starts_field(line, "FEATURES") {
            i = parse_features(lines, i + 1, &record_id, out);
        } else if starts_field(line, "ORIGIN") {
            let (data, next) = parse_origin(lines, i + 1);
            out.sequences.push(Sequence {
                record_id: record_id.clone(),
                data,
            });
            i = next;
        } else {
            // Unrecognised or unwanted top-level field. Skip its
            // continuation lines (any following lines that begin with
            // whitespace) so we don't misread them as new fields.
            i += 1;
            while i < lines.len() && lines[i].starts_with(' ') {
                i += 1;
            }
        }
    }

    out.records.push(Record {
        record_id,
        accession,
        version,
        organism,
        length,
    });

    Ok(i)
}

/// True iff `line` starts with `key` at column 0 (GenBank top-level fields
/// are always left-justified — indented lines are continuations).
fn starts_field(line: &str, key: &str) -> bool {
    line.len() >= key.len() && line.as_bytes()[0] != b' ' && line.starts_with(key)
}

/// `LOCUS       NAME        1000 bp    DNA     linear …`
///     -> ("NAME", 1000).
fn parse_locus(line: &str) -> Result<(String, u32), ParseError> {
    // Everything after the LOCUS keyword, whitespace-split. The name is
    // the first token; the length is the token immediately preceding "bp"
    // (or "aa" for protein records). Some producers omit spaces between
    // fields; the "find the token before bp" strategy is robust to that.
    let rest = line
        .strip_prefix("LOCUS")
        .ok_or_else(|| ParseError::Malformed(format!("missing LOCUS keyword: {line}")))?;
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    if tokens.is_empty() {
        return Err(ParseError::Malformed("empty LOCUS line".to_string()));
    }
    let record_id = tokens[0].to_string();

    let mut length: u32 = 0;
    for (idx, tok) in tokens.iter().enumerate() {
        if (*tok == "bp" || *tok == "aa") && idx > 0 {
            if let Ok(n) = tokens[idx - 1].parse::<u32>() {
                length = n;
                break;
            }
        }
    }
    Ok((record_id, length))
}

/// Value on a header line: whitespace-tolerant. `ACCESSION   NC_009925 X`
///   -> `"NC_009925"`. Empty string if the line has only the key.
fn first_token_after_key<'a>(line: &'a str, key: &str) -> &'a str {
    line.strip_prefix(key)
        .map(|rest| rest.trim())
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("")
}

/// Parse the FEATURES table starting at line `start`. Returns the index of
/// the first line that is no longer part of the table (ORIGIN, `//`, or
/// another top-level field).
fn parse_features(
    lines: &[&str],
    start: usize,
    record_id: &str,
    out: &mut Parsed,
) -> usize {
    let mut i = start;
    let mut feature_index: u32 = 0;

    // A feature-header line looks like:
    //   "     source          1..1000"
    //   "     CDS             complement(join(1..100,200..300))"
    // i.e. 5 spaces, then the feature type in columns 5..21, then location.
    // Continuation lines start with 21+ spaces.
    while i < lines.len() {
        let line = lines[i];

        // End of features block: another top-level field (starts at col 0)
        // or the record terminator.
        if !line.starts_with(' ') || line == "//" {
            return i;
        }

        // Must be indented AT LEAST 5 spaces to be a feature header. A
        // 21-space-indented line here would be a stray continuation (which
        // shouldn't happen — but be defensive).
        if !starts_at_col(line, 5) {
            i += 1;
            continue;
        }

        // Feature type: columns 5..21. GenBank pads to that width even for
        // short names like "gene". Use whichever is shorter — the line may
        // be shorter than 21 chars in pathological cases.
        let end_of_type = 21.min(line.len());
        let feature_type = line[5..end_of_type].trim().to_string();
        if feature_type.is_empty() {
            i += 1;
            continue;
        }

        // Location: columns 21.. on the same line, plus any following
        // continuation lines that DON'T start with `/`.
        let mut location = if line.len() > 21 {
            line[21..].trim().to_string()
        } else {
            String::new()
        };
        i += 1;
        while i < lines.len() {
            let cont = lines[i];
            if !starts_at_col(cont, 21) {
                break;
            }
            let stripped = cont.trim_start();
            if stripped.starts_with('/') {
                break;
            }
            location.push_str(stripped);
            i += 1;
        }

        let iv = location::parse(&location);

        out.features.push(Feature {
            record_id: record_id.to_string(),
            feature_index,
            feature_type,
            start_position: iv.start,
            end_position: iv.end,
            strand: iv.strand,
        });

        // Preserve the raw location string as a synthetic qualifier so
        // SQL callers can inspect `join(...)` / cross-refs the interval
        // collapse loses. Cheap; a few dozen bytes per feature.
        if !location.is_empty() {
            out.qualifiers.push(Qualifier {
                record_id: record_id.to_string(),
                feature_index,
                name: "_location".to_string(),
                value: location,
            });
        }

        // Qualifiers: any run of lines at col >= 21 starting with `/`.
        i = parse_qualifiers(lines, i, record_id, feature_index, out);

        feature_index += 1;
    }

    i
}

/// A line starts "at column c" if its first `c` bytes are spaces AND byte
/// `c` (if present) is not a space. Uses byte indexing — GenBank is ASCII.
fn starts_at_col(line: &str, c: usize) -> bool {
    let bytes = line.as_bytes();
    if bytes.len() <= c {
        return false;
    }
    if bytes[c] == b' ' {
        return false;
    }
    bytes[..c].iter().all(|b| *b == b' ')
}

/// Parse `/name=value` qualifiers (and standalone `/pseudo`-style flags)
/// starting at `start`. Returns the first index that is no longer a
/// qualifier line for this feature.
fn parse_qualifiers(
    lines: &[&str],
    start: usize,
    record_id: &str,
    feature_index: u32,
    out: &mut Parsed,
) -> usize {
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        // Qualifier lines are indented ~21 spaces and start with '/'.
        // If we hit a less-indented line (feature header, ORIGIN, //, or
        // top-level field) we're done.
        if !line.starts_with(' ') || !line.trim_start().starts_with('/') {
            return i;
        }
        if !starts_at_col(line, 21) {
            return i;
        }

        // Strip the leading '/'.
        let body = &line.trim_start()[1..];
        let (name, first_value) = match body.find('=') {
            Some(eq) => (body[..eq].to_string(), body[eq + 1..].to_string()),
            None => (body.to_string(), String::new()),
        };
        i += 1;

        // Collect continuation lines. A continuation is a col-21 indented
        // line that does NOT start with '/'. Value quoting: leading '"'
        // opens a multi-line string; the closing '"' may be many lines
        // later. Unquoted values (mostly numeric) don't continue.
        let mut value = first_value;
        if value.starts_with('"') {
            // Consume up to the closing quote.
            while i < lines.len() {
                if closes_quoted_value(&value) {
                    break;
                }
                let cont = lines[i];
                if !starts_at_col(cont, 21) {
                    break;
                }
                let stripped = cont.trim_start();
                if stripped.starts_with('/') {
                    // A new qualifier starts — the previous value was
                    // never properly closed. Emit what we have.
                    break;
                }
                // Biopython joins /translation continuations with no
                // separator (it's one long peptide) and everything else
                // with a single space. We can't tell them apart cheaply
                // from just the qualifier name at this point, so use the
                // more common rule (space) — /translation callers can
                // `regexp_replace(value, ' ', '')` in SQL. Acceptable
                // for a first slice; note it here so the delta doesn't
                // surprise anyone comparing to Biopython output.
                value.push(' ');
                value.push_str(stripped);
                i += 1;
            }
            // Strip enclosing quotes if present at both ends.
            value = unquote(&value);
        }

        out.qualifiers.push(Qualifier {
            record_id: record_id.to_string(),
            feature_index,
            name,
            value,
        });
    }
    i
}

/// A quoted qualifier value closes on an unescaped final '"'. GenBank
/// escapes embedded quotes as "" (doubled), same as SQL string literals.
fn closes_quoted_value(v: &str) -> bool {
    if v.len() < 2 || !v.starts_with('"') {
        return false;
    }
    // Count trailing quotes — an odd number means the final one closes.
    let trailing = v.bytes().rev().take_while(|b| *b == b'"').count();
    trailing % 2 == 1
}

fn unquote(v: &str) -> String {
    let trimmed = v.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        // Also collapse the GenBank "" -> " escape.
        trimmed[1..trimmed.len() - 1].replace("\"\"", "\"")
    } else {
        trimmed.to_string()
    }
}

/// Parse the ORIGIN sequence block. Format:
/// ```text
///         1 atgaaacgca ttagcaccac cattaccacc accatcacca ttaccacagg taacggtgcg
///        61 ggctgaaaaa
/// //
/// ```
/// Returns the joined sequence (letters only, lowercase preserved) and the
/// index of the line after the terminating `//` (or the first non-sequence
/// line, so the caller can decide).
fn parse_origin(lines: &[&str], start: usize) -> (String, usize) {
    let mut data = String::new();
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        if line == "//" {
            // Caller will re-see this and terminate the record.
            return (data, i);
        }
        if !line.starts_with(' ') && !line.is_empty() {
            // A new top-level field — sequence block ended early.
            return (data, i);
        }
        for c in line.chars() {
            if c.is_ascii_alphabetic() {
                data.push(c);
            }
        }
        i += 1;
    }
    (data, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny but representative record — two features, two qualifiers, a
    // short ORIGIN, and a `//` terminator. If this doesn't round-trip
    // sensibly, real inputs won't either.
    const SAMPLE: &str = "\
LOCUS       TEST0001                  30 bp    DNA     linear   UNK 01-JAN-2026
ACCESSION   TEST0001
VERSION     TEST0001.1
SOURCE      Escherichia coli
  ORGANISM  Escherichia coli
            Bacteria; Proteobacteria.
FEATURES             Location/Qualifiers
     source          1..30
                     /organism=\"Escherichia coli\"
                     /mol_type=\"genomic DNA\"
     gene            complement(5..25)
                     /gene=\"thrL\"
                     /locus_tag=\"b0001\"
ORIGIN
        1 atgaaacgca ttagcaccac cattacca
//
";

    #[test]
    fn parses_one_record() {
        let parsed = parse_genbank(SAMPLE.as_bytes()).unwrap();
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].record_id, "TEST0001");
        assert_eq!(parsed.records[0].accession, "TEST0001");
        assert_eq!(parsed.records[0].version, "TEST0001.1");
        assert_eq!(parsed.records[0].organism, "Escherichia coli");
        assert_eq!(parsed.records[0].length, 30);
    }

    #[test]
    fn parses_features() {
        let parsed = parse_genbank(SAMPLE.as_bytes()).unwrap();
        assert_eq!(parsed.features.len(), 2);
        assert_eq!(parsed.features[0].feature_type, "source");
        assert_eq!(parsed.features[0].start_position, 1);
        assert_eq!(parsed.features[0].end_position, 30);
        assert_eq!(parsed.features[0].strand, 1);

        assert_eq!(parsed.features[1].feature_type, "gene");
        assert_eq!(parsed.features[1].start_position, 5);
        assert_eq!(parsed.features[1].end_position, 25);
        assert_eq!(parsed.features[1].strand, -1);
    }

    #[test]
    fn parses_qualifiers() {
        let parsed = parse_genbank(SAMPLE.as_bytes()).unwrap();
        // 2 real qualifiers + 1 synthetic /_location on the source feature,
        // 2 real + 1 synthetic on the gene feature = 6.
        let organism = parsed
            .qualifiers
            .iter()
            .find(|q| q.feature_index == 0 && q.name == "organism")
            .expect("organism qualifier present");
        assert_eq!(organism.value, "Escherichia coli");

        let gene = parsed
            .qualifiers
            .iter()
            .find(|q| q.feature_index == 1 && q.name == "gene")
            .expect("gene qualifier present");
        assert_eq!(gene.value, "thrL");
    }

    #[test]
    fn parses_sequence() {
        let parsed = parse_genbank(SAMPLE.as_bytes()).unwrap();
        assert_eq!(parsed.sequences.len(), 1);
        assert_eq!(
            parsed.sequences[0].data,
            "atgaaacgcattagcaccaccattacca"
        );
    }

    #[test]
    fn multi_record_file() {
        let two = format!("{SAMPLE}\n{SAMPLE}");
        let parsed = parse_genbank(two.as_bytes()).unwrap();
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.features.len(), 4);
        assert_eq!(parsed.sequences.len(), 2);
    }

    #[test]
    fn empty_input_is_empty_result() {
        let parsed = parse_genbank(b"").unwrap();
        assert_eq!(parsed.records.len(), 0);
    }

    #[test]
    fn non_utf8_is_malformed() {
        let mut bad = SAMPLE.as_bytes().to_vec();
        bad.push(0xFF);
        bad.push(0xFE);
        let err = parse_genbank(&bad).unwrap_err();
        matches!(err, ParseError::Malformed(_));
    }
}
