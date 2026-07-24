//! Neutral core for the Fieldbook DuckDB extension — the wasm-side
//! counterpart to the native `duckdb-fieldbook` extension.
//!
//! A **Fieldbook** is a persisted, ordered, executable collection of SQL
//! entries kept inside the .duckdb file itself. State lives in three
//! tables under the `__fieldbook_` prefix; the CLI orchestrator drives
//! execution (`fieldbook_source` -> run at top level -> `fieldbook_record_run`).
//!
//! # Surface
//!
//! Mutate/record (declared here as scalars — the shim registers them via
//! the `runtime.scalar-registry`):
//!
//!   * `fieldbook_create(text) -> boolean`
//!   * `fieldbook_add_entry(text, text) -> integer`  (wasm side widens to Int64)
//!   * `fieldbook_drop(text) -> boolean`
//!   * `fieldbook_record_run(text, integer, bigint, bigint, text, text, bigint) -> boolean`
//!
//! Same sentinel rules as native (`fieldbook_record_run`):
//!   * `error`: empty string `""` stored as NULL
//!   * `row_count`: `-1` stored as NULL
//!
//! Read-side helpers (`fieldbook_list`, `fieldbook_entries`, `fieldbook_source`)
//! are NOT declared here — the shim registers them as SQL macros via the
//! `runtime.macro-registry` at load time, mirroring the native extension.
//!
//! # Executing SQL from a scalar body
//!
//! A `declare!` closure body is a bare `fn` pointer (no captured host
//! state), so this core reaches for host SQL through a globally installed
//! function pointer — the shim installs it at `load()` by wrapping the
//! generated `duckdb:extension/nested-exec` WIT import (see
//! `wit/duckdb-extension/nested-exec.wit`). Wasm components are
//! single-threaded, so an `AtomicPtr` load/store is sufficient (no cell
//! or mutex needed).
//!
//! The native extension's equivalent lives in a sibling `Connection`
//! guarded by a `Mutex`; the wasm side gets its sibling via the host's
//! nested-exec import, which sidesteps the core-mutex + wasm-store
//! re-entrancy that block `query`.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, Ordering};

use datalink_extcore::NeutralValue;
// `ArgExt` supplies `arg_text` / `arg_int` on `[NeutralValue]`; imported
// by the `declare!` macro expansion, so there is no explicit `use` here.


// ---------------------------------------------------------------------------
// Nested-exec plumbing
//
// The shim installs a function pointer that wraps its WIT-generated
// `duckdb::extension::nested_exec::nested_exec` call; the declared scalar
// bodies below reach for it through `nested_exec(...)`.
// ---------------------------------------------------------------------------

/// The result of a nested execution. Mirrors the WIT `exec-result` record
/// in `duckdb:extension/nested-exec` — kept as a neutral shape here so
/// this crate does not depend on any wit-bindgen output.
#[derive(Clone, Debug, Default)]
pub struct ExecResult {
    /// Rows for a row-producing statement (SELECT / RETURNING / row
    /// pragma), each cell stringified (NULL -> empty string). `None` for
    /// non-row-producing statements.
    pub rows: Option<Vec<Vec<String>>>,
    /// Affected-row count for DML (INSERT / UPDATE / DELETE). `None` for
    /// non-DML statements.
    pub rows_affected: Option<u64>,
}

/// Function-pointer type the shim installs. Runs `sql` on a sibling
/// connection to the same database (autocommitted, per the WIT contract).
pub type NestedExecFn = fn(&str) -> Result<ExecResult, String>;

// AtomicPtr for a `fn` pointer. Wasm is single-threaded; an AtomicPtr is
// used here only to satisfy the mutable-static rules with no unsafe.
static NESTED_EXEC: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the host's nested-exec function pointer. Called once from the
/// shim's `load()` before any scalar can execute.
pub fn install_nested_exec(f: NestedExecFn) {
    NESTED_EXEC.store(f as *mut (), Ordering::SeqCst);
}

/// Run `sql` on the host's sibling connection. Returns `Err` if the shim
/// has not installed a nested-exec function pointer yet.
pub fn nested_exec(sql: &str) -> Result<ExecResult, String> {
    let p = NESTED_EXEC.load(Ordering::SeqCst);
    if p.is_null() {
        return Err(String::from(
            "fieldbook: nested-exec not installed (shim load() did not run)",
        ));
    }
    // SAFETY: The pointer was written by `install_nested_exec` from a
    // `NestedExecFn` value; only that setter ever writes it, and it is a
    // plain function pointer (no captures, `'static`).
    let f: NestedExecFn = unsafe { core::mem::transmute(p) };
    f(sql)
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// SQL-quote a text literal (single quotes doubled). The native extension
/// uses the same helper; we do the same because `nested-exec` takes a raw
/// SQL string (no bound parameters).
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push('\'');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------------------
// Bootstrap SQL (identical to native `duckdb-fieldbook/src/schema.rs`).
//
// The shim runs `BOOTSTRAP_SQL` via `nested_exec` at `load()`. Every
// statement is IF NOT EXISTS / OR REPLACE, so re-loading is a no-op.
// ---------------------------------------------------------------------------

/// `CREATE TABLE IF NOT EXISTS` for `__fieldbook_books`. Mirrors
/// `duckdb-fieldbook/src/schema.rs::CREATE_BOOKS`.
pub const CREATE_BOOKS: &str = "\
CREATE TABLE IF NOT EXISTS __fieldbook_books (
    name        TEXT PRIMARY KEY,
    description TEXT,
    created_at  TIMESTAMP DEFAULT current_timestamp,
    updated_at  TIMESTAMP DEFAULT current_timestamp
)";

/// `CREATE TABLE IF NOT EXISTS` for `__fieldbook_entries`. Mirrors
/// `duckdb-fieldbook/src/schema.rs::CREATE_ENTRIES`.
pub const CREATE_ENTRIES: &str = "\
CREATE TABLE IF NOT EXISTS __fieldbook_entries (
    fieldbook   TEXT NOT NULL,
    ordinal     INTEGER NOT NULL,
    title       TEXT,
    source      TEXT NOT NULL,
    created_at  TIMESTAMP DEFAULT current_timestamp,
    updated_at  TIMESTAMP DEFAULT current_timestamp,
    PRIMARY KEY (fieldbook, ordinal)
)";

/// `CREATE TABLE IF NOT EXISTS` for `__fieldbook_runs`. Mirrors
/// `duckdb-fieldbook/src/schema.rs::CREATE_RUNS`.
pub const CREATE_RUNS: &str = "\
CREATE TABLE IF NOT EXISTS __fieldbook_runs (
    fieldbook    TEXT NOT NULL,
    ordinal      INTEGER NOT NULL,
    run_id       BIGINT NOT NULL,
    started_at   TIMESTAMP DEFAULT current_timestamp,
    duration_ms  BIGINT,
    status       TEXT,
    error        TEXT,
    row_count    BIGINT
)";

/// The three read-side macros the shim registers via the `runtime.macro-registry`.
/// Each entry is `(name, parameters, body_sql)`. Names / parameter lists /
/// bodies mirror `duckdb-fieldbook/src/lib.rs::READ_MACROS` verbatim (the
/// non-shadowing parameter-name workaround the native README documents).
pub const READ_MACROS: &[ReadMacro] = &[
    ReadMacro {
        name: "fieldbook_list",
        parameters: &[],
        body_sql: "TABLE \
            SELECT b.name, \
                   b.description, \
                   (SELECT count(*) FROM __fieldbook_entries e WHERE e.fieldbook = b.name) AS entries, \
                   b.updated_at \
            FROM __fieldbook_books b \
            ORDER BY b.name",
    },
    ReadMacro {
        name: "fieldbook_entries",
        parameters: &["fb_name"],
        body_sql: "TABLE \
            SELECT ordinal, title, source, updated_at \
            FROM __fieldbook_entries \
            WHERE fieldbook = fb_name \
            ORDER BY ordinal",
    },
    ReadMacro {
        name: "fieldbook_source",
        parameters: &["fb_name", "entry_ordinal"],
        body_sql: "(SELECT source \
                    FROM __fieldbook_entries e \
                    WHERE e.fieldbook = fb_name \
                      AND e.ordinal   = entry_ordinal)",
    },
];

/// One read-side macro declaration the shim feeds to `macro-registry.register-scalar`.
///
/// # NOTE for the shim
///
/// The `macro-registry.register-scalar` host import takes a `body_sql`
/// that is spliced verbatim after `AS`, so `TABLE ...` bodies (for
/// table-returning macros) and `(SELECT ...)` bodies (for scalar
/// macros) both work. The registry name is a legacy of when only
/// scalar macros existed; the SQL body decides the actual shape.
pub struct ReadMacro {
    /// SQL name (e.g. `"fieldbook_list"`).
    pub name: &'static str,
    /// Parameter names in the DuckDB macro signature.
    pub parameters: &'static [&'static str],
    /// The body SQL (spliced verbatim after `AS`).
    pub body_sql: &'static str,
}

/// Convenience: the three `CREATE TABLE IF NOT EXISTS` statements joined
/// with `;\n`. The shim can execute this in one `nested_exec` call.
pub fn bootstrap_sql() -> String {
    let mut out = String::new();
    out.push_str(CREATE_BOOKS);
    out.push_str(";\n");
    out.push_str(CREATE_ENTRIES);
    out.push_str(";\n");
    out.push_str(CREATE_RUNS);
    out.push_str(";\n");
    out
}

// ---------------------------------------------------------------------------
// Scalar bodies (shared helpers so the declare! closures stay short).
// ---------------------------------------------------------------------------

fn do_create(name: &str) -> Result<NeutralValue, String> {
    let sql = format!(
        "INSERT INTO __fieldbook_books (name) VALUES ({}) ON CONFLICT DO NOTHING",
        quote(name)
    );
    let r = nested_exec(&sql)?;
    // nested-exec reports rows_affected for DML; > 0 means we inserted.
    Ok(NeutralValue::Boolean(r.rows_affected.unwrap_or(0) > 0))
}

fn do_add_entry(name: &str, source: &str) -> Result<NeutralValue, String> {
    // 1) ensure the book exists.
    let exists_sql = format!(
        "SELECT count(*) FROM __fieldbook_books WHERE name = {}",
        quote(name)
    );
    let exists_res = nested_exec(&exists_sql)?;
    let exists: i64 = exists_res
        .rows
        .as_ref()
        .and_then(|rs| rs.first())
        .and_then(|r| r.first())
        .and_then(|c| c.parse::<i64>().ok())
        .unwrap_or(0);
    if exists == 0 {
        return Ok(NeutralValue::Null);
    }
    // 2) next ordinal.
    let next_sql = format!(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM __fieldbook_entries WHERE fieldbook = {}",
        quote(name)
    );
    let next_res = nested_exec(&next_sql)?;
    let next: i64 = next_res
        .rows
        .as_ref()
        .and_then(|rs| rs.first())
        .and_then(|r| r.first())
        .and_then(|c| c.parse::<i64>().ok())
        .unwrap_or(1);
    // 3) insert.
    let insert_sql = format!(
        "INSERT INTO __fieldbook_entries (fieldbook, ordinal, source) VALUES ({}, {}, {})",
        quote(name),
        next,
        quote(source)
    );
    match nested_exec(&insert_sql) {
        Ok(_) => {
            // Bump updated_at; ignore failures (parity with native's `let _`).
            let _ = nested_exec(&format!(
                "UPDATE __fieldbook_books SET updated_at = current_timestamp WHERE name = {}",
                quote(name)
            ));
            Ok(NeutralValue::Int64(next))
        }
        Err(_) => Ok(NeutralValue::Null),
    }
}

fn do_drop(name: &str) -> Result<NeutralValue, String> {
    let q = quote(name);
    // Delete runs/entries first (best-effort; mirrors native).
    let _ = nested_exec(&format!("DELETE FROM __fieldbook_runs WHERE fieldbook = {q}"));
    let _ = nested_exec(&format!(
        "DELETE FROM __fieldbook_entries WHERE fieldbook = {q}"
    ));
    let r = nested_exec(&format!("DELETE FROM __fieldbook_books WHERE name = {q}"))?;
    Ok(NeutralValue::Boolean(r.rows_affected.unwrap_or(0) > 0))
}

#[allow(clippy::too_many_arguments)]
fn do_record_run(
    name: &str,
    ordinal: i64,
    run_id: i64,
    duration_ms: i64,
    status: &str,
    error: &str,
    row_count: i64,
) -> Result<NeutralValue, String> {
    // Sentinels -> NULL in the stored row (parity with native).
    let error_sql = if error.is_empty() {
        String::from("NULL")
    } else {
        quote(error)
    };
    let rows_sql = if row_count < 0 {
        String::from("NULL")
    } else {
        row_count.to_string()
    };
    let sql = format!(
        "INSERT INTO __fieldbook_runs \
         (fieldbook, ordinal, run_id, duration_ms, status, error, row_count) \
         VALUES ({}, {}, {}, {}, {}, {}, {})",
        quote(name),
        ordinal,
        run_id,
        duration_ms,
        quote(status),
        error_sql,
        rows_sql
    );
    Ok(NeutralValue::Boolean(nested_exec(&sql).is_ok()))
}

// ---------------------------------------------------------------------------
// Capability declaration.
//
// NOTE ON WIDTHS: the native extension declares Integer (i32) for
// `fieldbook_add_entry` (return) and `fieldbook_record_run` (`ordinal`).
// The neutral type set has only `int64` — DuckDB's built-in cast handles
// the INTEGER->BIGINT widening at the SQL boundary, so callers see BIGINT
// on the wasm side. This is the only signature drift from native and is
// unavoidable without extending `NeutralType`.
// ---------------------------------------------------------------------------

datalink_extcore::declare! {
    core = Core;
    extension = "fieldbook";
    version = env!("CARGO_PKG_VERSION");

    scalar fieldbook_create(text) -> boolean [propagate, nondeterministic] = |args| {
        let name = args.arg_text(0, "fieldbook_create")?;
        do_create(&name)
    };

    scalar fieldbook_add_entry(text, text) -> int64 [propagate, nondeterministic] = |args| {
        let name = args.arg_text(0, "fieldbook_add_entry")?;
        let source = args.arg_text(1, "fieldbook_add_entry")?;
        do_add_entry(&name, &source)
    };

    scalar fieldbook_drop(text) -> boolean [propagate, nondeterministic] = |args| {
        let name = args.arg_text(0, "fieldbook_drop")?;
        do_drop(&name)
    };

    // Native declares (text, integer, bigint, bigint, text, text, bigint).
    // Wasm neutral: (text, int64, int64, int64, text, text, int64). See the
    // "NOTE ON WIDTHS" comment above.
    scalar fieldbook_record_run(text, int64, int64, int64, text, text, int64) -> boolean
        [propagate, nondeterministic] = |args| {
        let name = args.arg_text(0, "fieldbook_record_run")?;
        let ordinal = args.arg_int(1, "fieldbook_record_run")?;
        let run_id = args.arg_int(2, "fieldbook_record_run")?;
        let duration_ms = args.arg_int(3, "fieldbook_record_run")?;
        let status = args.arg_text(4, "fieldbook_record_run")?;
        let error = args.arg_text(5, "fieldbook_record_run")?;
        let row_count = args.arg_int(6, "fieldbook_record_run")?;
        do_record_run(&name, ordinal, run_id, duration_ms, &status, &error, row_count)
    };
}
