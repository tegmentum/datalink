//! Dynlink-mode sqlite bridge emitter (Phase A, §A.4 Option 1).
//!
//! Emits a bridge crate that dispatches every SQL scalar through
//! `compose:dynlink/linker` — CBOR envelope in / CBOR envelope out
//! against a resident provider identified by `opts.provider_id` —
//! instead of the wac-plug-linked WIT interfaces the sibling
//! `datalink-shim-sqlite-emit` produces.
//!
//! The bridge maps onto the declarative `sqlite:extension@1.0.0`
//! contract (fresh recon, `/Users/zacharywhitley/git/sqlink/sqlite-wit/
//! wit/sqlite-extension/*.wit`):
//!
//!   * `metadata.describe() -> manifest` — the guest advertises
//!     every scalar it wants registered; the host installs the
//!     sqlite3 trampolines against its own connection.
//!   * `scalar-function.call(func-id, args) -> result<sql-value,
//!     string>` — per-row dispatch keyed by the manifest-assigned
//!     `func-id`.
//!
//! There is **no** imperative `register-*` call on the extension
//! side (the pre-1.0.0 contract had an `extension` interface with
//! `register-scalar-function`; that has been retired). This crate's
//! previous emit forked against the stale contract; the rewrite
//! matches the shape shipping in `postgis-sqlink-bridge`.
//!
//! Wire discipline mirrors
//! `postgis-wasm/crates/provider/src/envelope.rs`:
//!
//! ```ignore
//! Request  { v: 1, args: Vec<CborValue> }
//! Response { ok:  Option<CborValue>, err: Option<String> }
//! ```
//!
//! Aggregate + vtab exports are wired in Phase 9.3.next: the world
//! adds `sqlite:extension/aggregate-function@1.0.0` (advertising
//! every aggregate + window function the catalog surfaces, with
//! `is_window=true` for the latter) and `sqlite:extension/vtab@1.0.0`
//! (advertising every table-function the catalog surfaces as an
//! eponymous vtab). Both dispatch through the same
//! `linker.resolve-by-id + invoke` CBOR envelope the scalar path
//! uses:
//!
//!   * Aggregate `step`/`inverse` accumulate the per-row CBOR-encoded
//!     args into a per-context Vec; `finalize`/`value` ship the
//!     accumulated Vec to the provider as a single CBOR List arg on
//!     the `<name>` method (finalize) or `<name>-value` (value).
//!   * Vtab (table-function) `connect` returns a single-column BLOB
//!     schema; `filter` invokes `<name>` and buffers the returned
//!     resultset; `next`/`eof`/`column`/`rowid` stream from the
//!     buffered rows. Table-functions with non-trivial cursor
//!     semantics remain a follow-up — the guest surface here is a
//!     structural stub that error-returns anywhere the CBOR wire
//!     alone can't reconstruct the semantics.
//!
//! Collation / hook exports remain OMITTED at this phase.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::sql_extension_catalog::{Catalog, FnKind, LeavesOverlay};
use crate::DynlinkOptions;

/// Per-table-function signature loaded from `table_functions` in the
/// shim-interface DB. Mirrors the sibling
/// `datalink_shim_duckdb_dynlink_emit::emit_dynlink::TableSig` shape.
///
/// `output_columns` carries the row schema `[(name, type_token), ...]`
/// when the shim's `output_schema` arm advertised one at extract time
/// — the emit renders it into a per-vtab `CREATE TABLE x(...)` schema
/// at register time. If `None`, the emit falls back to the pre-schema-
/// lift shape: `CREATE TABLE x("result" BLOB, "_arg0..3" HIDDEN)`.
///
/// `param_tokens` gives the argv arity — one HIDDEN slot per positional
/// arg is emitted alongside the output columns. Type tokens are drawn
/// from the closed set the shim-interface ingester produces
/// (`binary` | `boolean` | `float64` | `int64` | `text`).
#[derive(Debug, Clone)]
pub(crate) struct TableSig {
    pub param_tokens: Vec<String>,
    pub output_columns: Option<Vec<(String, String)>>,
}

/// Load `SELECT name, param_types_json, output_columns_json FROM
/// table_functions WHERE extension = ?` into a name→signature map.
///
/// `param_types_json` is a JSON `array-of-arrays`; the current
/// extension catalogs carry a single overload group per name so we
/// take element 0. `output_columns_json` is a JSON array of `[name,
/// type_token]` pairs, absent (or an empty `[]`) for legacy rows
/// that predate the B5 schema lift.
pub(crate) fn load_table_sigs(
    sqlite: &Path,
    extension: &str,
) -> Result<HashMap<String, TableSig>> {
    let conn = rusqlite::Connection::open_with_flags(
        sqlite,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening shim-interface sqlite: {}", sqlite.display()))?;
    let mut stmt = conn.prepare(
        "SELECT name, param_types_json, output_columns_json \
         FROM table_functions WHERE extension = ?1",
    )?;
    let rows = stmt.query_map([extension], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut out: HashMap<String, TableSig> = HashMap::new();
    let mut multi_overload_count = 0usize;
    for row in rows {
        let (name, param_json, output_columns_json) = row?;
        let outer: Vec<Vec<String>> = serde_json::from_str(&param_json)
            .with_context(|| format!("parsing param_types_json for table_function {name}"))?;
        if outer.len() > 1 {
            multi_overload_count += 1;
        }
        let param_tokens = outer.into_iter().next().unwrap_or_default();
        let output_columns = match output_columns_json.as_deref() {
            Some(s) if !s.is_empty() && s != "[]" => {
                let cols: Vec<[String; 2]> = serde_json::from_str(s).with_context(|| {
                    format!("parsing output_columns_json for table_function {name}")
                })?;
                Some(
                    cols.into_iter()
                        .map(|[n, t]| (n, t))
                        .collect::<Vec<(String, String)>>(),
                )
            }
            _ => None,
        };
        out.insert(
            name,
            TableSig {
                param_tokens,
                output_columns,
            },
        );
    }
    if multi_overload_count > 0 {
        eprintln!(
            "[sqlite-dynlink-emit] warning: {multi_overload_count} table_function(s) with >1 overload \
             group; using the first (Phase A behaviour)."
        );
    }
    Ok(out)
}

/// Map a shim-interface type token to a SQLite column-type keyword
/// suitable for the `CREATE TABLE x(...)` schema the vtab advertises.
///
/// SQLite uses dynamic typing with column-declared "type affinity"
/// rather than strict types — declaring a column `INTEGER` doesn't
/// prevent a row from storing a BLOB in it. The mapping below picks
/// the affinity that best matches the token so the query planner
/// gets sensible default-collation + sorting behaviour:
///
///   * `binary`  → BLOB (BLOB affinity, no coercion)
///   * `boolean` → INTEGER (SQLite has no BOOL type; 0/1 in an INT)
///   * `int64`   → INTEGER
///   * `float64` → REAL
///   * `text`    → TEXT
///
/// Unknown tokens fall back to BLOB, matching the pre-schema-lift
/// bridge's opaque wire shape.
fn token_to_sqlite_type(token: &str) -> &'static str {
    match token {
        "binary" => "BLOB",
        "boolean" => "INTEGER",
        "int64" => "INTEGER",
        "float64" => "REAL",
        "text" => "TEXT",
        _ => "BLOB",
    }
}

/// Emit a Dynlink-mode sqlite bridge crate under `out_dir`.
///
/// Produced layout:
///
/// ```text
/// Cargo.toml
/// README.md
/// src/lib.rs
/// wit/world.wit
/// wit/deps/compose-dynlink/     (copied from datalink-dynlink)
/// wit/deps/sys-compose/         (copied from datalink-dynlink)
/// wit/deps/sqlite-extension/    (copied from ~/git/sqlink/sqlite-wit/wit/…)
/// ```
pub fn emit_dynlink(
    catalog: &Catalog,
    _leaves_overlay: Option<&LeavesOverlay>,
    out_dir: &Path,
    opts: &DynlinkOptions,
) -> Result<()> {
    fs::create_dir_all(out_dir.join("src"))?;
    fs::create_dir_all(out_dir.join("wit/deps"))?;

    let leaves = catalog
        .resolve(&opts.target)
        .with_context(|| format!("resolving target '{}'", opts.target))?;
    let functions = catalog.functions_for(&leaves);

    let crate_name = crate_name_for(opts);
    let version = if catalog.meta.version.is_empty() {
        "0.1.0".to_string()
    } else {
        catalog.meta.version.clone()
    };

    // Phase 9.3.next: partition the catalog's function surface into
    // the four kinds the dispatch tables advertise separately.
    // Scalars route through `scalar-function`; aggregates + windows
    // through `aggregate-function` (with `is_window=true` marking
    // the latter); table-functions through `vtab` as eponymous
    // vtabs. The catalog's `FnKind::Window` is folded into the
    // aggregate-function-spec list at emit time.
    let has_tables = functions
        .iter()
        .any(|(k, _)| *k == FnKind::Table);
    let has_aggs = functions
        .iter()
        .any(|(k, _)| *k == FnKind::Aggregate || *k == FnKind::Window);

    fs::write(out_dir.join("Cargo.toml"), cargo_toml(&crate_name, &version))?;
    fs::write(
        out_dir.join("wit/world.wit"),
        world_wit(&opts.sub_ext, has_aggs, has_tables),
    )?;
    populate_deps(&out_dir.join("wit/deps"))?;

    // Build the scalar alias→canonical map from `catalog.aliases`.
    // The catalog carries name-mangling aliases (e.g.
    // `st_geomfromtext` → `st_geom_from_text`) as first-class
    // `[[aliases]]` entries so both SQL spellings resolve to the
    // same provider WIT method. The bridge exposes both forms via
    // `metadata.describe()` (they're both in `leaf.scalars`), but
    // the provider only matches the WIT-canonical (long) form.
    // Without translation, a call to `st_geomfromtext(...)` reaches
    // the provider as method `st-geomfromtext` and fails with
    // `unknown method`.
    // Collect scalar + table-function aliases together — both dispatch
    // paths (`dispatch_call_scalar` for scalars, `dispatch_call_table`
    // for UDTFs via the vtab arm) run the wire method through
    // `canonical_for(name).replace('_', '-')`, so the compact-form
    // spelling of a UDTF (e.g. `st_dumppoints`) needs the same
    // canonical→WIT-name propagation the scalar path relies on.
    let scalar_aliases: Vec<(String, String)> = catalog
        .aliases
        .iter()
        .filter(|a| a.kind == "scalar" || a.kind == "table_function")
        .map(|a| (a.alias.clone(), a.canonical.clone()))
        .collect();

    // Load per-vtab signatures from the sibling shim-interface DB, if
    // provided. Every table-function name the catalog surfaces SHOULD
    // have a matching row so the vtab.connect schema arm advertises
    // the real output columns + arg arity; when absent (either the DB
    // wasn't passed, or a specific fn hasn't been extracted with a B5
    // `output_columns_json`), that fn falls back to the pre-schema-
    // lift opaque `result BLOB` + 4 hidden slots shape.
    let table_sig_map = match opts.interface_sqlite.as_deref() {
        Some(p) => load_table_sigs(p, &catalog.meta.extension).with_context(|| {
            format!(
                "loading table_function signatures from shim-interface: {}",
                p.display()
            )
        })?,
        None => {
            if has_tables {
                eprintln!(
                    "[sqlite-dynlink-emit] warning: no --interface sqlite provided; \
                     emitting opaque single-BLOB vtab schema for every table_function \
                     (pre-B5 fallback shape)."
                );
            }
            HashMap::new()
        }
    };

    let lib_src = lib_rs(
        &opts.provider_id,
        &opts.extension_root,
        &catalog.meta.extension,
        &version,
        functions.iter().collect::<Vec<_>>().as_slice(),
        &scalar_aliases,
        &table_sig_map,
    );
    fs::write(out_dir.join("src/lib.rs"), lib_src)?;

    fs::write(
        out_dir.join("README.md"),
        readme(&crate_name, &opts.provider_id, &opts.sub_ext, &opts.target),
    )?;

    Ok(())
}

fn crate_name_for(opts: &DynlinkOptions) -> String {
    let sub: String = opts
        .sub_ext
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{sub}-sqlite-bridge-dynlink")
}

fn cargo_toml(crate_name: &str, version: &str) -> String {
    format!(
        r#"[package]
name = "{crate_name}"
version = "{version}"
edition = "2021"
description = "Phase A dynlink-mode sqlite bridge — routes SQL dispatch through compose:dynlink/linker against a resident provider."
license = "Apache-2.0"
publish = false

[workspace]
members = ["."]

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = {{ version = "0.41", features = ["macros"] }}
wit-bindgen-rt = {{ version = "0.41", features = ["bitflags"] }}
ciborium = {{ version = "0.2", default-features = false }}
ciborium-io = {{ version = "0.2", default-features = false }}
serde = {{ version = "1", default-features = false, features = ["derive", "alloc"] }}
serde_bytes = {{ version = "0.11", default-features = false, features = ["alloc"] }}
serde_json = {{ version = "1", default-features = false, features = ["alloc"] }}

[profile.release]
opt-level = "s"
lto = true
codegen-units = 1
strip = true
"#,
    )
}

/// Convert a snake_case sub-ext name into a kebab-case WIT package
/// segment that satisfies the component-model rule "each `-`-separated
/// segment must start with `[a-z]`". Underscores become dashes; digit-
/// starting segments (`3d`, `2d`, `4d`) get the same word-form
/// treatment as `scripts/fix-postgis-kebab.sh` (`3d` → `threed`, etc.)
/// so a `postgis_3d` sub-ext produces `postgis-threed`, not
/// `postgis-3d` (which wit-bindgen rejects).
fn kebab_safe_pkg_name(sub_ext: &str) -> String {
    let raw: String = sub_ext
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    raw.split('-')
        .map(|seg| match seg {
            "2" => "two".to_string(),
            "3" => "three".to_string(),
            "4" => "four".to_string(),
            "2d" => "twod".to_string(),
            "3d" => "threed".to_string(),
            "4d" => "fourd".to_string(),
            _ => seg.to_string(),
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn world_wit(sub_ext: &str, has_aggs: bool, has_tables: bool) -> String {
    let pkg = kebab_safe_pkg_name(sub_ext);
    let agg_export = if has_aggs {
        "\n    export sqlite:extension/aggregate-function@1.0.0;"
    } else {
        ""
    };
    let vtab_export = if has_tables {
        "\n    export sqlite:extension/vtab@1.0.0;"
    } else {
        ""
    };
    format!(
        r#"package sqlite-bridge:{pkg}@0.1.0;

/// Phase A dynlink-mode sqlite bridge.
///
/// The bridge imports `compose:dynlink/linker` for outbound
/// dispatch to a resident provider and exports the declarative
/// `sqlite:extension@1.0.0` metadata + scalar-function pair
/// (plus `aggregate-function` when the catalog surfaces any
/// aggregate / window functions, and `vtab` when it surfaces any
/// table-functions). The host reads `metadata.describe()` at
/// load, installs sqlite3 trampolines against every advertised
/// scalar / aggregate / vtab, and routes per-row calls back
/// through the matching dispatch export.
world bridge {{
    import compose:dynlink/linker@0.1.0;

    // sqlite:extension imports needed by the exports' types.
    import sqlite:extension/types@1.0.0;
    import sqlite:extension/policy@1.0.0;

    export sqlite:extension/metadata@1.0.0;
    export sqlite:extension/scalar-function@1.0.0;{agg_export}{vtab_export}
}}
"#,
    )
}

/// Copy the vendored WIT trees the emitted bridge needs into
/// `deps_dir`. Sources:
///
///   * `compose:dynlink` + `sys:compose` — from `datalink-dynlink`'s
///     WIT tree (the definitive copy for this repo).
///   * `sqlite:extension` — from `~/git/sqlink/sqlite-wit/wit/…`
///     (the fresh @1.0.0 contract).
fn populate_deps(deps_dir: &Path) -> Result<()> {
    // compose-dynlink + sys-compose from datalink-dynlink.
    let dynlink_root = std::env::var("DATALINK_DYNLINK_WIT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("git/datalink/crates/datalink-dynlink/wit")
        });
    let compose_dynlink_from = dynlink_root.join("compose-dynlink");
    let sys_compose_from = dynlink_root.join("compose-dynlink/deps/sys-compose");
    if !compose_dynlink_from.is_dir() {
        return Err(anyhow!(
            "compose:dynlink WIT source missing: {} (set DATALINK_DYNLINK_WIT)",
            compose_dynlink_from.display()
        ));
    }
    if !sys_compose_from.is_dir() {
        return Err(anyhow!(
            "sys:compose WIT source missing: {}",
            sys_compose_from.display()
        ));
    }
    let compose_dst = deps_dir.join("compose-dynlink");
    fs::create_dir_all(&compose_dst)?;
    for name in ["package.wit", "linker.wit", "endpoint.wit"] {
        let f = compose_dynlink_from.join(name);
        if f.is_file() {
            copy_kebab_fixed(&f, &compose_dst.join(name))?;
        }
    }
    copy_tree_kebab_fixed(&sys_compose_from, &deps_dir.join("sys-compose"))?;

    // sqlite:extension contract package. `SQLINK_WIT` should point
    // at the sqlite-wit tree (defaults to
    // `~/git/sqlink/sqlite-wit/wit/sqlite-extension/`); we copy the
    // whole tree since `policy` uses `http.method` from `host-spi`
    // and `metadata` uses `types + policy`, so trimming is fragile.
    // The `worlds/` subdirectory (if any) is skipped — the bridge
    // synthesises its own world at `wit/world.wit`.
    let sqlite_from = std::env::var("SQLINK_WIT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("git/sqlink/sqlite-wit/wit/sqlite-extension")
        });
    if !sqlite_from.is_dir() {
        return Err(anyhow!(
            "sqlite:extension WIT source missing: {} (set SQLINK_WIT)",
            sqlite_from.display()
        ));
    }
    let sqlite_dst = deps_dir.join("sqlite-extension");
    fs::create_dir_all(&sqlite_dst)?;
    for entry in fs::read_dir(&sqlite_from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src = entry.path();
        if ty.is_file() {
            // Skip the upstream world file — the bridge world
            // lives at wit/world.wit and is synthesised above.
            if src.file_name().and_then(|s| s.to_str()) == Some("world.wit") {
                continue;
            }
            let dst = sqlite_dst.join(entry.file_name());
            copy_kebab_fixed(&src, &dst)?;
        }
    }
    Ok(())
}

fn copy_kebab_fixed(src: &Path, dst: &Path) -> Result<()> {
    let bytes = fs::read(src)?;
    if src.extension().and_then(|e| e.to_str()) == Some("wit") {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let fixed = datalink_shim_codegen_core::kebab_fix::kebab_fix_wit(&text);
        fs::write(dst, fixed)?;
    } else {
        fs::write(dst, bytes)?;
    }
    Ok(())
}

fn copy_tree_kebab_fixed(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if ty.is_dir() {
            copy_tree_kebab_fixed(&src, &dst)?;
        } else if ty.is_file() {
            copy_kebab_fixed(&src, &dst)?;
        }
    }
    Ok(())
}

fn readme(crate_name: &str, provider_id: &str, sub_ext: &str, target: &str) -> String {
    format!(
        "# {crate_name}\n\
         \n\
         Phase A dynlink-mode sqlite bridge for `{sub_ext}` (target `{target}`).\n\
         \n\
         Exports the declarative `sqlite:extension@1.0.0` metadata + scalar-\n\
         function contract. `metadata.describe()` advertises every scalar the\n\
         catalog names; `scalar-function.call(func-id, args)` routes per-row\n\
         invocations through `compose:dynlink/linker` against the resident\n\
         provider `{provider_id}`. Aggregate / vtab / hook exports are\n\
         deferred to a follow-up.\n"
    )
}

// ============================================================
// src/lib.rs generation
// ============================================================

fn lib_rs(
    provider_id: &str,
    extension_root: &str,
    catalog_extension: &str,
    version: &str,
    functions: &[&(FnKind, String)],
    scalar_aliases: &[(String, String)],
    table_sig_map: &HashMap<String, TableSig>,
) -> String {
    let mut scalar_names: Vec<&str> = functions
        .iter()
        .filter(|(k, _)| *k == FnKind::Scalar)
        .map(|(_, n)| n.as_str())
        .collect();
    scalar_names.sort();
    scalar_names.dedup();

    // Phase 9.3.next partitioning: aggregates + windows share the
    // `aggregate-function-spec` list (windows get `is_window=true`);
    // table-functions become eponymous `vtab-spec` entries.
    let mut aggregate_names: Vec<&str> = functions
        .iter()
        .filter(|(k, _)| *k == FnKind::Aggregate)
        .map(|(_, n)| n.as_str())
        .collect();
    aggregate_names.sort();
    aggregate_names.dedup();
    let mut window_names: Vec<&str> = functions
        .iter()
        .filter(|(k, _)| *k == FnKind::Window)
        .map(|(_, n)| n.as_str())
        .collect();
    window_names.sort();
    window_names.dedup();
    let mut table_names: Vec<&str> = functions
        .iter()
        .filter(|(k, _)| *k == FnKind::Table)
        .map(|(_, n)| n.as_str())
        .collect();
    table_names.sort();
    table_names.dedup();

    let has_aggs = !aggregate_names.is_empty() || !window_names.is_empty();
    let has_tables = !table_names.is_empty();

    // Emit the compact→canonical alias table as a `match` arm body
    // consumed by the `canonical_for` helper below. Only aliases
    // that appear as scalars in this bridge's dispatch set contribute
    // an arm — if the alias isn't a name we register with sqlite,
    // there's no dispatch site that could reach it. Aliases whose
    // canonical form is missing from the dispatch set are dropped
    // too (no arm to route to).
    // Include table-function names in the alias arm filter — both
    // `st_dumppoints` (compact) and `st_dump_points` (canonical) live
    // in `table_names`, and both need to be in the emitted
    // `canonical_for` for the UDTF vtab.filter path to route them
    // through their long form before the kebab-cased method hits the
    // provider dispatch.
    let scalar_name_set: std::collections::BTreeSet<&str> = scalar_names
        .iter()
        .chain(table_names.iter())
        .copied()
        .collect();
    let mut alias_arms = String::new();
    for (alias, canonical) in scalar_aliases {
        if scalar_name_set.contains(alias.as_str())
            && scalar_name_set.contains(canonical.as_str())
        {
            let a = alias.replace('"', "\\\"");
            let c = canonical.replace('"', "\\\"");
            alias_arms.push_str(&format!("            \"{a}\" => \"{c}\",\n"));
        }
    }

    // Build the func-id ↔ name lookup and the ScalarFunctionSpec
    // list body for `metadata.describe()`. Ids start at 1 (id 0 is
    // reserved as a manifest sentinel).
    let mut scalar_id_arms = String::new();
    let mut scalar_specs = String::new();
    for (idx, name) in scalar_names.iter().enumerate() {
        let id = (idx + 1) as u64;
        let escaped = name.replace('"', "\\\"");
        scalar_id_arms.push_str(&format!(
            "        {id} => Some(\"{escaped}\"),\n"
        ));
        // Phase A dynlink advertises every scalar with num_args=-1
        // (variadic). The catalog carries no arity info; declaring
        // -1 lets sqlite route calls of any arity through
        // `scalar-function.call`, where the provider can inspect
        // args.len() and reject if needed. TODO(phase-B): thread
        // arity from `datalink-shim-codegen-core::interface_db`
        // once the catalog carries the shape.
        scalar_specs.push_str(&format!(
            r#"            ScalarFunctionSpec {{
                id: {id},
                name: "{escaped}".to_string(),
                num_args: -1,
                func_flags: FunctionFlags::DETERMINISTIC,
            }},
"#,
        ));
    }

    // ── Aggregate + window emission ──
    //
    // Aggregate ids start at 1_000_000 (well above the scalar
    // namespace so no arm-index confusion at debug time);
    // windows follow contiguously after the aggregates in the same
    // namespace with `is_window=true`. Both share the
    // `aggregate_name_by_id` lookup and the `AggregateGuest`
    // dispatch — the provider owns semantic distinctions between
    // aggregate-only and window-mode calls.
    let mut aggregate_specs = String::new();
    let mut aggregate_id_arms = String::new();
    let agg_base_id: u64 = 1_000_000;
    for (idx, name) in aggregate_names.iter().enumerate() {
        let id = agg_base_id + idx as u64;
        let escaped = name.replace('"', "\\\"");
        aggregate_id_arms.push_str(&format!(
            "        {id} => Some(\"{escaped}\"),\n"
        ));
        aggregate_specs.push_str(&format!(
            r#"            AggregateFunctionSpec {{
                id: {id},
                name: "{escaped}".to_string(),
                num_args: -1,
                func_flags: FunctionFlags::empty(),
                is_window: false,
            }},
"#,
        ));
    }
    let win_base_id: u64 = agg_base_id + aggregate_names.len() as u64;
    for (idx, name) in window_names.iter().enumerate() {
        let id = win_base_id + idx as u64;
        let escaped = name.replace('"', "\\\"");
        aggregate_id_arms.push_str(&format!(
            "        {id} => Some(\"{escaped}\"),\n"
        ));
        aggregate_specs.push_str(&format!(
            r#"            AggregateFunctionSpec {{
                id: {id},
                name: "{escaped}".to_string(),
                num_args: -1,
                func_flags: FunctionFlags::empty(),
                is_window: true,
            }},
"#,
        ));
    }

    // ── Vtab (table-function) emission ──
    //
    // Table-functions surface as eponymous vtabs. Vtab ids start at
    // 2_000_000 to keep the debug namespaces disjoint from scalars
    // (1..) and aggregates (1_000_000..).
    let mut vtab_specs = String::new();
    let mut vtab_id_arms = String::new();
    let vtab_base_id: u64 = 2_000_000;
    for (idx, name) in table_names.iter().enumerate() {
        let id = vtab_base_id + idx as u64;
        let escaped = name.replace('"', "\\\"");
        vtab_id_arms.push_str(&format!(
            "        {id} => Some(\"{escaped}\"),\n"
        ));
        vtab_specs.push_str(&format!(
            r#"            VtabSpec {{
                id: {id},
                name: "{escaped}".to_string(),
                eponymous: true,
                mutable: false,
                batched: false,
            }},
"#,
        ));
    }

    // ── Conditional emit blocks for aggregate + vtab surfaces ──
    //
    // Both surfaces are structurally optional: when the catalog
    // carries no aggregates / windows / table-functions the world
    // omits the corresponding export and the lib.rs skips the
    // guest impl entirely (empty blocks). When present, the guest
    // impl routes each call through the same CBOR envelope the
    // scalar path uses.
    let (agg_import, agg_manifest_local, agg_manifest_field, agg_impl_block) = if has_aggs {
        let import = "use bindings::exports::sqlite::extension::aggregate_function::Guest as AggregateGuest;\nuse bindings::exports::sqlite::extension::metadata::AggregateFunctionSpec;\n";
        let manifest_local = format!(
            "        let aggregate_functions: Vec<AggregateFunctionSpec> = vec![\n{aggregate_specs}        ];\n",
        );
        let manifest_field = "aggregate_functions,";
        // Aggregate + window dispatch — accumulator-per-context,
        // ships accumulated CBOR-encoded rows to the provider on
        // finalize / value. Sequential context lifetime: one context
        // is opened by SQLite for each aggregation group, streams rows
        // through `step`, and closes on `finalize`. Windows additionally
        // support `inverse` (row removal from a sliding frame) and
        // `value` (peek at intermediate).
        let impl_block = format!(
            r##"

fn aggregate_name_by_id(id: u64) -> Option<&'static str> {{
    match id {{
{aggregate_id_arms}        _ => None,
    }}
}}

fn agg_accumulator() -> &'static std::sync::Mutex<std::collections::HashMap<u64, Vec<CborValue>>> {{
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u64, Vec<CborValue>>>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}}

fn aggregate_method_for(func_id: u64, suffix: &str) -> Result<String, String> {{
    let name = aggregate_name_by_id(func_id)
        .ok_or_else(|| format!("unknown aggregate id {{}}", func_id))?;
    let canonical = canonical_for(name);
    // Provider method naming convention: `<kebab-name>-<suffix>`
    // where suffix is one of `step` (unused — step accumulates
    // locally), `finalize`, `value`, or `inverse`. The provider
    // owns the aggregate semantics; the bridge is a pure CBOR
    // tunnel that ships the accumulated per-row args in one shot.
    Ok(format!("{{}}-{{}}", canonical.replace('_', "-"), suffix))
}}

impl AggregateGuest for Component {{
    fn step(func_id: u64, context_id: u64, args: Vec<SqlValue>) -> Result<(), String> {{
        // SQL-aggregate NULL-row semantics: any NULL contribution
        // is skipped (mirrors sqlite3's built-in count(*)/sum()).
        if args.iter().any(|v| matches!(v, SqlValue::Null)) {{
            return Ok(());
        }}
        let _ = aggregate_name_by_id(func_id)
            .ok_or_else(|| format!("unknown aggregate id {{}}", func_id))?;
        let cbor_args: Vec<CborValue> = args
            .iter()
            .map(sqlv_to_cbor)
            .collect::<Result<Vec<_>, String>>()?;
        agg_accumulator()
            .lock()
            .expect("aggregate accumulator mutex poisoned")
            .entry(context_id)
            .or_insert_with(Vec::new)
            .push(CborValue::List(cbor_args));
        Ok(())
    }}

    fn finalize(func_id: u64, context_id: u64) -> Result<SqlValue, String> {{
        let method = aggregate_method_for(func_id, "finalize")?;
        let rows = agg_accumulator()
            .lock()
            .expect("aggregate accumulator mutex poisoned")
            .remove(&context_id)
            .unwrap_or_default();
        // Empty aggregation → NULL per SQL semantics (sum() over 0
        // rows is NULL, etc.). Providers that want a domain-specific
        // zero can encode it in their finalize handler.
        if rows.is_empty() {{
            return Ok(SqlValue::Null);
        }}
        let resp = call(&method, vec![CborValue::List(rows)])?;
        response_to_sqlv(resp)
    }}

    fn value(func_id: u64, context_id: u64) -> Result<SqlValue, String> {{
        let method = aggregate_method_for(func_id, "value")?;
        let rows = agg_accumulator()
            .lock()
            .expect("aggregate accumulator mutex poisoned")
            .get(&context_id)
            .cloned()
            .unwrap_or_default();
        if rows.is_empty() {{
            return Ok(SqlValue::Null);
        }}
        let resp = call(&method, vec![CborValue::List(rows)])?;
        response_to_sqlv(resp)
    }}

    fn inverse(func_id: u64, context_id: u64, _args: Vec<SqlValue>) -> Result<(), String> {{
        let _ = aggregate_name_by_id(func_id)
            .ok_or_else(|| format!("unknown aggregate id {{}}", func_id))?;
        agg_accumulator()
            .lock()
            .expect("aggregate accumulator mutex poisoned")
            .entry(context_id)
            .and_modify(|v| {{ v.pop(); }});
        Ok(())
    }}
}}
"##,
        );
        (import.to_string(), manifest_local, manifest_field, impl_block)
    } else {
        (String::new(), String::new(), "aggregate_functions: vec![],", String::new())
    };

    // ── Per-vtab connect schema arms ──
    //
    // For every table_function surfaced above, build a match arm that
    // returns a `CREATE TABLE x(<real cols>, <hidden argv slots>)`
    // schema string. Real column names + affinities come from the
    // shim-interface DB's `table_functions.output_columns_json`;
    // argv arity comes from `param_types_json`'s length. When the DB
    // wasn't threaded through (or a specific fn's row is missing /
    // pre-B5), that arm falls back to the pre-schema-lift shape so a
    // codegen run without `--interface` still produces a valid crate.
    //
    // Schema fallback (both when no sig, and as the final `_` arm for
    // unknown vtab ids the manifest never advertised): one opaque
    // `result` BLOB column + four hidden `_arg0..3` slots. The four-
    // hidden-slot upper bound tracks the max UDTF arity in current
    // postgis / mobilitydb catalogs (5) — the query planner leaves
    // unused slots alone and binds the used ones through xBestIndex.
    let mut vtab_connect_arms = String::new();
    let mut missing_vtab_sig_count = 0usize;
    for (idx, name) in table_names.iter().enumerate() {
        let id = vtab_base_id + idx as u64;
        let schema = match table_sig_map.get(*name) {
            Some(sig) => {
                let mut parts: Vec<String> = Vec::new();
                let cols = sig.output_columns.as_deref();
                match cols {
                    Some(cols) if !cols.is_empty() => {
                        for (col_name, tok) in cols {
                            let escaped = col_name.replace('"', "\\\"");
                            let ty = token_to_sqlite_type(tok);
                            parts.push(format!("\\\"{escaped}\\\" {ty}"));
                        }
                    }
                    _ => {
                        // A row with a known arity but no output_schema
                        // arm — pre-B5 wire shape. Preserve the opaque
                        // single-BLOB column so the provider's List-of-
                        // Bytes response still lands somewhere.
                        parts.push("\\\"result\\\" BLOB".to_string());
                    }
                }
                for i in 0..sig.param_tokens.len() {
                    parts.push(format!("\\\"_arg{i}\\\" HIDDEN"));
                }
                // Every eponymous vtab needs at least one HIDDEN argv
                // slot — otherwise SQLite can't route positional args
                // from the SELECT-from-fn(...) syntax. When arity is
                // zero (unlikely but valid), synthesise one slot so
                // the query planner still has something to bind.
                if sig.param_tokens.is_empty() {
                    parts.push("\\\"_arg0\\\" HIDDEN".to_string());
                }
                format!("CREATE TABLE x({})", parts.join(", "))
            }
            None => {
                missing_vtab_sig_count += 1;
                // Pre-schema-lift fallback shape. Byte-identical to
                // what the previous emit hard-coded for every vtab.
                "CREATE TABLE x(\\\"result\\\" BLOB, \
                 \\\"_arg0\\\" HIDDEN, \\\"_arg1\\\" HIDDEN, \\\"_arg2\\\" HIDDEN, \\\"_arg3\\\" HIDDEN)"
                    .to_string()
            }
        };
        vtab_connect_arms.push_str(&format!(
            "                {id} => Ok(\"{schema}\".to_string()),\n"
        ));
    }
    if missing_vtab_sig_count > 0 {
        eprintln!(
            "[sqlite-dynlink-emit] warning: {missing_vtab_sig_count} table_function(s) had no \
             shim-interface signature (opaque single-BLOB vtab schema fallback used)."
        );
    }

    let (vtab_import, vtab_manifest_local, vtab_manifest_field, vtab_impl_block) = if has_tables {
        let import = "use bindings::exports::sqlite::extension::vtab::{Guest as VtabGuest, IndexInfo, IndexPlan, VtabRow};\nuse bindings::exports::sqlite::extension::metadata::VtabSpec;\n";
        let manifest_local = format!(
            "        let vtabs: Vec<VtabSpec> = vec![\n{vtab_specs}        ];\n",
        );
        let manifest_field = "vtabs,";
        // Table-function (eponymous vtab) dispatch. The Phase A
        // wire: `filter` invokes the provider's `<name>` method
        // with the argv values, expects a CBOR list of rows back
        // (each row = one BLOB), and buffers them; `next`/`eof`/
        // `column`/`rowid` stream from the buffer. Multi-column
        // rowsets remain a follow-up: rows are advertised as a
        // single BLOB column plus one HIDDEN column per argv slot
        // (matching the eponymous vtab schema shape the legacy
        // monolithic bridge uses for st_dump).
        let impl_block = format!(
            r##"

fn vtab_name_by_id(id: u64) -> Option<&'static str> {{
    match id {{
{vtab_id_arms}        _ => None,
    }}
}}

// Per-cursor state: buffered result rows + current position.
// Keyed by (vtab_id, cursor_id) since cursor ids may collide
// across vtabs in the host's per-instance allocation strategy.
fn cursor_state()
    -> &'static std::sync::Mutex<std::collections::HashMap<(u64, u64), (Vec<Vec<u8>>, usize)>>
{{
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<(u64, u64), (Vec<Vec<u8>>, usize)>>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}}

impl VtabGuest for Component {{
    fn create(
        vtab_id: u64,
        instance_id: u64,
        db_name: String,
        table_name: String,
        args: Vec<String>,
    ) -> Result<String, String> {{
        // Eponymous vtabs (which is all we advertise) see only
        // connect(). Fall through so a caller who does hit create()
        // gets the same schema.
        <Self as VtabGuest>::connect(vtab_id, instance_id, db_name, table_name, args)
    }}

    fn connect(
        vtab_id: u64,
        _instance_id: u64,
        _db_name: String,
        _table_name: String,
        _args: Vec<String>,
    ) -> Result<String, String> {{
        // Per-vtab CREATE TABLE schema. Each arm was emitted from
        // the shim-interface DB's `table_functions.output_columns_json`
        // + `param_types_json.len()` — real output column names +
        // affinities, hidden slot per positional argv. The final `_`
        // arm covers vtab ids the manifest never advertised (unknown
        // to this bridge — surface as an error so callers see the
        // mismatch immediately) and functions whose shim-interface row
        // predates the B5 `output_columns_json` schema (opaque single-
        // BLOB fallback with 4 hidden argv slots).
        match vtab_id {{
{vtab_connect_arms}            _ => Err(format!("unknown vtab id {{}}", vtab_id)),
        }}
    }}

    fn destroy(_vtab_id: u64, _instance_id: u64) -> Result<(), String> {{ Ok(()) }}
    fn disconnect(_vtab_id: u64, _instance_id: u64) -> Result<(), String> {{ Ok(()) }}

    fn best_index(_vtab_id: u64, _instance_id: u64, info: IndexInfo) -> Result<IndexPlan, String> {{
        use bindings::exports::sqlite::extension::vtab::ConstraintUsage;
        let mut next_argv_idx: i32 = 1;
        let constraint_usage = info
            .constraints
            .iter()
            .map(|c| {{
                if c.usable {{
                    let ci = ConstraintUsage {{ argv_index: next_argv_idx, omit: true }};
                    next_argv_idx += 1;
                    ci
                }} else {{
                    ConstraintUsage {{ argv_index: 0, omit: false }}
                }}
            }})
            .collect();
        Ok(IndexPlan {{
            constraint_usage,
            idx_num: 0,
            idx_str: None,
            estimated_cost: 1.0,
            estimated_rows: 100,
            orderby_consumed: false,
        }})
    }}

    fn open(_vtab_id: u64, _instance_id: u64, _cursor_id: u64) -> Result<(), String> {{ Ok(()) }}

    fn close(vtab_id: u64, cursor_id: u64) -> Result<(), String> {{
        cursor_state()
            .lock()
            .expect("vtab cursor state mutex poisoned")
            .remove(&(vtab_id, cursor_id));
        Ok(())
    }}

    fn filter(
        vtab_id: u64,
        cursor_id: u64,
        _idx_num: i32,
        _idx_str: Option<String>,
        args: Vec<SqlValue>,
    ) -> Result<(), String> {{
        let name = vtab_name_by_id(vtab_id)
            .ok_or_else(|| format!("unknown vtab id {{}}", vtab_id))?;
        let canonical = canonical_for(name);
        let method = canonical.replace('_', "-");
        let cbor_args: Vec<CborValue> = args
            .iter()
            .map(sqlv_to_cbor)
            .collect::<Result<Vec<_>, String>>()?;
        let resp = call(&method, cbor_args)?;
        // Response shape: List of Bytes (one blob per row). Any
        // other shape → single-row wrapper (best-effort).
        let rows: Vec<Vec<u8>> = match resp {{
            ResponseValue::List(items) => items
                .into_iter()
                .map(|item| match item {{
                    ResponseValue::Bytes(b) => b,
                    ResponseValue::Null => Vec::new(),
                    other => {{
                        // Non-blob row → serialise as CBOR so callers
                        // still see a byte payload.
                        let mut out = Vec::new();
                        let _ = ciborium::into_writer(&response_value_to_cbor(other), &mut out);
                        out
                    }}
                }})
                .collect(),
            ResponseValue::Null => Vec::new(),
            other => {{
                let mut out = Vec::new();
                let _ = ciborium::into_writer(&response_value_to_cbor(other), &mut out);
                vec![out]
            }}
        }};
        cursor_state()
            .lock()
            .expect("vtab cursor state mutex poisoned")
            .insert((vtab_id, cursor_id), (rows, 0));
        Ok(())
    }}

    fn next(vtab_id: u64, cursor_id: u64) -> Result<(), String> {{
        let mut guard = cursor_state()
            .lock()
            .expect("vtab cursor state mutex poisoned");
        if let Some((_, pos)) = guard.get_mut(&(vtab_id, cursor_id)) {{
            *pos += 1;
        }}
        Ok(())
    }}

    fn eof(vtab_id: u64, cursor_id: u64) -> bool {{
        let guard = cursor_state()
            .lock()
            .expect("vtab cursor state mutex poisoned");
        match guard.get(&(vtab_id, cursor_id)) {{
            Some((rows, pos)) => *pos >= rows.len(),
            None => true,
        }}
    }}

    fn column(vtab_id: u64, cursor_id: u64, col: i32) -> Result<SqlValue, String> {{
        let guard = cursor_state()
            .lock()
            .expect("vtab cursor state mutex poisoned");
        let (rows, pos) = guard
            .get(&(vtab_id, cursor_id))
            .ok_or_else(|| format!("no cursor state for vtab {{}}/cursor {{}}", vtab_id, cursor_id))?;
        if *pos >= rows.len() {{
            return Err("column past EOF".to_string());
        }}
        // Column 0 = result BLOB; hidden columns (>=1) round-trip
        // the argv back as-is (the query planner already bound them).
        if col == 0 {{
            Ok(SqlValue::Blob(rows[*pos].clone()))
        }} else {{
            Ok(SqlValue::Null)
        }}
    }}

    fn rowid(vtab_id: u64, cursor_id: u64) -> Result<i64, String> {{
        let guard = cursor_state()
            .lock()
            .expect("vtab cursor state mutex poisoned");
        let (_, pos) = guard
            .get(&(vtab_id, cursor_id))
            .ok_or_else(|| format!("no cursor state for vtab {{}}/cursor {{}}", vtab_id, cursor_id))?;
        Ok(*pos as i64)
    }}

    fn fetch_batch(
        _vtab_id: u64,
        _cursor_id: u64,
        _max_rows: u32,
    ) -> Result<Vec<VtabRow>, String> {{
        // Per-vtab opt-in via `vtab-spec.batched = true`. This
        // bridge advertises `batched: false` for every vtab so the
        // host never routes here — return the sentinel error the
        // host's cli trampoline probes for.
        Err("not implemented".to_string())
    }}
}}

fn response_value_to_cbor(v: ResponseValue) -> CborValue {{
    match v {{
        ResponseValue::Null => CborValue::Null,
        ResponseValue::Bool(b) => CborValue::Bool(b),
        ResponseValue::Int(i) => CborValue::Int(i),
        ResponseValue::Float(f) => CborValue::Float(f),
        ResponseValue::Text(t) => CborValue::Text(t),
        ResponseValue::Bytes(b) => CborValue::Bytes(b),
        ResponseValue::List(items) => CborValue::List(
            items.into_iter().map(response_value_to_cbor).collect(),
        ),
    }}
}}
"##,
        );
        (import.to_string(), manifest_local, manifest_field, impl_block)
    } else {
        (String::new(), String::new(), "vtabs: vec![],", String::new())
    };

    let extension_root = extension_root.to_string();
    let catalog_extension = catalog_extension.to_string();

    format!(
        r##"//! Auto-generated by `datalink_shim_sqlite_dynlink_emit::emit_dynlink`
//! (Phase A, opaque-blob scalar dispatch). Do NOT edit by hand — regenerate.
#![allow(unused_imports, dead_code)]
#![allow(unsafe_op_in_unsafe_fn)]

mod bindings {{
    wit_bindgen::generate!({{
        path: "wit",
        world: "bridge",
        generate_all,
    }});
}}

use bindings::exports::sqlite::extension::metadata::{{
    Guest as MetadataGuest, Manifest, ScalarFunctionSpec,
}};
use bindings::exports::sqlite::extension::scalar_function::Guest as ScalarFunctionGuest;
{agg_import}{vtab_import}use bindings::sqlite::extension::types::{{FunctionFlags, SqlValue}};

use bindings::compose::dynlink::linker;

const PROVIDER_ID: &str = "{provider_id}";
const EXTENSION_ROOT: &str = "{extension_root}";
const CATALOG_EXTENSION: &str = "{catalog_extension}";
const CATALOG_VERSION: &str = "{version}";

fn resolve() -> Result<linker::Instance, String> {{
    linker::resolve_by_id(&PROVIDER_ID.to_string())
        .map_err(|e| format!("dynlink resolve('{{}}'): {{:?}}", PROVIDER_ID, e))
}}

// -----------------------------------------------------------
// CBOR envelope (mirrors provider crate's Request/Response).
// -----------------------------------------------------------

#[derive(Debug, Clone)]
enum CborValue {{
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    List(Vec<CborValue>),
}}

impl serde::Serialize for CborValue {{
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {{
        match self {{
            CborValue::Null => s.serialize_unit(),
            CborValue::Bool(b) => s.serialize_bool(*b),
            CborValue::Int(i) => s.serialize_i64(*i),
            CborValue::Float(f) => s.serialize_f64(*f),
            CborValue::Text(t) => s.serialize_str(t),
            CborValue::Bytes(b) => s.serialize_bytes(b),
            CborValue::List(items) => {{
                use serde::ser::SerializeSeq;
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {{
                    seq.serialize_element(item)?;
                }}
                seq.end()
            }}
        }}
    }}
}}

#[derive(Debug, Clone, serde::Serialize)]
struct Request {{
    #[serde(rename = "v")]
    version: u32,
    args: Vec<CborValue>,
}}

#[derive(Debug, Clone, serde::Deserialize)]
struct Response {{
    #[serde(default)]
    ok: Option<ResponseValue>,
    #[serde(default)]
    err: Option<String>,
}}

#[derive(Debug, Clone)]
enum ResponseValue {{
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    List(Vec<ResponseValue>),
}}

impl<'de> serde::Deserialize<'de> for ResponseValue {{
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {{
        use serde::de::{{Error, MapAccess, SeqAccess, Visitor}};
        struct V;
        impl<'de> Visitor<'de> for V {{
            type Value = ResponseValue;
            fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {{
                f.write_str("a CBOR value (null, bool, int, float, text, bytes, list)")
            }}
            fn visit_unit<E: Error>(self) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Null) }}
            fn visit_none<E: Error>(self) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Null) }}
            fn visit_bool<E: Error>(self, v: bool) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Bool(v)) }}
            fn visit_i64<E: Error>(self, v: i64) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Int(v)) }}
            fn visit_u64<E: Error>(self, v: u64) -> Result<ResponseValue, E> {{
                if v <= i64::MAX as u64 {{ Ok(ResponseValue::Int(v as i64)) }}
                else {{ Err(E::custom("u64 overflow to i64")) }}
            }}
            fn visit_f64<E: Error>(self, v: f64) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Float(v)) }}
            fn visit_str<E: Error>(self, v: &str) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Text(v.to_string())) }}
            fn visit_string<E: Error>(self, v: String) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Text(v)) }}
            fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Bytes(v.to_vec())) }}
            fn visit_byte_buf<E: Error>(self, v: Vec<u8>) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Bytes(v)) }}
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<ResponseValue, A::Error> {{
                let mut items = Vec::new();
                while let Some(v) = s.next_element::<ResponseValue>()? {{ items.push(v); }}
                Ok(ResponseValue::List(items))
            }}
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<ResponseValue, A::Error> {{
                let k: Option<String> = m.next_key()?;
                let k = k.ok_or_else(|| A::Error::custom("empty map"))?;
                let v = match k.as_str() {{
                    "Null" => {{ let _: serde::de::IgnoredAny = m.next_value()?; ResponseValue::Null }}
                    "Bool" => ResponseValue::Bool(m.next_value()?),
                    "Int"  => ResponseValue::Int(m.next_value()?),
                    "Float" => ResponseValue::Float(m.next_value()?),
                    "Text" => ResponseValue::Text(m.next_value()?),
                    "Bytes" => {{
                        let b: serde_bytes::ByteBuf = m.next_value()?;
                        ResponseValue::Bytes(b.into_vec())
                    }}
                    "List" => ResponseValue::List(m.next_value()?),
                    other => return Err(A::Error::custom(format!("unknown tag: {{}}", other))),
                }};
                Ok(v)
            }}
        }}
        d.deserialize_any(V)
    }}
}}

fn encode_request(args: Vec<CborValue>) -> Result<Vec<u8>, String> {{
    let mut out = Vec::new();
    ciborium::into_writer(&Request {{ version: 1, args }}, &mut out)
        .map_err(|e| format!("cbor encode: {{}}", e))?;
    Ok(out)
}}

fn decode_response(bytes: &[u8]) -> Result<Response, String> {{
    ciborium::from_reader(bytes).map_err(|e| format!("cbor decode: {{}}", e))
}}

fn call(method: &str, args: Vec<CborValue>) -> Result<ResponseValue, String> {{
    let inst = resolve()?;
    let payload = encode_request(args)?;
    let bytes = inst
        .invoke(&method.to_string(), &payload)
        .map_err(|e| format!("{{}}: invoke: {{:?}}", method, e))?;
    let resp = decode_response(&bytes)?;
    if let Some(err) = resp.err {{
        return Err(format!("{{}}: {{}}", method, err));
    }}
    Ok(resp.ok.unwrap_or(ResponseValue::Null))
}}

// -----------------------------------------------------------
// SqlValue marshalling — variant discipline per the @1.0.0
// contract. The `wit-value` arm is Phase-A-out-of-scope; the
// bridge treats it as null in both directions.
// -----------------------------------------------------------

fn sqlv_to_cbor(v: &SqlValue) -> Result<CborValue, String> {{
    Ok(match v {{
        SqlValue::Null => CborValue::Null,
        SqlValue::Integer(i) => CborValue::Int(*i),
        SqlValue::Real(f) => CborValue::Float(*f),
        SqlValue::Text(t) => CborValue::Text(t.clone()),
        SqlValue::Blob(b) => CborValue::Bytes(b.clone()),
        // WitValue is Phase-A-out-of-scope. Prior code silently
        // downgraded it to CBOR null, which lied to the provider
        // (an explicit-typed argument became indistinguishable
        // from a real NULL). Surface an explicit error instead so
        // callers see a diagnostic on the SQL side.
        SqlValue::WitValue(_) => {{
            return Err("bridge: WitValue arg not supported in dynlink Phase A".to_string());
        }}
    }})
}}

fn response_to_sqlv(v: ResponseValue) -> Result<SqlValue, String> {{
    Ok(match v {{
        ResponseValue::Null => SqlValue::Null,
        ResponseValue::Bool(b) => SqlValue::Integer(if b {{ 1 }} else {{ 0 }}),
        ResponseValue::Int(i) => SqlValue::Integer(i),
        ResponseValue::Float(f) => SqlValue::Real(f),
        ResponseValue::Text(t) => SqlValue::Text(t),
        ResponseValue::Bytes(b) => SqlValue::Blob(b),
        // A list-shaped response has no SQLite scalar sqlvalue
        // arm. Prior code silently returned SqlValue::Null, hiding
        // a shape mismatch behind an implicit NULL. Return an
        // explicit error so the SQL side sees a diagnostic.
        ResponseValue::List(_) => {{
            return Err("bridge: list-shaped response not supported".to_string());
        }}
    }})
}}

fn scalar_name_by_id(id: u64) -> Option<&'static str> {{
    match id {{
{scalar_id_arms}        _ => None,
    }}
}}

/// SQL-name aliasing: translate a compact-form SQL name (e.g.
/// `st_geomfromtext`) to its WIT-canonical (long) form
/// (`st_geom_from_text`) before kebab-casing to the provider
/// method name. The catalog carries both spellings as scalars in
/// `leaf.scalars` (both are advertised via `metadata.describe()`
/// so callers can use either), and as first-class `[[aliases]]`
/// entries. The provider dispatch only matches the WIT-canonical
/// form; the bridge's per-row `call` calls `canonical_for(name)`
/// so both spellings route to the same provider arm.
fn canonical_for(name: &str) -> &str {{
    match name {{
{alias_arms}        other => other,
    }}
}}

// -----------------------------------------------------------
// Guest impls.
// -----------------------------------------------------------

struct Component;

impl MetadataGuest for Component {{
    fn describe() -> Manifest {{
        let scalar_functions: Vec<ScalarFunctionSpec> = vec![
{scalar_specs}        ];
{agg_manifest_local}{vtab_manifest_local}        Manifest {{
            name: EXTENSION_ROOT.to_string(),
            version: CATALOG_VERSION.to_string(),
            scalar_functions,
            {agg_manifest_field}
            collations: vec![],
            {vtab_manifest_field}
            dot_commands: vec![],
            has_authorizer: false,
            has_update_hook: false,
            has_commit_hook: false,
            has_wal_hook: false,
            wal_hook_id: 0,
            declared_capabilities: vec![],
            optional_capabilities: vec![],
            preferred_prefix: None,
            prefix_expansion: None,
            typed_values: vec![],
        }}
    }}
}}

impl ScalarFunctionGuest for Component {{
    fn call(func_id: u64, args: Vec<SqlValue>) -> Result<SqlValue, String> {{
        let name = scalar_name_by_id(func_id)
            .ok_or_else(|| format!("unknown function id {{}}", func_id))?;
        // SQL-style null propagation. Providers that need to
        // observe explicit NULL arguments will need a follow-up
        // Phase to opt in per-arm.
        if args.iter().any(|v| matches!(v, SqlValue::Null)) {{
            return Ok(SqlValue::Null);
        }}
        // Alias translation: SQL name → WIT-canonical → provider
        // method (kebab-case). Compact-form SQL names (e.g.
        // `st_geomfromtext`) route through `canonical_for` to their
        // long form (`st_geom_from_text`) before kebab-casing;
        // non-alias names pass through unchanged. See
        // `canonical_for` above.
        let canonical = canonical_for(name);
        let method = canonical.replace('_', "-");
        let cbor_args: Vec<CborValue> = args
            .iter()
            .map(sqlv_to_cbor)
            .collect::<Result<Vec<_>, String>>()?;
        let resp = call(&method, cbor_args)?;
        response_to_sqlv(resp)
    }}
}}
{agg_impl_block}{vtab_impl_block}
bindings::export!(Component with_types_in bindings);
"##,
    )
}
