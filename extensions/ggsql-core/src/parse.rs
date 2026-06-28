//! DB-neutral `VISUALIZE` parse/rewrite logic for `ggsql`.
//!
//! The PARSING (recognizing `VISUALIZE <select>`, extracting the inner
//! select, the never-panic contract) is fully DB-agnostic and shared.
//! Only the emitted rewrite's DIALECT differs — SQLite has no
//! `repeat()` / `GREATEST()` and spells its cast types differently from
//! DuckDB — so the tiny dialect surface (`varchar` / `bigint` cast
//! keywords + the bar-cell expression) is passed in by each shim via
//! [`Dialect`]. The parser is written once; each DB contributes a ~3-line
//! dialect table.
//!
//! Lifted (behaviour-preserving) from the hand-written ducklink
//! `extensions/ggsql-component/src/parse.rs`. No WIT types, no `std` — so
//! it compiles natively for fuzzing and into either the
//! `duckdb:extension` or `sqlite:extension` shim unchanged.
//!
//! TRUST BOUNDARY: the host hands this fully attacker-controlled text of
//! any statement the built-in parser rejected. The contract: NEVER PANIC
//! on any input — garbage, huge, multi-byte, deeply nested, or
//! adversarial statements all come back as `Declined` / `Invalid` / a
//! (possibly nonsensical) `Rewrite` string, never an abort.

use alloc::format;
use alloc::string::String;

/// The small per-DB dialect surface the `VISUALIZE` rewrite needs. The
/// parser is shared; each shim supplies one of the constants below.
pub struct Dialect {
    /// Cast keyword for a text column (`VARCHAR` on DuckDB, `TEXT` on
    /// SQLite).
    pub varchar: &'static str,
    /// Cast keyword for a 64-bit integer (`BIGINT` on DuckDB, `INTEGER`
    /// on SQLite).
    pub bigint: &'static str,
    /// A template for the bar cell: a string of `#` repeated by the
    /// (already-cast, non-negative) value. The substring `{N}` is
    /// replaced by the count expression. DuckDB has `repeat`/`GREATEST`;
    /// SQLite synthesizes a repeat via `hex(zeroblob(n))` + `replace`.
    pub bar_template: &'static str,
}

/// DuckDB dialect (ducklink parser-dispatch shim).
pub const DUCKDB: Dialect = Dialect {
    varchar: "VARCHAR",
    bigint: "BIGINT",
    bar_template: "repeat('#', GREATEST({N}, 0))",
};

/// SQLite dialect (sqlink host-shell parse-failure intercept). `repeat`
/// is unavailable, so `hex(zeroblob(n))` yields `2n` hex chars `"00…"`
/// which `replace('00','#')` collapses to `n` `#`; `MAX(x,0)` is
/// SQLite's scalar (multi-arg) max.
pub const SQLITE: Dialect = Dialect {
    varchar: "TEXT",
    bigint: "INTEGER",
    bar_template: "replace(hex(zeroblob(MAX({N}, 0))), '00', '#')",
};

/// Outcome of offering a statement to the ggsql parser, in neutral terms.
/// Each shim maps these onto its DB's surface:
///   * ducklink -> `parser-dispatch.parse-outcome` (`declined` / `rewrite`)
///     + `duckerror::invalidargument` for `Invalid`.
///   * sqlink   -> the `__sqlink_parse(text)->text` scalar return:
///     `Declined` becomes NULL (the host treats NULL/empty as "decline"),
///     `Rewrite` becomes the SQL text, `Invalid` becomes an `Err`.
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

/// Parse/rewrite a single statement the built-in parser rejected, in the
/// given SQL `dialect`. Pure, total, and panic-free for every `&str`.
pub fn parse_visualize(query: &str, dialect: &Dialect) -> Outcome {
    let trimmed = query.trim().trim_end_matches(';').trim();

    // Case-insensitive `VISUALIZE` prefix check over the FIRST `KW.len()`
    // chars. `eq_ignore_ascii_case` is false when the byte lengths differ,
    // so a head holding any multi-byte char (byte length != 9) can never
    // match. A match therefore guarantees the head is exactly 9 ASCII
    // bytes => byte index `KW.len()` is a valid char boundary and the
    // slice below cannot panic.
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

    // Rewrite: wrap the inner select as a CTE projecting (label, n) and
    // emit a (label, n, bar) rollup. The CTE column list (`__viz(label,
    // n)`) is standard SQL accepted by BOTH DuckDB and SQLite (a
    // subquery `AS t(a,b)` alias is NOT portable, so we name the columns
    // on the CTE). The bar cell renders '#' repeated by the value using
    // the dialect's `bar_template`. This desugars entirely to standard
    // SQL — the whole point of the string->SQL rewrite form. A
    // nonsensical `inner` yields nonsensical-but-syntactically-embedded
    // SQL; the host's binder is what rejects it (cleanly), not us.
    let n_expr = format!("CAST(n AS {})", dialect.bigint);
    let bar = dialect.bar_template.replace("{N}", &n_expr);
    let rewritten = format!(
        "WITH __viz(label, n) AS ({inner}) \
         SELECT CAST(label AS {vc}) AS label, \
                {n_expr} AS n, \
                {bar} AS bar \
         FROM __viz \
         ORDER BY n DESC",
        vc = dialect.varchar,
    );
    Outcome::Rewrite(rewritten)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn declines_non_visualize() {
        assert_eq!(parse_visualize("SELECT 1", &DUCKDB), Outcome::Declined);
        assert_eq!(parse_visualize("", &SQLITE), Outcome::Declined);
        assert_eq!(parse_visualize("   ", &DUCKDB), Outcome::Declined);
        assert_eq!(parse_visualize("vis", &SQLITE), Outcome::Declined);
    }

    #[test]
    fn rewrites_visualize_case_insensitive() {
        for q in [
            "VISUALIZE SELECT a, b FROM t",
            "visualize select a,b from t;",
            "  ViSuAlIzE SELECT 1, 2 ; ",
        ] {
            match parse_visualize(q, &DUCKDB) {
                Outcome::Rewrite(sql) => assert!(sql.contains("__viz")),
                other => panic!("expected rewrite, got {other:?}"),
            }
        }
    }

    #[test]
    fn dialect_bar_differs() {
        let d = match parse_visualize("VISUALIZE SELECT a, b FROM t", &DUCKDB) {
            Outcome::Rewrite(s) => s,
            o => panic!("{o:?}"),
        };
        let s = match parse_visualize("VISUALIZE SELECT a, b FROM t", &SQLITE) {
            Outcome::Rewrite(s) => s,
            o => panic!("{o:?}"),
        };
        assert!(d.contains("repeat('#'") && d.contains("VARCHAR") && d.contains("BIGINT"));
        assert!(s.contains("zeroblob") && s.contains("TEXT") && s.contains("INTEGER"));
        assert!(!s.contains("repeat('#'"));
    }

    #[test]
    fn empty_inner_is_invalid() {
        assert!(matches!(parse_visualize("VISUALIZE", &DUCKDB), Outcome::Invalid(_)));
        assert!(matches!(parse_visualize("VISUALIZE ;", &SQLITE), Outcome::Invalid(_)));
        assert!(matches!(parse_visualize("  visualize   ", &DUCKDB), Outcome::Invalid(_)));
    }

    #[test]
    fn multibyte_prefix_does_not_panic() {
        assert_eq!(parse_visualize("visualizé SELECT 1", &DUCKDB), Outcome::Declined);
        assert_eq!(parse_visualize("v\u{0131}sualize SELECT 1", &SQLITE), Outcome::Declined);
        let _ = parse_visualize("visual\u{2603}ze SELECT 1", &DUCKDB);
        let _ = parse_visualize("\u{1F4A9}\u{1F4A9}\u{1F4A9}\u{1F4A9}\u{1F4A9}", &SQLITE);
    }
}
