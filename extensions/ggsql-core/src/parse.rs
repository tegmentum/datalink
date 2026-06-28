//! DB-neutral `VISUALIZE` parse/rewrite logic for `ggsql`.
//!
//! Lifted VERBATIM (behaviour-preserving) from the hand-written ducklink
//! `extensions/ggsql-component/src/parse.rs`. This is the rewrite engine
//! both DB ports share: text in, SQL text out. No DB-specific dispatch,
//! no WIT types, no `std` — so it compiles natively for fuzzing and into
//! either the `duckdb:extension` or `sqlite:extension` shim unchanged.
//!
//! TRUST BOUNDARY: the host hands this fully attacker-controlled text of
//! any statement the built-in parser rejected. The contract: NEVER PANIC
//! on any input — garbage, huge, multi-byte, deeply nested, or
//! adversarial statements all come back as `Declined` / `Invalid` / a
//! (possibly nonsensical) `Rewrite` string, never an abort.

use alloc::format;
use alloc::string::String;

/// Outcome of offering a statement to the ggsql parser, in neutral terms.
/// Each shim maps these onto its DB's surface:
///   * ducklink -> `parser-dispatch.parse-outcome` (`declined` / `rewrite`)
///     + `duckerror::invalidargument` for `Invalid`.
///   * sqlink   -> the `__sqlink_parse(text)->text` scalar return: `Declined`
///     becomes NULL (the host treats NULL/empty as "decline"), `Rewrite`
///     becomes the SQL text, `Invalid` becomes an `Err` (a clean parse error).
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not a `VISUALIZE` statement; the host proceeds to the next parser
    /// extension / its own parse error.
    Declined,
    /// A malformed `VISUALIZE` (e.g. no inner select); surfaced as a
    /// clean parse error carrying this message.
    Invalid(String),
    /// The statement is claimed and rewritten to this ordinary SQL.
    Rewrite(String),
}

/// Keyword we intercept, lower-case. Exactly 9 ASCII bytes.
const KW: &str = "visualize";

/// Parse/rewrite a single statement the built-in parser rejected. Pure,
/// total, and panic-free for every `&str`.
pub fn parse_visualize(query: &str) -> Outcome {
    let trimmed = query.trim().trim_end_matches(';').trim();

    // Case-insensitive `VISUALIZE` prefix check over the FIRST `KW.len()`
    // chars. `eq_ignore_ascii_case` is false when the byte lengths differ,
    // so a head holding any multi-byte char (byte length != 9) can never
    // match. A match therefore guarantees the head is exactly 9 ASCII bytes
    // => byte index `KW.len()` is a valid char boundary and the slice below
    // cannot panic.
    let head: String = trimmed.chars().take(KW.len()).collect();
    if !head.eq_ignore_ascii_case(KW) {
        return Outcome::Declined;
    }

    let inner = trimmed[KW.len()..].trim();
    if inner.is_empty() {
        return Outcome::Invalid(String::from(
            "VISUALIZE requires a SELECT statement, e.g. VISUALIZE SELECT region, n FROM t",
        ));
    }

    // Rewrite: wrap the inner select as a CTE and emit a (label, n, bar)
    // rollup. The inner select is expected to project (label, value); we
    // render a unit bar of '#' repeated by value. This desugars entirely
    // to standard SQL -- the whole point of the string->SQL rewrite form.
    // A nonsensical `inner` yields nonsensical-but-syntactically-embedded
    // SQL; the host's binder is what rejects it (cleanly), not us.
    let rewritten = format!(
        "WITH __viz AS ({inner}) \
         SELECT CAST(label AS VARCHAR) AS label, \
                CAST(n AS BIGINT) AS n, \
                repeat('#', GREATEST(CAST(n AS BIGINT), 0)) AS bar \
         FROM (SELECT * FROM __viz) AS t(label, n) \
         ORDER BY n DESC"
    );
    Outcome::Rewrite(rewritten)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn declines_non_visualize() {
        assert_eq!(parse_visualize("SELECT 1"), Outcome::Declined);
        assert_eq!(parse_visualize(""), Outcome::Declined);
        assert_eq!(parse_visualize("   "), Outcome::Declined);
        assert_eq!(parse_visualize("vis"), Outcome::Declined);
    }

    #[test]
    fn rewrites_visualize_case_insensitive() {
        for q in [
            "VISUALIZE SELECT a, b FROM t",
            "visualize select a,b from t;",
            "  ViSuAlIzE SELECT 1, 2 ; ",
        ] {
            match parse_visualize(q) {
                Outcome::Rewrite(sql) => assert!(sql.contains("__viz")),
                other => panic!("expected rewrite, got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_inner_is_invalid() {
        assert!(matches!(parse_visualize("VISUALIZE"), Outcome::Invalid(_)));
        assert!(matches!(parse_visualize("VISUALIZE ;"), Outcome::Invalid(_)));
        assert!(matches!(parse_visualize("  visualize   "), Outcome::Invalid(_)));
    }

    #[test]
    fn multibyte_prefix_does_not_panic() {
        assert_eq!(parse_visualize("visualizé SELECT 1"), Outcome::Declined);
        assert_eq!(parse_visualize("v\u{0131}sualize SELECT 1"), Outcome::Declined);
        let _ = parse_visualize("visual\u{2603}ze SELECT 1");
        let _ = parse_visualize("\u{1F4A9}\u{1F4A9}\u{1F4A9}\u{1F4A9}\u{1F4A9}");
    }
}
