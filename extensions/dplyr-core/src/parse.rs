//! DB-neutral `dplyr( <pipeline> )` parse/transpile logic for `dplyr`.
//!
//! Recognizing a `dplyr( tbl |> verb(..) |> .. )` statement, splitting the
//! `|>` pipeline, and translating the six core verbs
//! (`filter` / `select` / `mutate` / `arrange` / `summarise` / `group_by`)
//! to ordinary SQL is fully DB-agnostic and shared. The ONLY engine
//! difference in this subset is how a boolean literal is spelled (DuckDB
//! `TRUE`/`FALSE`; SQLite has no boolean and uses `1`/`0`), so that tiny
//! surface is passed in by each shim via [`Dialect`] — the transpiler is
//! written once; each DB contributes a ~3-line dialect table. Mirrors the
//! ggsql-core split exactly.
//!
//! TRUST BOUNDARY: the host hands this fully attacker-controlled text of
//! any statement the built-in parser rejected. The contract: NEVER PANIC
//! on any input — garbage, huge, multi-byte, unbalanced parens, or
//! adversarial statements all come back as `Declined` / `Invalid` / a
//! (possibly nonsensical) `Rewrite` string, never an abort.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The small per-DB dialect surface the dplyr transpiler needs. The
/// transpiler is shared; each shim supplies one of the constants below.
pub struct Dialect {
    /// Engine name (diagnostics only).
    pub name: &'static str,
    /// How a `TRUE` literal is spelled (`TRUE` on DuckDB, `1` on SQLite).
    pub true_lit: &'static str,
    /// How a `FALSE` literal is spelled (`FALSE` on DuckDB, `0` on SQLite).
    pub false_lit: &'static str,
}

/// DuckDB dialect (ducklink parser-dispatch shim).
pub const DUCKDB: Dialect = Dialect {
    name: "duckdb",
    true_lit: "TRUE",
    false_lit: "FALSE",
};

/// SQLite dialect (sqlink host-shell parse-failure intercept). SQLite has
/// no native boolean, so `TRUE`/`FALSE` become `1`/`0`.
pub const SQLITE: Dialect = Dialect {
    name: "sqlite",
    true_lit: "1",
    false_lit: "0",
};

/// Outcome of offering a statement to the dplyr transpiler, in neutral
/// terms. Each shim maps these onto its DB's surface:
///   * ducklink -> `parser-dispatch.parse-outcome` (`declined` / `rewrite`)
///     + `duckerror::invalidargument` for `Invalid`.
///   * sqlink   -> the `__sqlink_parse(text)->text` scalar return:
///     `Declined` becomes NULL, `Rewrite` becomes the SQL text, `Invalid`
///     becomes an `Err`.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not a `dplyr(...)` statement; the host proceeds.
    Declined,
    /// A malformed `dplyr(...)`; surfaced as a clean parse error.
    Invalid(String),
    /// The statement is claimed and transpiled to this ordinary SQL.
    Rewrite(String),
}

/// Keyword we intercept, lower-case.
const KW: &str = "dplyr";

/// Parse/transpile a single `dplyr( <pipeline> )` statement the built-in
/// parser rejected, in the given SQL `dialect`. Pure, total, panic-free.
pub fn parse_dplyr(query: &str, dialect: &Dialect) -> Outcome {
    let trimmed = query.trim().trim_end_matches(';').trim();

    // Locate the opening paren of `dplyr(`. The head before it must be the
    // bare keyword (case-insensitive). `eq_ignore_ascii_case` is false on a
    // byte-length mismatch, so a multi-byte head can never match.
    let open = match trimmed.find('(') {
        Some(i) => i,
        None => return Outcome::Declined,
    };
    let head = trimmed[..open].trim();
    if !head.eq_ignore_ascii_case(KW) {
        return Outcome::Declined;
    }
    if !trimmed.ends_with(')') {
        return Outcome::Invalid(String::from(
            "dplyr(...) must be a single closed call, e.g. dplyr( tbl |> filter(x == 1) )",
        ));
    }
    // Inner pipeline = between the first '(' and the final ')'.
    let inner = trimmed[open + 1..trimmed.len() - 1].trim();
    if inner.is_empty() {
        return Outcome::Invalid(String::from(
            "dplyr() requires a pipeline, e.g. dplyr( tbl |> filter(x == 1) |> select(a, b) )",
        ));
    }

    let segments = split_top(inner, "|>");
    let mut it = segments.into_iter();
    let table = match it.next() {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            return Outcome::Invalid(String::from(
                "dplyr pipeline must start with a table, e.g. dplyr( tbl |> ... )",
            ))
        }
    };

    let mut wheres: Vec<String> = Vec::new();
    let mut havings: Vec<String> = Vec::new();
    let mut select: Option<Vec<String>> = None;
    let mut mutates: Vec<(String, String)> = Vec::new();
    let mut group_by: Vec<String> = Vec::new();
    let mut summarise: Vec<(String, String)> = Vec::new();
    let mut arrange: Vec<String> = Vec::new();
    let mut grouped = false;

    for seg in it {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let (verb, args) = match split_call(seg) {
            Some(v) => v,
            None => {
                return Outcome::Invalid(format!(
                    "dplyr: expected a verb call like filter(...), got `{seg}`"
                ))
            }
        };
        match verb.to_ascii_lowercase().as_str() {
            "filter" => {
                let cond = translate_expr(args, dialect);
                if cond.trim().is_empty() {
                    return Outcome::Invalid(String::from("dplyr: filter() needs a condition"));
                }
                if grouped {
                    havings.push(format!("({cond})"));
                } else {
                    wheres.push(format!("({cond})"));
                }
            }
            "select" => {
                let cols: Vec<String> = split_top(args, ",")
                    .into_iter()
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty())
                    .collect();
                if cols.is_empty() {
                    return Outcome::Invalid(String::from("dplyr: select() needs column(s)"));
                }
                select = Some(cols);
            }
            "mutate" => {
                for a in split_top(args, ",") {
                    match split_assign(&a) {
                        Some((name, rhs)) => {
                            mutates.push((name, translate_expr(&rhs, dialect)));
                        }
                        None => {
                            return Outcome::Invalid(format!(
                                "dplyr: mutate() needs name = expr, got `{}`",
                                a.trim()
                            ))
                        }
                    }
                }
            }
            "summarise" | "summarize" => {
                for a in split_top(args, ",") {
                    match split_assign(&a) {
                        Some((name, rhs)) => {
                            summarise.push((name, translate_expr(&rhs, dialect)));
                        }
                        None => {
                            return Outcome::Invalid(format!(
                                "dplyr: summarise() needs name = expr, got `{}`",
                                a.trim()
                            ))
                        }
                    }
                }
            }
            "group_by" => {
                for c in split_top(args, ",") {
                    let c = c.trim();
                    if !c.is_empty() {
                        group_by.push(c.to_string());
                    }
                }
                grouped = true;
            }
            "arrange" => {
                for a in split_top(args, ",") {
                    let a = a.trim();
                    if a.is_empty() {
                        continue;
                    }
                    // desc(col) -> col DESC ; asc(col) -> col ASC ; col -> col
                    if let Some(inner) = strip_call(a, "desc") {
                        arrange.push(format!("{} DESC", inner.trim()));
                    } else if let Some(inner) = strip_call(a, "asc") {
                        arrange.push(format!("{} ASC", inner.trim()));
                    } else {
                        arrange.push(a.to_string());
                    }
                }
            }
            other => {
                return Outcome::Invalid(format!(
                    "dplyr: unsupported verb `{other}` (supported: filter, select, mutate, \
                     arrange, summarise, group_by)"
                ))
            }
        }
    }

    // Build the SELECT list. With summarise, the projection is the group
    // keys plus the aggregate expressions; otherwise the explicit select
    // (or `*`) plus any mutate-added columns.
    let select_list = if !summarise.is_empty() {
        let mut cols = group_by.clone();
        for (name, expr) in &summarise {
            cols.push(format!("{expr} AS {name}"));
        }
        cols.join(", ")
    } else {
        let mut cols = match &select {
            Some(s) => s.clone(),
            None => alloc::vec![String::from("*")],
        };
        for (name, expr) in &mutates {
            cols.push(format!("{expr} AS {name}"));
        }
        cols.join(", ")
    };

    let mut sql = format!("SELECT {select_list} FROM {table}");
    if !wheres.is_empty() {
        sql.push_str(&format!(" WHERE {}", wheres.join(" AND ")));
    }
    if !group_by.is_empty() {
        sql.push_str(&format!(" GROUP BY {}", group_by.join(", ")));
    }
    if !havings.is_empty() {
        sql.push_str(&format!(" HAVING {}", havings.join(" AND ")));
    }
    if !arrange.is_empty() {
        sql.push_str(&format!(" ORDER BY {}", arrange.join(", ")));
    }
    Outcome::Rewrite(sql)
}

/// Split `s` on the top-level (paren-depth-0) occurrences of `sep`.
/// `sep` is a 1- or 2-byte ASCII separator (`"|>"` or `","`).
fn split_top(s: &str, sep: &str) -> Vec<String> {
    let bytes = s.as_bytes();
    let sb = sep.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ => {}
        }
        if depth == 0
            && i + sb.len() <= bytes.len()
            && &bytes[i..i + sb.len()] == sb
        {
            out.push(s[start..i].to_string());
            i += sb.len();
            start = i;
            continue;
        }
        i += 1;
    }
    out.push(s[start..].to_string());
    out
}

/// Split a verb call `verb(args)` into `(verb, args)`; `None` if it is not
/// shaped like a call.
fn split_call(seg: &str) -> Option<(&str, &str)> {
    let open = seg.find('(')?;
    if !seg.trim_end().ends_with(')') {
        return None;
    }
    let verb = seg[..open].trim();
    if verb.is_empty() {
        return None;
    }
    let close = seg.rfind(')')?;
    if close <= open {
        return None;
    }
    Some((verb, seg[open + 1..close].trim()))
}

/// If `s` is exactly `name(inner)` (case-insensitive name), return `inner`.
fn strip_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = s.find('(')?;
    if !s.trim_end().ends_with(')') {
        return None;
    }
    if !s[..open].trim().eq_ignore_ascii_case(name) {
        return None;
    }
    let close = s.rfind(')')?;
    if close <= open {
        return None;
    }
    Some(&s[open + 1..close])
}

/// Split `name = expr` on the first STANDALONE `=` (not `==`/`>=`/`<=`/`!=`).
fn split_assign(s: &str) -> Option<(String, String)> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            let prev = if i > 0 { bytes[i - 1] } else { 0 };
            let next = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
            let is_cmp = next == b'='
                || prev == b'='
                || prev == b'<'
                || prev == b'>'
                || prev == b'!';
            if !is_cmp {
                let name = s[..i].trim().to_string();
                let rhs = s[i + 1..].trim().to_string();
                if name.is_empty() || rhs.is_empty() {
                    return None;
                }
                return Some((name, rhs));
            }
        }
        i += 1;
    }
    None
}

/// Translate a dplyr/R expression fragment to SQL: `==`->`=`, `&&`/`&`->AND,
/// `||`/`|`->OR, `mean(`->`avg(`, `n()`->`count(*)`, and the boolean
/// literals via the dialect.
fn translate_expr(s: &str, dialect: &Dialect) -> String {
    let mut e = s.trim().to_string();
    e = e.replace("==", "=");
    e = e.replace("&&", " AND ");
    e = e.replace("||", " OR ");
    e = e.replace('&', " AND ");
    e = e.replace('|', " OR ");
    e = e.replace("mean(", "avg(");
    e = e.replace("n()", "count(*)");
    e = replace_word(&e, "TRUE", dialect.true_lit);
    e = replace_word(&e, "FALSE", dialect.false_lit);
    e = replace_word(&e, "true", dialect.true_lit);
    e = replace_word(&e, "false", dialect.false_lit);
    // Collapse the runs of spaces the AND/OR substitutions can introduce.
    collapse_ws(&e)
}

/// Replace whole-word occurrences of `word` with `to` (ASCII identifier
/// boundaries). Avoids clobbering `word` embedded inside a longer name.
fn replace_word(s: &str, word: &str, to: &str) -> String {
    let bytes = s.as_bytes();
    let wb = word.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let at_word = i + wb.len() <= bytes.len() && &bytes[i..i + wb.len()] == wb;
        let left_ok = i == 0 || !is_ident(bytes[i - 1]);
        let right_ok = i + wb.len() >= bytes.len() || !is_ident(bytes[i + wb.len()]);
        if at_word && left_ok && right_ok {
            out.push_str(to);
            i += wb.len();
        } else {
            // Push one char (respecting UTF-8 boundaries).
            let ch_len = utf8_len(bytes[i]);
            let end = core::cmp::min(i + ch_len, bytes.len());
            out.push_str(&s[i..end]);
            i = end;
        }
    }
    out
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.trim().chars() {
        if ch == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    fn rw(q: &str, d: &Dialect) -> String {
        match parse_dplyr(q, d) {
            Outcome::Rewrite(s) => s,
            o => panic!("expected rewrite, got {o:?}"),
        }
    }

    #[test]
    fn declines_non_dplyr() {
        assert_eq!(parse_dplyr("SELECT 1", &DUCKDB), Outcome::Declined);
        assert_eq!(parse_dplyr("", &SQLITE), Outcome::Declined);
        assert_eq!(parse_dplyr("ggplot(x)", &DUCKDB), Outcome::Declined);
    }

    #[test]
    fn basic_filter_select_arrange() {
        let sql = rw(
            "dplyr( mtcars |> filter(cyl == 6) |> select(mpg, hp) |> arrange(desc(hp)) )",
            &DUCKDB,
        );
        assert_eq!(
            sql,
            "SELECT mpg, hp FROM mtcars WHERE (cyl = 6) ORDER BY hp DESC"
        );
    }

    #[test]
    fn group_by_summarise() {
        let sql = rw(
            "dplyr( mtcars |> group_by(cyl) |> summarise(avg_mpg = mean(mpg), n = n()) |> arrange(desc(avg_mpg)) )",
            &DUCKDB,
        );
        assert_eq!(
            sql,
            "SELECT cyl, avg(mpg) AS avg_mpg, count(*) AS n FROM mtcars GROUP BY cyl ORDER BY avg_mpg DESC"
        );
    }

    #[test]
    fn filter_after_group_by_is_having() {
        let sql = rw(
            "dplyr( mtcars |> group_by(cyl) |> summarise(n = n()) |> filter(n > 5) )",
            &DUCKDB,
        );
        assert_eq!(
            sql,
            "SELECT cyl, count(*) AS n FROM mtcars GROUP BY cyl HAVING (n > 5)"
        );
    }

    #[test]
    fn mutate_adds_columns() {
        let sql = rw("dplyr( t |> mutate(wt_kg = wt * 1000) )", &DUCKDB);
        assert_eq!(sql, "SELECT *, wt * 1000 AS wt_kg FROM t");
    }

    #[test]
    fn boolean_dialect_differs() {
        let d = rw("dplyr( t |> filter(flag == TRUE) )", &DUCKDB);
        let s = rw("dplyr( t |> filter(flag == TRUE) )", &SQLITE);
        assert!(d.contains("flag = TRUE"), "{d}");
        assert!(s.contains("flag = 1"), "{s}");
    }

    #[test]
    fn and_or_translation() {
        let sql = rw("dplyr( t |> filter(a == 1 & b == 2 | c == 3) )", &DUCKDB);
        assert_eq!(sql, "SELECT * FROM t WHERE (a = 1 AND b = 2 OR c = 3)");
    }

    #[test]
    fn malformed_is_invalid_not_panic() {
        assert!(matches!(parse_dplyr("dplyr(", &DUCKDB), Outcome::Invalid(_)));
        assert!(matches!(parse_dplyr("dplyr()", &DUCKDB), Outcome::Invalid(_)));
        assert!(matches!(
            parse_dplyr("dplyr( t |> bogus(x) )", &DUCKDB),
            Outcome::Invalid(_)
        ));
    }

    #[test]
    fn adversarial_inputs_never_panic() {
        let _ = parse_dplyr("dplyr( ((((((", &DUCKDB);
        let _ = parse_dplyr("dplyr( \u{1F4A9} |> filter(\u{2603}) )", &SQLITE);
        let _ = parse_dplyr("dplyré( t )", &DUCKDB);
        let _ = parse_dplyr("dplyr )))) (((( ", &DUCKDB);
    }
}
