//! GenBank feature location parsing.
//!
//! Real GenBank locations form a small algebra:
//! ```text
//!   loc      := simple | complement(loc) | join(loc, loc, ...) | order(...) | ref
//!   simple   := N | N..N | <N..N | N..>N
//!   ref      := ACCESSION.VER:loc              (cross-record; skipped here)
//! ```
//!
//! For the first-pass biology surface the WIT collapses a location down to
//! `(start-position: u32, end-position: u32, strand: s8)` — a single interval
//! plus an orientation. That's enough to drive Synphage's conservation and
//! synteny queries (both operate on gene bounding boxes, not exon structure).
//!
//! Coverage:
//! - `100..900`                              -> (100, 900, +1)
//! - `complement(100..900)`                  -> (100, 900, -1)
//! - `100`                                   -> (100, 100, +1)
//! - `join(1..100,200..300)`                 -> (1,   300, +1)   [outer bounds]
//! - `complement(join(1..100,200..300))`     -> (1,   300, -1)
//! - `<1..100` / `100..>200` (fuzzy ends)    -> stripped, treated as exact
//!
//! TODO(edge cases for a later slice):
//! - `join(A..B,complement(C..D))` mixed-strand joins collapse to +1 here.
//! - Cross-record refs `NC_009925.1:1..100` collapse to (0,0,+1); the
//!   feature row is still emitted with the raw location string preserved as
//!   a synthetic `/_location` qualifier so SQL can filter on it.
//! - `bond(...)`, `gap(...)`, `order(...)` are treated as `join(...)`.

/// The subset of a location the WIT record cares about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interval {
    pub start: u32,
    pub end: u32,
    pub strand: i8,
}

impl Interval {
    pub const UNKNOWN: Interval = Interval {
        start: 0,
        end: 0,
        strand: 1,
    };
}

/// Parse a (possibly nested) GenBank location string into a single bounding
/// interval + strand. Never fails — unrecognised syntax collapses to
/// `Interval::UNKNOWN` so the enclosing feature row still emits.
pub fn parse(raw: &str) -> Interval {
    let s = raw.trim();
    if s.is_empty() {
        return Interval::UNKNOWN;
    }

    // complement(inner) — strip and negate strand.
    if let Some(inner) = strip_call(s, "complement") {
        let mut inner_iv = parse(inner);
        inner_iv.strand = -inner_iv.strand;
        return inner_iv;
    }

    // join/order/bond — take the outer bounds of every child interval.
    for op in ["join", "order", "bond"] {
        if let Some(inner) = strip_call(s, op) {
            return outer_bounds(inner);
        }
    }

    // Cross-record ref (ACCESSION.VER:loc) — punt for now. Downstream SQL
    // can still see the feature; the qualifier layer preserves the raw
    // string. TODO: emit a proper external-ref record when we have joined
    // GenBank inputs to test against.
    if let Some(colon) = s.find(':') {
        // Only treat as cross-ref if the part before the colon looks like
        // an accession (letters + digits + dots), not "100:200" nonsense.
        let prefix = &s[..colon];
        if prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_') {
            return Interval::UNKNOWN;
        }
    }

    parse_simple(s)
}

/// `simple := N | N..N | <N..N | N..>N`.
fn parse_simple(s: &str) -> Interval {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();

    // "100..200"
    if let Some((lo, hi)) = split_range(&cleaned) {
        return Interval {
            start: lo,
            end: hi,
            strand: 1,
        };
    }

    // Point location "100".
    if let Ok(n) = cleaned.parse::<u32>() {
        return Interval {
            start: n,
            end: n,
            strand: 1,
        };
    }

    Interval::UNKNOWN
}

fn split_range(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.split("..");
    let lo = parts.next()?.parse::<u32>().ok()?;
    let hi_str = parts.next()?;
    if parts.next().is_some() {
        // "1..100..200" — not a valid simple range.
        return None;
    }
    let hi = hi_str.parse::<u32>().ok()?;
    Some((lo, hi))
}

/// If `s` looks like `NAME(...)`, return the interior. Whitespace-tolerant.
fn strip_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let s = s.trim();
    let rest = s.strip_prefix(name)?;
    let rest = rest.trim_start();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    Some(inner)
}

/// For `join(a, b, c)` / `order(...)`: parse each comma-separated child at
/// paren-depth 0 and return the min-start / max-end across them.
fn outer_bounds(inner: &str) -> Interval {
    let mut min_start = u32::MAX;
    let mut max_end = 0u32;
    let mut strand = 1i8;
    let mut saw_any = false;

    for child in split_top_commas(inner) {
        let iv = parse(child);
        if iv == Interval::UNKNOWN {
            continue;
        }
        saw_any = true;
        if iv.start < min_start {
            min_start = iv.start;
        }
        if iv.end > max_end {
            max_end = iv.end;
        }
        // First child's strand wins — mixed-strand joins are rare and are
        // flagged as a TODO at the top of this file.
        strand = iv.strand;
    }

    if !saw_any {
        return Interval::UNKNOWN;
    }
    Interval {
        start: min_start,
        end: max_end,
        strand,
    }
}

/// Split `a, b(c, d), e` on top-level commas, ignoring commas inside parens.
fn split_top_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut last = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[last..i].trim());
                last = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[last..].trim());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple() {
        assert_eq!(
            parse("100..900"),
            Interval {
                start: 100,
                end: 900,
                strand: 1,
            }
        );
    }

    #[test]
    fn complement() {
        assert_eq!(
            parse("complement(100..900)"),
            Interval {
                start: 100,
                end: 900,
                strand: -1,
            }
        );
    }

    #[test]
    fn point() {
        assert_eq!(
            parse("42"),
            Interval {
                start: 42,
                end: 42,
                strand: 1,
            }
        );
    }

    #[test]
    fn join_outer_bounds() {
        assert_eq!(
            parse("join(1..100,200..300)"),
            Interval {
                start: 1,
                end: 300,
                strand: 1,
            }
        );
    }

    #[test]
    fn complement_join() {
        assert_eq!(
            parse("complement(join(1..100,200..300))"),
            Interval {
                start: 1,
                end: 300,
                strand: -1,
            }
        );
    }

    #[test]
    fn fuzzy_endpoints() {
        assert_eq!(
            parse("<1..>200"),
            Interval {
                start: 1,
                end: 200,
                strand: 1,
            }
        );
    }

    #[test]
    fn cross_ref_collapses() {
        assert_eq!(parse("NC_009925.1:100..900"), Interval::UNKNOWN);
    }
}
