//! Neutral core for the ducklink `mosaic` DuckDB wasm extension.
//!
//! # Surface
//!
//! Six scalars + one read-side macro (registered by the per-DB shim
//! against the standard `runtime.scalar-registry` + `catalog.register-macro`):
//!
//! ```text
//! mosaic_create(name TEXT, spec_json TEXT)                       -> TEXT   (app URL)
//! mosaic_create(name TEXT, spec_json TEXT, opts_json TEXT)       -> TEXT   (app URL, with opts)
//! mosaic_drop(name TEXT)                                         -> BOOLEAN
//! mosaic_url(name TEXT)                                          -> TEXT
//! mosaic_spec(name TEXT)                                         -> TEXT   (raw spec JSON)
//! mosaic_plot(sql TEXT, kind TEXT, opts_json TEXT)               -> TEXT   (URL; wraps mosaic_create)
//!
//! mosaic_list()  -- table macro; enumerates __mosaic_apps
//! ```
//!
//! # opts_json shape
//!
//! Every field is optional; passing an empty string or `NULL` inherits
//! the defaults.
//!
//! ```text
//! {
//!   "token": "auto" | "none" | "<hex-string>",  -- default "auto"
//!   "description": "..."                          -- future-facing
//! }
//! ```
//!
//! # Bridges the shim installs at load()
//!
//! Three fn pointers, mirroring the fieldbook/cache pattern: the
//! `declare!` bodies here cannot capture host state, so the shim
//! installs the pieces that need host services (nested SQL / bundle
//! bytes / random tokens) and the bodies below reach for them through
//! plain fn-ptr indirection. Wasm is single-threaded, so `AtomicPtr`
//! is enough to satisfy the mutable-static rules with no unsafe.
//!
//! - `NestedExecFn` — same shape as fieldbook-core's; runs SQL on a
//!   sibling connection so the `INSERT INTO routes` lands atomically
//!   and is visible to the httpd's next request.
//! - `BundleJsFn` / `IndexHtmlFn` — return the embedded browser
//!   artifacts (`include_bytes!`'d in the shim so this crate stays
//!   binary-free).
//! - `TokenGenFn` — 32 hex chars of entropy; the shim uses
//!   `getrandom`. Called only when opts.token == "auto".
//!
//! # URL layout (per Section 8 of the design memo)
//!
//! ```text
//! /ducklink/mosaic/app/{name}                    -- SPA index.html    (static)
//! /ducklink/mosaic/app/{name}/bundle.js          -- JS runtime        (static)
//! /ducklink/mosaic/api/app/{name}/spec           -- spec JSON         (sql)
//! /ducklink/mosaic/api/query                     -- Mosaic REST       (sql, shared)
//! ```
//!
//! The query route runs one SQL statement that dispatches on the JSON
//! envelope's `type` field (`json` / `exec`) and enforces the token
//! check by pattern-matching the raw query string bound as `$query`.

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, Ordering};

use datalink_extcore::NeutralValue;
// ArgExt supplies `arg_text` on `[NeutralValue]`; imported implicitly by
// the `declare!` macro expansion.

// ---------------------------------------------------------------------------
// Storage schema.
// ---------------------------------------------------------------------------

/// Companion table `mosaic_create` writes into. Mirrors the design-memo
/// §6 task 2 shape (`name PRIMARY KEY` + verbatim spec + token +
/// created/updated timestamps). Every column is TEXT so the whole thing
/// travels through the sql-route handler without an extra cast.
pub const CREATE_MOSAIC_APPS: &str = "\
CREATE TABLE IF NOT EXISTS __mosaic_apps (
    name        TEXT PRIMARY KEY,
    spec        TEXT NOT NULL,
    token       TEXT,
    description TEXT,
    created_at  TIMESTAMP DEFAULT current_timestamp,
    updated_at  TIMESTAMP DEFAULT current_timestamp
)";

/// The bootstrap statement (one CREATE TABLE IF NOT EXISTS). Re-running
/// it is a no-op.
pub fn bootstrap_sql() -> String {
    let mut s = String::new();
    s.push_str(CREATE_MOSAIC_APPS);
    s.push_str(";\n");
    s
}

// ---------------------------------------------------------------------------
// URL layout (kept as constants + one builder so drift can't happen).
// ---------------------------------------------------------------------------

pub const APP_PREFIX: &str = "/ducklink/mosaic/app";
pub const API_PREFIX: &str = "/ducklink/mosaic/api";

pub fn app_query_pattern(name: &str) -> String {
    format!("{API_PREFIX}/app/{name}/query")
}

pub fn app_url(name: &str) -> String {
    format!("{APP_PREFIX}/{name}")
}

pub fn app_url_with_token(name: &str, token: Option<&str>) -> String {
    match token {
        Some(tok) if !tok.is_empty() => format!("{APP_PREFIX}/{name}?token={tok}"),
        _ => app_url(name),
    }
}

pub fn app_bundle_pattern(name: &str) -> String {
    format!("{APP_PREFIX}/{name}/bundle.js")
}

pub fn app_page_pattern(name: &str) -> String {
    format!("{APP_PREFIX}/{name}")
}

pub fn app_spec_pattern(name: &str) -> String {
    format!("{API_PREFIX}/app/{name}/spec")
}

// ---------------------------------------------------------------------------
// SQL for the shared POST /api/query route (Mosaic restConnector target).
//
// The httpd's sql-kind handler binds `$body` = request body text and
// `$query` = raw query string (see crates/ducklink-host/src/httpd.rs
// `ordered_handler_params`). We implement the restConnector envelope
// entirely in SQL:
//
//   * type=json -> return list_transform + to_json (JSON array of row objects)
//   * type=exec -> nested exec-style stmt; return "{}"
//   * otherwise -> return "{}" (Mosaic ignores unknown branches)
//
// Token enforcement: caller passes ?token=<hex> in the query string; the
// server-side comparison is a plain SQL string contains. `<TOKEN>` in the
// template is substituted by `mosaic_create` (or replaced by a wildcard
// when opts.token == "none"). We use a single placeholder tag so route
// insertion doesn't have to know the SQL shape.
// ---------------------------------------------------------------------------

/// Placeholder in `QUERY_ROUTE_SQL_TEMPLATE`. Replaced at insert time
/// with either the app's token literal (`?token=<hex>`) or `''` when
/// opts.token == "none" (which the WHERE below then treats as always-true).
pub const TOKEN_PLACEHOLDER: &str = "__DUCKLINK_TOKEN__";

/// Build the per-app SQL for the `POST /ducklink/mosaic/api/app/{name}/query`
/// route. The token is embedded as a SQL literal (single-quote escaped)
/// because DuckDB's `query()` table function does not accept subqueries
/// in its argument — so we can't look the token up out of __mosaic_apps
/// at request time. Per-app routes with hard-coded tokens sidesteps
/// that limitation. Empty `token` (opts.token = "none") produces a
/// permissive `LIKE '%'` — always true.
pub fn build_app_query_route_sql(token: &str) -> String {
    let literal = quote(token);
    format!("\
SELECT * FROM query(
    CASE
        WHEN COALESCE($query, '') LIKE '%token=' || {literal} || '%'
             OR {literal} = ''
            THEN regexp_extract(COALESCE($body, ''), \
                                '\"sql\"[[:space:]]*:[[:space:]]*\"([^\"]*)\"', 1)
        ELSE 'SELECT ''unauthorized'' AS error, ''UNAUTHORIZED'' AS code'
    END
)\
")
}

/// LEGACY / kept for reference: the SQL for the once-shared query route.
/// Kept as documentation for what a route sharing the same handler across
/// every app would need to look like (blocked today by DuckDB's
/// no-subquery-in-table-function restriction). Not used by
/// `mosaic_create` since 2026-07 — per-app routes with embedded tokens
/// took over.
///
/// Parses the restConnector envelope out of `$body` (a JSON blob with
/// `{type, sql}` fields) with regexp_extract — no dependency on the
/// json extension, which isn't loaded on `ducklink serve`. The rows
/// query() returns are auto-serialized to a JSON array of row objects
/// by the httpd's `build_route_response` (see
/// crates/ducklink-host/src/httpd.rs — multi-row branch → `[{col:val,
/// ...}, ...]`, single-column-single-row branch → `body` verbatim), so
/// this handler never needs to_json / json_group_array.
///
/// Token enforcement:
///
///   * `?app=<name>` query parameter identifies which mosaic app the
///     request targets. We look up its token in `__mosaic_apps` and
///     require the request's `?token=<hex>` to match. Empty / NULL
///     tokens on the row (opts.token = "none" at create time) skip
///     the check.
///   * On auth failure the response is a single `{"error":...}` row.
///   * On success `query()` runs the extracted SQL string; the httpd
///     serializes the rows.
///
/// The route is SHARED across every mosaic app (one INSERT ON CONFLICT
/// DO NOTHING at first mosaic_create).
pub const QUERY_ROUTE_SQL_TEMPLATE: &str = "\
WITH parsed AS (
    SELECT
        regexp_extract(COALESCE($body, ''),  '\"sql\"[[:space:]]*:[[:space:]]*\"([^\"]*)\"', 1) AS sql,
        regexp_extract(COALESCE($body, ''),  '\"type\"[[:space:]]*:[[:space:]]*\"([^\"]*)\"', 1) AS type,
        regexp_extract(COALESCE($query, ''), 'app=([A-Za-z0-9_-]+)', 1)                          AS app
),
auth AS (
    SELECT
        p.sql AS sql,
        p.type AS type,
        p.app AS app,
        (t.token IS NULL OR t.token = ''
         OR COALESCE($query, '') LIKE '%token=' || t.token || '%') AS ok
    FROM parsed p
    LEFT JOIN __mosaic_apps t ON t.name = p.app
)
SELECT * FROM query(
    CASE
        WHEN (SELECT ok FROM auth) IS NOT TRUE
            THEN 'SELECT ''unauthorized'' AS error, ''UNAUTHORIZED'' AS code'
        WHEN (SELECT type FROM auth) = 'exec'
            THEN 'SELECT ''ok'' AS status'
        ELSE (SELECT sql FROM auth)
    END
)
";

// ---------------------------------------------------------------------------
// Read macro (`mosaic_list`). Enumerates every registered app + surface
// its URL so operators can `SELECT * FROM mosaic_list()`.
// ---------------------------------------------------------------------------

pub struct ReadMacro {
    pub name: &'static str,
    pub parameters: &'static [&'static str],
    pub body_sql: &'static str,
}

// Phase 1: the shim skips macro registration entirely. Both the
// canonical `TABLE ...` body shape and any body that references
// __mosaic_apps fail against the current runtime.catalog.register-macro
// path (parser wraps the body in `AS (<body>)`, which rejects `TABLE`,
// and the __mosaic_apps table only lands on the sibling connection at
// load() — the outer connection where the macro binds doesn't see it).
// Callers should `SELECT * FROM __mosaic_apps ORDER BY name` directly
// (or `POST /sql` it) until nested-exec re-entry lands on the primary
// connection at load() too.
pub const READ_MACROS: &[ReadMacro] = &[];

// ---------------------------------------------------------------------------
// Bridge: shim -> core fn pointers.
// ---------------------------------------------------------------------------

/// Nested-exec fn pointer, same shape as fieldbook-core's. Runs SQL on
/// a sibling connection to the same database (autocommitted, per the
/// WIT contract).
#[derive(Clone, Debug, Default)]
pub struct ExecResult {
    pub rows: Option<Vec<Vec<String>>>,
    pub rows_affected: Option<u64>,
}

pub type NestedExecFn = fn(&str) -> Result<ExecResult, String>;
pub type BundleJsFn = fn() -> &'static [u8];
pub type IndexHtmlFn = fn() -> &'static str;
pub type TokenGenFn = fn() -> String;

static NESTED_EXEC: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static BUNDLE_JS: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static INDEX_HTML: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static TOKEN_GEN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn install_nested_exec(f: NestedExecFn) {
    NESTED_EXEC.store(f as *mut (), Ordering::SeqCst);
}
pub fn install_bundle_js(f: BundleJsFn) {
    BUNDLE_JS.store(f as *mut (), Ordering::SeqCst);
}
pub fn install_index_html(f: IndexHtmlFn) {
    INDEX_HTML.store(f as *mut (), Ordering::SeqCst);
}
pub fn install_token_gen(f: TokenGenFn) {
    TOKEN_GEN.store(f as *mut (), Ordering::SeqCst);
}

pub fn nested_exec(sql: &str) -> Result<ExecResult, String> {
    let p = NESTED_EXEC.load(Ordering::SeqCst);
    if p.is_null() {
        return Err(String::from(
            "mosaic: nested-exec not installed (shim load() did not run)",
        ));
    }
    // SAFETY: pointer was written by `install_nested_exec` from a
    // `NestedExecFn` value; only that setter writes it.
    let f: NestedExecFn = unsafe { core::mem::transmute(p) };
    f(sql)
}

pub fn bundle_js() -> Result<&'static [u8], String> {
    let p = BUNDLE_JS.load(Ordering::SeqCst);
    if p.is_null() {
        return Err(String::from("mosaic: bundle_js not installed"));
    }
    let f: BundleJsFn = unsafe { core::mem::transmute(p) };
    Ok(f())
}

pub fn index_html() -> Result<&'static str, String> {
    let p = INDEX_HTML.load(Ordering::SeqCst);
    if p.is_null() {
        return Err(String::from("mosaic: index_html not installed"));
    }
    let f: IndexHtmlFn = unsafe { core::mem::transmute(p) };
    Ok(f())
}

pub fn gen_token() -> Result<String, String> {
    let p = TOKEN_GEN.load(Ordering::SeqCst);
    if p.is_null() {
        return Err(String::from("mosaic: token_gen not installed"));
    }
    let f: TokenGenFn = unsafe { core::mem::transmute(p) };
    Ok(f())
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// SQL-quote a text literal (single quotes doubled).
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

/// Validate an app name. Only URL-safe characters (letters, digits,
/// dash, underscore) — the pattern shows up in every URL and in
/// regexp_extract in QUERY_ROUTE_SQL_TEMPLATE.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Validate a token. Empty or 8-64 hex characters.
pub fn valid_token(tok: &str) -> bool {
    tok.is_empty()
        || (tok.len() >= 8
            && tok.len() <= 64
            && tok.chars().all(|c| c.is_ascii_hexdigit()))
}

// ---------------------------------------------------------------------------
// opts_json parsing.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Opts {
    /// Resolved token; empty string means "no token check enforced".
    pub token: String,
    pub description: Option<String>,
}

/// Parse an opts_json blob. Empty / null / whitespace-only maps to the
/// default (token = auto).
pub fn parse_opts(opts_json: &str) -> Result<Opts, String> {
    let trimmed = opts_json.trim();
    if trimmed.is_empty() {
        return Ok(Opts {
            token: gen_token()?,
            description: None,
        });
    }
    let v: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("mosaic: invalid opts_json: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| String::from("mosaic: opts_json must be an object"))?;

    let token = match obj.get("token") {
        None | Some(serde_json::Value::Null) => gen_token()?,
        Some(serde_json::Value::String(s)) => match s.as_str() {
            "auto" => gen_token()?,
            "none" => String::new(),
            other => {
                if !valid_token(other) {
                    return Err(String::from(
                        "mosaic: opts.token must be \"auto\", \"none\", or 8-64 hex chars",
                    ));
                }
                other.to_string()
            }
        },
        _ => return Err(String::from("mosaic: opts.token must be a string")),
    };
    let description = match obj.get("description") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        _ => return Err(String::from("mosaic: opts.description must be a string")),
    };
    Ok(Opts { token, description })
}

// ---------------------------------------------------------------------------
// mosaic_plot: generate a canonical vgplot spec from raw SQL + kind + opts.
//
// The spec follows the mosaic-spec YAML/JSON shape (data section maps
// name -> {query} table; plot section is an array of marks). Kept
// intentionally small — a template with column substitution, not a
// planner. Kinds supported in Phase 1: line, bar, dot, area.
// ---------------------------------------------------------------------------

pub fn build_plot_spec(sql: &str, kind: &str, opts_json: &str) -> Result<String, String> {
    let opts: serde_json::Value = if opts_json.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(opts_json)
            .map_err(|e| format!("mosaic_plot: invalid opts_json: {e}"))?
    };
    let obj = opts
        .as_object()
        .ok_or_else(|| String::from("mosaic_plot: opts_json must be an object"))?;
    let x = obj
        .get("x")
        .and_then(|v| v.as_str())
        .ok_or_else(|| String::from("mosaic_plot: opts.x is required (column name)"))?;
    let y = obj
        .get("y")
        .and_then(|v| v.as_str())
        .ok_or_else(|| String::from("mosaic_plot: opts.y is required (column name)"))?;
    let color = obj.get("color").and_then(|v| v.as_str());

    let mark_kind = match kind {
        "line" | "bar" | "dot" | "area" => kind,
        other => return Err(format!("mosaic_plot: unsupported kind {other:?} (line|bar|dot|area)")),
    };

    // Build the mark object.
    let mut mark_from = serde_json::Map::new();
    mark_from.insert("from".into(), serde_json::Value::String("source".into()));
    mark_from.insert("x".into(), serde_json::Value::String(x.into()));
    mark_from.insert("y".into(), serde_json::Value::String(y.into()));
    if let Some(c) = color {
        mark_from.insert("stroke".into(), serde_json::Value::String(c.into()));
    }

    let mut mark = serde_json::Map::new();
    mark.insert(
        format!("mark"),
        serde_json::Value::String(mark_kind.into()),
    );
    for (k, v) in mark_from {
        mark.insert(k, v);
    }

    // data.source = { query: "SELECT ..." }
    let mut data_source = serde_json::Map::new();
    data_source.insert("query".into(), serde_json::Value::String(sql.into()));
    let mut data = serde_json::Map::new();
    data.insert("source".into(), serde_json::Value::Object(data_source));

    let mut spec = serde_json::Map::new();
    spec.insert("data".into(), serde_json::Value::Object(data));
    spec.insert("width".into(), serde_json::Value::from(720));
    spec.insert("height".into(), serde_json::Value::from(320));
    spec.insert(
        "plot".into(),
        serde_json::Value::Array(alloc::vec![serde_json::Value::Object(mark)]),
    );

    serde_json::to_string(&serde_json::Value::Object(spec))
        .map_err(|e| format!("mosaic_plot: json serialize: {e}"))
}

// ---------------------------------------------------------------------------
// Route insertion helpers (called from the shim's create/drop bodies).
//
// Each returns the raw INSERT/DELETE SQL — the shim runs it through
// nested_exec so the caller sees one transaction boundary per row.
// ---------------------------------------------------------------------------

/// SQL to INSERT the per-app static index.html + bundle.js + spec route
/// triple + query route (four routes total). The caller is responsible
/// for having already deleted any prior rows with the same
/// (method, pattern).
pub fn insert_app_routes_sql(
    name: &str,
    token: &str,
    index_html_body: &str,
    bundle_js_utf8: &str,
) -> String {
    let mut sql = String::new();
    // index.html (static, text/html)
    sql.push_str(&format!(
        "INSERT INTO routes (method, pattern, handler, kind, ctype, priority) \
         VALUES ('GET', {pat}, {body}, 'static', 'text/html; charset=utf-8', 10);\n",
        pat = quote(&app_page_pattern(name)),
        body = quote(index_html_body),
    ));
    // bundle.js (static, application/javascript)
    sql.push_str(&format!(
        "INSERT INTO routes (method, pattern, handler, kind, ctype, priority) \
         VALUES ('GET', {pat}, {body}, 'static', 'application/javascript; charset=utf-8', 10);\n",
        pat = quote(&app_bundle_pattern(name)),
        body = quote(bundle_js_utf8),
    ));
    // spec JSON (sql-kind: SELECT spec FROM __mosaic_apps WHERE name = ?)
    // Single-column result → body is the spec text; ctype from row.
    let spec_sql = format!(
        "SELECT spec AS body, 'application/json' AS ctype, 200 AS status \
         FROM __mosaic_apps WHERE name = {}",
        quote(name),
    );
    sql.push_str(&format!(
        "INSERT INTO routes (method, pattern, handler, kind, ctype, priority) \
         VALUES ('GET', {pat}, {handler}, 'sql', 'application/json', 10);\n",
        pat = quote(&app_spec_pattern(name)),
        handler = quote(&spec_sql),
    ));
    // Per-app query route (sql-kind) — the token is embedded literally
    // in the SQL because query() rejects subqueries; per-app routes
    // sidestep that so an operator can host N apps with N different
    // tokens.
    sql.push_str(&format!(
        "INSERT INTO routes (method, pattern, handler, kind, ctype, priority) \
         VALUES ('POST', {pat}, {handler}, 'sql', 'application/json', 10);\n",
        pat = quote(&app_query_pattern(name)),
        handler = quote(&build_app_query_route_sql(token)),
    ));
    sql
}

/// SQL to DELETE every route row this app owns.
pub fn delete_app_routes_sql(name: &str) -> String {
    let patterns = [
        app_page_pattern(name),
        app_bundle_pattern(name),
        app_spec_pattern(name),
        app_query_pattern(name),
    ];
    let mut list = String::new();
    for (i, p) in patterns.iter().enumerate() {
        if i > 0 {
            list.push(',');
        }
        list.push_str(&quote(p));
    }
    format!("DELETE FROM routes WHERE pattern IN ({list})")
}

// ---------------------------------------------------------------------------
// Scalar body helpers (shared with the declare! closures).
// ---------------------------------------------------------------------------

fn ensure_bootstrap() -> Result<(), String> {
    // Idempotent CREATE TABLE IF NOT EXISTS; also ensures the routes
    // table exists (mosaic requires the operator to have run
    // `ducklink serve --init-routes` at least once, or to have DDL'd
    // it themselves).
    let _ = nested_exec(&bootstrap_sql())?;
    Ok(())
}

fn app_exists(name: &str) -> Result<bool, String> {
    let sql = format!(
        "SELECT count(*) FROM __mosaic_apps WHERE name = {}",
        quote(name)
    );
    let r = nested_exec(&sql)?;
    let n: i64 = r
        .rows
        .as_ref()
        .and_then(|rs| rs.first())
        .and_then(|row| row.first())
        .and_then(|c| c.parse::<i64>().ok())
        .unwrap_or(0);
    Ok(n > 0)
}

/// PHASE 1 workaround (nested-exec re-entry currently traps on the
/// standalone ducklink CLI; see extensions/mosaic-component/README.md):
/// build the SQL string a caller can `POST /sql` to install the app
/// entirely from the httpd's connection context. `mosaic_create`
/// internally does exactly the same work through nested-exec — this
/// scalar just externalises it so operators can drive the install
/// without needing nested-exec to work first.
///
/// Returns a semicolon-terminated batch of INSERT statements (idempotent
/// via `ON CONFLICT DO NOTHING` on the shared query route; the caller
/// is responsible for making sure the app name is fresh — collisions
/// on __mosaic_apps.name PK are the caller's error).
pub fn build_install_sql(
    name: &str,
    spec_json: &str,
    opts_json: &str,
) -> Result<(String /* url */, String /* sql */), String> {
    if !valid_name(name) {
        return Err(String::from(
            "mosaic_install_sql: name must be URL-safe (letters, digits, _-), 1-64 chars",
        ));
    }
    let _: serde_json::Value = serde_json::from_str(spec_json)
        .map_err(|e| format!("mosaic_install_sql: spec not valid JSON: {e}"))?;
    let opts = parse_opts(opts_json)?;

    let bundle = bundle_js()?;
    let bundle_str = core::str::from_utf8(bundle)
        .map_err(|_| String::from("mosaic: bundle.js is not valid UTF-8"))?;
    let idx = index_html()?;
    let index_html_body = idx
        .replace("__NAME__", name)
        .replace("__TOKEN__", &opts.token);

    let mut sql = String::new();
    sql.push_str(&bootstrap_sql());
    // Insert into __mosaic_apps.
    let token_sql = if opts.token.is_empty() {
        String::from("NULL")
    } else {
        quote(&opts.token)
    };
    let desc_sql = match &opts.description {
        Some(d) => quote(d),
        None => String::from("NULL"),
    };
    sql.push_str(&format!(
        "INSERT INTO __mosaic_apps (name, spec, token, description) VALUES ({n}, {s}, {t}, {d});\n",
        n = quote(name),
        s = quote(spec_json),
        t = token_sql,
        d = desc_sql,
    ));
    sql.push_str(&insert_app_routes_sql(name, &opts.token, &index_html_body, bundle_str));

    Ok((app_url_with_token(name, Some(&opts.token)), sql))
}

fn do_install_sql(name: &str, spec_json: &str, opts_json: &str) -> Result<NeutralValue, String> {
    let (_url, sql) = build_install_sql(name, spec_json, opts_json)?;
    Ok(NeutralValue::Text(sql))
}

fn do_create(name: &str, spec_json: &str, opts_json: &str) -> Result<NeutralValue, String> {
    if !valid_name(name) {
        return Err(String::from(
            "mosaic_create: name must be URL-safe (letters, digits, _-), 1-64 chars",
        ));
    }
    // Sanity-check the spec is valid JSON.
    let _: serde_json::Value =
        serde_json::from_str(spec_json).map_err(|e| format!("mosaic_create: spec not valid JSON: {e}"))?;

    let opts = parse_opts(opts_json)?;

    ensure_bootstrap()?;
    if app_exists(name)? {
        return Err(format!(
            "mosaic_create: app {name:?} already exists (drop it first via mosaic_drop)"
        ));
    }

    // 1) INSERT __mosaic_apps.
    let token_sql = if opts.token.is_empty() {
        String::from("NULL")
    } else {
        quote(&opts.token)
    };
    let desc_sql = match &opts.description {
        Some(d) => quote(d),
        None => String::from("NULL"),
    };
    let insert = format!(
        "INSERT INTO __mosaic_apps (name, spec, token, description) VALUES ({n}, {s}, {t}, {d})",
        n = quote(name),
        s = quote(spec_json),
        t = token_sql,
        d = desc_sql,
    );
    nested_exec(&insert)?;

    // 2) Per-app static + spec + query routes. Substitute __NAME__ /
    //    __TOKEN__ in the index-template.html so the boot() call in the
    //    SPA hits the right token-guarded URL.
    let bundle = bundle_js()?;
    let bundle_str = core::str::from_utf8(bundle)
        .map_err(|_| String::from("mosaic: bundle.js is not valid UTF-8"))?;
    let idx = index_html()?;
    let index_html_body = idx
        .replace("__NAME__", name)
        .replace("__TOKEN__", &opts.token);
    nested_exec(&insert_app_routes_sql(name, &opts.token, &index_html_body, bundle_str))?;

    Ok(NeutralValue::Text(app_url_with_token(name, Some(&opts.token))))
}

fn do_drop(name: &str) -> Result<NeutralValue, String> {
    if !valid_name(name) {
        return Err(String::from(
            "mosaic_drop: name must be URL-safe (letters, digits, _-), 1-64 chars",
        ));
    }
    ensure_bootstrap()?;
    // Drop routes first, then the app row.
    nested_exec(&delete_app_routes_sql(name))?;
    let r = nested_exec(&format!(
        "DELETE FROM __mosaic_apps WHERE name = {}",
        quote(name)
    ))?;
    Ok(NeutralValue::Boolean(r.rows_affected.unwrap_or(0) > 0))
}

fn do_url(name: &str) -> Result<NeutralValue, String> {
    if !valid_name(name) {
        return Err(String::from("mosaic_url: bad name"));
    }
    ensure_bootstrap()?;
    let sql = format!(
        "SELECT token FROM __mosaic_apps WHERE name = {}",
        quote(name)
    );
    let r = nested_exec(&sql)?;
    match r.rows.as_ref().and_then(|rs| rs.first()) {
        None => Ok(NeutralValue::Null),
        Some(row) => {
            let token = row.first().map(String::as_str).unwrap_or_default();
            Ok(NeutralValue::Text(app_url_with_token(
                name,
                if token.is_empty() { None } else { Some(token) },
            )))
        }
    }
}

fn do_spec(name: &str) -> Result<NeutralValue, String> {
    if !valid_name(name) {
        return Err(String::from("mosaic_spec: bad name"));
    }
    ensure_bootstrap()?;
    let sql = format!(
        "SELECT spec FROM __mosaic_apps WHERE name = {}",
        quote(name)
    );
    let r = nested_exec(&sql)?;
    match r.rows.as_ref().and_then(|rs| rs.first()) {
        None => Ok(NeutralValue::Null),
        Some(row) => Ok(NeutralValue::Text(
            row.first().cloned().unwrap_or_default(),
        )),
    }
}

fn do_plot(sql: &str, kind: &str, opts_json: &str) -> Result<NeutralValue, String> {
    // Build the spec, generate a synthetic name (mosaic_plot_<sha8>),
    // and delegate to do_create. The synthetic name lets operators
    // call mosaic_plot(...) multiple times without a collision — but
    // they can still mosaic_url(name) to fish out the URL again.
    let spec = build_plot_spec(sql, kind, opts_json)?;
    // Cheap 8-char hash of the SQL for the synthetic name (deterministic
    // -- calling twice with the same SQL/kind/opts returns the SAME url
    // rather than blowing up with "already exists"). Use the DJB2
    // hash — no crypto needed.
    let mut h: u64 = 5381;
    for b in sql.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(*b as u64);
    }
    for b in kind.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(*b as u64);
    }
    for b in opts_json.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(*b as u64);
    }
    let name = format!("plot_{h:016x}");

    // Idempotent: if the app already exists, just return its URL.
    ensure_bootstrap()?;
    if app_exists(&name)? {
        return do_url(&name);
    }
    do_create(&name, &spec, "")
}

// ---------------------------------------------------------------------------
// Capability declaration.
// ---------------------------------------------------------------------------

datalink_extcore::declare! {
    core = Core;
    extension = "mosaic";
    version = env!("CARGO_PKG_VERSION");

    scalar mosaic_create_2(text, text) -> text [propagate, nondeterministic] = |args| {
        let name = args.arg_text(0, "mosaic_create")?;
        let spec = args.arg_text(1, "mosaic_create")?;
        do_create(&name, &spec, "")
    };

    scalar mosaic_create_3(text, text, text) -> text [propagate, nondeterministic] = |args| {
        let name = args.arg_text(0, "mosaic_create")?;
        let spec = args.arg_text(1, "mosaic_create")?;
        let opts = args.arg_text(2, "mosaic_create")?;
        do_create(&name, &spec, &opts)
    };

    scalar mosaic_drop(text) -> boolean [propagate, nondeterministic] = |args| {
        let name = args.arg_text(0, "mosaic_drop")?;
        do_drop(&name)
    };

    scalar mosaic_url(text) -> text [propagate, nondeterministic] = |args| {
        let name = args.arg_text(0, "mosaic_url")?;
        do_url(&name)
    };

    scalar mosaic_spec(text) -> text [propagate, nondeterministic] = |args| {
        let name = args.arg_text(0, "mosaic_spec")?;
        do_spec(&name)
    };

    scalar mosaic_plot(text, text, text) -> text [propagate, nondeterministic] = |args| {
        let sql = args.arg_text(0, "mosaic_plot")?;
        let kind = args.arg_text(1, "mosaic_plot")?;
        let opts = args.arg_text(2, "mosaic_plot")?;
        do_plot(&sql, &kind, &opts)
    };

    // Pure spec generator; the sibling of mosaic_plot that skips the
    // create step (no nested-exec needed). Returns the canonical vgplot
    // JSON spec so callers can inspect / edit / mosaic_create it
    // separately.
    scalar mosaic_plot_spec(text, text, text) -> text [propagate, deterministic] = |args| {
        let sql = args.arg_text(0, "mosaic_plot_spec")?;
        let kind = args.arg_text(1, "mosaic_plot_spec")?;
        let opts = args.arg_text(2, "mosaic_plot_spec")?;
        build_plot_spec(&sql, &kind, &opts).map(NeutralValue::Text)
    };

    // Phase-1 workaround alongside the canonical `mosaic_create`. Returns
    // the semicolon-batched SQL an operator can POST /sql back into a
    // running `ducklink serve` to install an app without needing
    // nested-exec re-entry to work on their host. See the E2E script
    // (scripts/mosaic-phase1-e2e.sh) for the intended usage.
    scalar mosaic_install_sql(text, text, text) -> text [propagate, nondeterministic] = |args| {
        let name = args.arg_text(0, "mosaic_install_sql")?;
        let spec = args.arg_text(1, "mosaic_install_sql")?;
        let opts = args.arg_text(2, "mosaic_install_sql")?;
        do_install_sql(&name, &spec, &opts)
    };
}

// ---------------------------------------------------------------------------
// SQL-name overrides.
//
// The two `mosaic_create` overloads register under the same SQL name
// via the shim; the shim reads this table to rewrite decl.name at
// registration time. Same idea cache-core uses to make `cache(uri)` +
// `cache(config_json, uri)` share the SQL name `cache`.
// ---------------------------------------------------------------------------

pub const REGISTERED_NAMES: &[(&str, &str)] = &[
    ("mosaic_create_2", "mosaic_create"),
    ("mosaic_create_3", "mosaic_create"),
    // The rest match their declared identifier verbatim.
];

pub fn registered_name_for(decl_name: &str) -> &str {
    for (from, to) in REGISTERED_NAMES {
        if *from == decl_name {
            return to;
        }
    }
    decl_name
}
