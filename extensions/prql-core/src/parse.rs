//! DB-neutral, transparent PRQL -> SQL transpile logic for `prql`.
//!
//! Wraps `prqlc` (the same compiler the existing `prql_to_sql` scalar
//! ships) with a target-[`Dialect`] param so the SAME engine emits the
//! DuckDB dialect for the ducklink parser-dispatch shim and the SQLite
//! dialect for the sqlink parse-failure intercept. This is the
//! TRANSPARENT upgrade: a bare `from x | filter ... | select ...`
//! statement the built-in SQL parser rejects is offered here, recognized
//! as PRQL, and rewritten to ordinary SQL the host re-plans — no explicit
//! `prql_to_sql(...)` call needed.
//!
//! TRUST BOUNDARY: the host hands this fully attacker-controlled text of
//! any statement the built-in parser rejected. The contract: NEVER PANIC
//! — `prqlc` is wrapped in `catch_unwind` so a compiler bug cannot unwind
//! across the WIT boundary; non-PRQL input declines, malformed PRQL is a
//! clean `Invalid`.

use alloc::string::{String, ToString};

/// Target SQL dialect for the PRQL compile. Maps to `prqlc`'s dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// DuckDB (ducklink parser-dispatch shim).
    DuckDb,
    /// SQLite (sqlink host-shell parse-failure intercept).
    Sqlite,
}

/// DuckDB dialect alias (named to mirror ggsql/dplyr-core's `DUCKDB`).
pub const DUCKDB: Dialect = Dialect::DuckDb;
/// SQLite dialect alias (mirrors ggsql/dplyr-core's `SQLITE`).
pub const SQLITE: Dialect = Dialect::Sqlite;

/// Outcome of offering a statement to the PRQL transpiler, in neutral
/// terms. Each shim maps these onto its DB's surface exactly like
/// ggsql/dplyr-core.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not a PRQL statement; the host proceeds to its own parse error.
    Declined,
    /// Looked like PRQL but failed to compile; a clean parse error.
    Invalid(String),
    /// Claimed and compiled to this ordinary SQL.
    Rewrite(String),
}

/// First-token signals that a statement is PRQL (a pipeline source or a
/// top-level declaration). A bare `from`/`let`/`func`/`prql` start is what
/// distinguishes PRQL from the SQL the built-in parser already rejected.
const PRQL_STARTS: &[&str] = &["from", "let", "func", "prql"];

fn prqlc_dialect(d: Dialect) -> prqlc::sql::Dialect {
    match d {
        Dialect::DuckDb => prqlc::sql::Dialect::DuckDb,
        Dialect::Sqlite => prqlc::sql::Dialect::SQLite,
    }
}

/// Compile a PRQL string to SQL for `dialect`. `None`-free: returns the
/// SQL or a flattened error string. Defends against a `prqlc` panic.
pub fn compile(src: &str, dialect: Dialect) -> Result<String, String> {
    let opts = prqlc::Options::default()
        .no_signature()
        .with_target(prqlc::Target::Sql(Some(prqlc_dialect(dialect))));
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prqlc::compile(src, &opts)));
    match res {
        Ok(Ok(sql)) => Ok(sql),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(String::from("prql: internal compiler error")),
    }
}

/// Parse/transpile a single statement the built-in parser rejected, in the
/// given target `dialect`. Pure, total, panic-free for every `&str`.
pub fn parse_prql(query: &str, dialect: Dialect) -> Outcome {
    let trimmed = query.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Outcome::Declined;
    }

    // First identifier token, lower-cased. If it is not a PRQL pipeline /
    // declaration start, decline so we never claim plain SQL the engine
    // could still handle.
    let first: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_lowercase();
    if !PRQL_STARTS.contains(&first.as_str()) {
        return Outcome::Declined;
    }

    match compile(trimmed, dialect) {
        Ok(sql) => {
            let sql = sql.trim().to_string();
            if sql.is_empty() {
                Outcome::Declined
            } else {
                Outcome::Rewrite(sql)
            }
        }
        // It looked like PRQL (right keyword) but did not compile -> a
        // clean parse error rather than a silent decline.
        Err(e) => Outcome::Invalid(e),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    fn rw(q: &str, d: Dialect) -> String {
        match parse_prql(q, d) {
            Outcome::Rewrite(s) => s,
            o => panic!("expected rewrite, got {o:?}"),
        }
    }

    #[test]
    fn declines_non_prql() {
        assert_eq!(parse_prql("SELECT 1", DUCKDB), Outcome::Declined);
        assert_eq!(parse_prql("", SQLITE), Outcome::Declined);
        assert_eq!(parse_prql("update t set x = 1", DUCKDB), Outcome::Declined);
    }

    #[test]
    fn transparent_pipeline_compiles() {
        let sql = rw("from invoices | filter total > 100 | select {id, total}", DUCKDB);
        let up = sql.to_uppercase();
        assert!(up.contains("SELECT"), "{sql}");
        assert!(up.contains("FROM"), "{sql}");
        assert!(up.contains("INVOICES"), "{sql}");
        assert!(up.contains("WHERE"), "{sql}");
    }

    #[test]
    fn dialect_is_threaded() {
        // Both dialects compile; the proof that the dialect param reaches
        // prqlc is that each is accepted by its own engine downstream
        // (exercised in the e2e). Here we just assert both produce SQL.
        let d = rw("from t | take 5", DUCKDB).to_uppercase();
        let s = rw("from t | take 5", SQLITE).to_uppercase();
        assert!(d.contains("FROM") && d.contains("LIMIT"), "{d}");
        assert!(s.contains("FROM") && s.contains("LIMIT"), "{s}");
    }

    #[test]
    fn malformed_prql_is_invalid() {
        // Starts with `from` (claimed) but is not valid PRQL.
        assert!(matches!(
            parse_prql("from", DUCKDB),
            Outcome::Invalid(_) | Outcome::Declined
        ));
        assert!(matches!(
            parse_prql("from t | filter", DUCKDB),
            Outcome::Invalid(_)
        ));
    }

    #[test]
    fn adversarial_inputs_never_panic() {
        let _ = parse_prql("from \u{1F4A9} | filter \u{2603}", SQLITE);
        let _ = parse_prql("from(((((", DUCKDB);
        let _ = parse_prql(&"from x | ".repeat(1000), DUCKDB);
    }
}
