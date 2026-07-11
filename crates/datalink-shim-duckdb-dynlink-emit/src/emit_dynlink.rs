//! Dynlink-mode duckdb bridge emitter (Phase A, §A.4 Option 1).
//!
//! Emits a bridge crate that dispatches every SQL arm through
//! `compose:dynlink/linker` — CBOR envelope in / CBOR envelope out
//! against a resident provider identified by `opts.provider_id` —
//! instead of the wac-plug-linked WIT interfaces the sibling
//! `datalink-shim-duckdb-emit` produces.
//!
//! Following §A.4 Option 1, every scalar arm is opaque: the
//! `callback-dispatch::call-scalar` row-major singleton path
//! marshals its `duckvalue` args into CBOR (blob-preferred, all
//! primitives faithfully preserved), forwards the request through
//! `linker.resolve-by-id + invoke`, and rewraps the response into
//! a `duckvalue`. The columnar hot paths and every other export
//! (aggregate / cast / table / pragma / index / files / …) are
//! stubbed with `duckerror::unsupported` at Phase A scope.
//!
//! Wire discipline mirrors
//! `postgis-wasm/crates/provider/src/envelope.rs`:
//!
//! ```ignore
//! Request  { v: 1, args: Vec<CborValue> }
//! Response { ok:  Option<CborValue>, err: Option<String> }
//! ```
//!
//! The `CborValue` variants are serialised at their bare CBOR
//! type via a manual `Serialize` — matching the provider-side
//! envelope. See the deep note on `Response::ok` null-collapse
//! rehydration in the emitted `call` function.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::sql_extension_catalog::{Catalog, FnKind, LeavesOverlay};
use crate::DynlinkOptions;

/// Per-scalar signature loaded from the sibling shim-interface DB.
///
/// `param_tokens` is a list of already-normalised type tokens (one of
/// `"binary" | "boolean" | "float64" | "int64" | "text"`) — the
/// `sql_extension_catalog_emit` ingester collapses every upstream WIT type
/// down to that closed set before writing `scalars.param_types_json`.
/// `return_token` is one of the same tokens.
#[derive(Debug, Clone)]
pub(crate) struct ScalarSig {
    pub param_tokens: Vec<String>,
    pub return_token: String,
}

/// Per-aggregate signature loaded from `aggregates` in the shim-interface DB.
///
/// Aggregates carry only `param_types_json` — the return type is
/// domain-dependent and the DB doesn't record it. The dynlink emit
/// registers every aggregate with a Blob return (opaque blob-return);
/// the provider is free to encode a non-blob final value via the
/// CBOR envelope's tagged variants.
#[derive(Debug, Clone)]
pub(crate) struct AggregateSig {
    pub param_tokens: Vec<String>,
}

/// Per-table-function signature loaded from `table_functions` in the
/// shim-interface DB. Same shape as aggregates — no return schema; the
/// dynlink emit declares a single BLOB output column, and the provider
/// streams rows through the CBOR envelope's List-of-Bytes response.
#[derive(Debug, Clone)]
pub(crate) struct TableSig {
    pub param_tokens: Vec<String>,
}

/// Load `SELECT name, param_types_json, return_type FROM scalars
/// WHERE extension = ?` into a name→signature map.
///
/// `param_types_json` is a JSON `array-of-arrays`; postgis today only
/// carries a single overload group per name so we take element 0.
/// Multi-overload extensions land in Phase B: for now we log a
/// warning to stderr and pick the first group deterministically.
pub(crate) fn load_scalar_sigs(
    sqlite: &Path,
    extension: &str,
) -> Result<HashMap<String, ScalarSig>> {
    let conn = rusqlite::Connection::open_with_flags(
        sqlite,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening shim-interface sqlite: {}", sqlite.display()))?;
    let mut stmt = conn.prepare(
        "SELECT name, param_types_json, return_type FROM scalars WHERE extension = ?1",
    )?;
    let rows = stmt.query_map([extension], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out: HashMap<String, ScalarSig> = HashMap::new();
    let mut multi_overload_count = 0usize;
    for row in rows {
        let (name, param_json, return_token) = row?;
        // Parse `[[T,...], ...]` and pick the first overload group.
        // Postgis has exactly one overload per name today; other
        // extensions may not — track a warning count so operators can
        // see if the first-only assumption bites them.
        let outer: Vec<Vec<String>> = serde_json::from_str(&param_json)
            .with_context(|| format!("parsing param_types_json for scalar {name}"))?;
        if outer.len() > 1 {
            multi_overload_count += 1;
        }
        let param_tokens = outer.into_iter().next().unwrap_or_default();
        out.insert(
            name,
            ScalarSig {
                param_tokens,
                return_token,
            },
        );
    }
    if multi_overload_count > 0 {
        eprintln!(
            "[duckdb-dynlink-emit] warning: {multi_overload_count} scalar(s) with >1 overload \
             group; using the first (Phase A behaviour). Multi-overload dispatch lands in Phase B."
        );
    }
    Ok(out)
}

/// Load `SELECT name, param_types_json FROM aggregates WHERE extension = ?`
/// UNIONed with the window_functions surface into a name→signature map.
/// DuckDB doesn't have a separate window-registry — window functions
/// register through the same `aggregate-registry` capability — so the
/// dynlink emit folds both categories into the same signature map at
/// load time.
pub(crate) fn load_aggregate_sigs(
    sqlite: &Path,
    extension: &str,
) -> Result<HashMap<String, AggregateSig>> {
    let conn = rusqlite::Connection::open_with_flags(
        sqlite,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening shim-interface sqlite: {}", sqlite.display()))?;
    let mut out: HashMap<String, AggregateSig> = HashMap::new();
    let mut multi_overload_count = 0usize;
    for source_table in ["aggregates", "window_functions"] {
        let sql = format!(
            "SELECT name, param_types_json FROM {source_table} WHERE extension = ?1"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([extension], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (name, param_json) = row?;
            let outer: Vec<Vec<String>> = serde_json::from_str(&param_json)
                .with_context(|| format!("parsing param_types_json for {source_table}.{name}"))?;
            if outer.len() > 1 {
                multi_overload_count += 1;
            }
            let param_tokens = outer.into_iter().next().unwrap_or_default();
            // Aggregates and window functions can share a name (rare but
            // possible in postgis clustering). Aggregate rows take
            // precedence — they're the more common surface.
            out.entry(name).or_insert(AggregateSig { param_tokens });
        }
    }
    if multi_overload_count > 0 {
        eprintln!(
            "[duckdb-dynlink-emit] warning: {multi_overload_count} aggregate/window(s) with >1 overload \
             group; using the first (Phase A behaviour)."
        );
    }
    Ok(out)
}

/// Load `SELECT name, param_types_json FROM table_functions WHERE extension = ?`
/// into a name→signature map.
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
        "SELECT name, param_types_json FROM table_functions WHERE extension = ?1",
    )?;
    let rows = stmt.query_map([extension], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out: HashMap<String, TableSig> = HashMap::new();
    let mut multi_overload_count = 0usize;
    for row in rows {
        let (name, param_json) = row?;
        let outer: Vec<Vec<String>> = serde_json::from_str(&param_json)
            .with_context(|| format!("parsing param_types_json for table_function {name}"))?;
        if outer.len() > 1 {
            multi_overload_count += 1;
        }
        let param_tokens = outer.into_iter().next().unwrap_or_default();
        out.insert(name, TableSig { param_tokens });
    }
    if multi_overload_count > 0 {
        eprintln!(
            "[duckdb-dynlink-emit] warning: {multi_overload_count} table_function(s) with >1 overload \
             group; using the first (Phase A behaviour)."
        );
    }
    Ok(out)
}

/// Map a sqlite scalar-type token to the WIT-bindgen `Logicaltype`
/// variant literal used at the register call site.
///
/// The token set is closed to the 5 values the shim-interface
/// ingester produces (see `postgis-shim-interface/src/bin/
/// sql_extension_catalog_emit/db_ingest.rs::ScalarRow`). Every geometry /
/// geography / raster / topogeometry type upstream is collapsed to
/// `"binary"` (WKB / serialised wire form); every numeric width
/// collapses to `int64` or `float64`. Unknown tokens fall back to
/// `Blob` — matches the WacPlug convention and keeps a codegen run
/// against a hypothetical future token from lockstep.
fn token_to_logicaltype_lit(token: &str) -> &'static str {
    match token {
        "boolean" => "Logicaltype::Boolean",
        "int64" => "Logicaltype::Int64",
        "float64" => "Logicaltype::Float64",
        "text" => "Logicaltype::Text",
        "binary" => "Logicaltype::Blob",
        _ => "Logicaltype::Blob",
    }
}

/// Emit a Dynlink-mode duckdb bridge crate under `out_dir`.
///
/// Produced layout:
///
/// ```text
/// Cargo.toml
/// README.md
/// src/lib.rs
/// wit/world.wit
/// wit/deps/compose-dynlink/*.wit   (copied from datalink-dynlink)
/// wit/deps/sys-compose/*.wit
/// wit/deps/duckdb/*.wit            (copied from ~/git/ducklink/wit/duckdb-extension)
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

    // Load per-fn signatures from the sibling shim-interface DB, if
    // provided. Every scalar name the catalog surfaces MUST have a
    // matching row in the sqlite for the emit to declare the right
    // arity + arg logicaltypes (postgis: 1218/1218 today, verified
    // via `SELECT COUNT(*) FROM scalars WHERE extension='postgis'`).
    // When absent, we fall back to the arity-1 opaque-Blob shape so a
    // codegen run without `--interface` still produces a valid crate.
    let sig_map = match opts.interface_sqlite.as_deref() {
        Some(p) => load_scalar_sigs(p, &catalog.meta.extension).with_context(|| {
            format!(
                "loading scalar signatures from shim-interface: {}",
                p.display()
            )
        })?,
        None => {
            eprintln!(
                "[duckdb-dynlink-emit] warning: no --interface sqlite provided; \
                 emitting arity-1 Blob shape for every scalar (Phase A fallback)."
            );
            HashMap::new()
        }
    };
    // Phase 9.3.next: aggregate + table-function signatures. Missing
    // rows fall back to arity-1 Blob (mirrors scalar behaviour).
    let agg_sig_map = match opts.interface_sqlite.as_deref() {
        Some(p) => load_aggregate_sigs(p, &catalog.meta.extension).with_context(|| {
            format!(
                "loading aggregate signatures from shim-interface: {}",
                p.display()
            )
        })?,
        None => HashMap::new(),
    };
    let table_sig_map = match opts.interface_sqlite.as_deref() {
        Some(p) => load_table_sigs(p, &catalog.meta.extension).with_context(|| {
            format!(
                "loading table_function signatures from shim-interface: {}",
                p.display()
            )
        })?,
        None => HashMap::new(),
    };

    fs::write(out_dir.join("Cargo.toml"), cargo_toml(&crate_name, &version))?;
    fs::write(out_dir.join("wit/world.wit"), world_wit(&opts.sub_ext))?;
    populate_deps(&out_dir.join("wit/deps"))?;

    // Build the scalar alias→canonical map from `catalog.aliases`.
    // The catalog carries name-mangling aliases (e.g.
    // `st_geomfromtext` → `st_geom_from_text`) as first-class
    // `[[aliases]]` entries so both SQL spellings resolve to the
    // same provider WIT method. The bridge exposes both forms to
    // DuckDB (they're both in `leaf.scalars`), but the provider only
    // matches the WIT-canonical (long) form. Without translation, a
    // call to `st_geomfromtext(...)` reaches the provider as method
    // `st-geomfromtext` and fails with `unknown method`.
    // Scalar aliases feed the `canonical_for` translation the bridge
    // uses to rewrite compact SQL names into the provider's canonical
    // WIT method name. Aggregate + table aliases funnel through the
    // same helper (see `canonical_for` at emit time) so they're
    // collected together — a bridge that only ships scalars simply
    // has no aggregate/table aliases to include. Guarded downstream
    // by "arm registered on this bridge?" so an alias for a name that
    // doesn't reach the dispatch set contributes zero dead arms.
    let scalar_aliases: Vec<(String, String)> = catalog
        .aliases
        .iter()
        .filter(|a| {
            a.kind == "scalar"
                || a.kind == "aggregate"
                || a.kind == "table"
                || a.kind == "table_function"
                || a.kind == "window_function"
        })
        .map(|a| (a.alias.clone(), a.canonical.clone()))
        .collect();

    // #64 / #67: primary spatial logical types the bridge should
    // announce to DuckDB. The catalog carries every logical type
    // (`[[types]] kind = "logical"`) but only the top-level
    // containers (GEOMETRY / GEOGRAPHY / RASTER / TOPOLOGY) need
    // paired BLOB casts — element types like `point` / `polygon`
    // never appear as first-class SQL column types on the wire.
    // Names are uppercased to match the SQL parser's binder
    // (`::GEOMETRY`, `::GEOGRAPHY`) and the sibling
    // `datalink-shim-duckdb-emit::register::render_logical_types`
    // convention.
    let primary_type_names: std::collections::BTreeSet<&str> =
        ["geometry", "geography", "raster", "topology", "topogeometry"]
            .into_iter()
            .collect();
    let types_present = catalog.types_for(&leaves);
    let mut logical_types: Vec<String> = types_present
        .iter()
        .filter(|t| t.kind == "logical")
        .filter(|t| primary_type_names.contains(t.name.as_str()))
        .map(|t| t.name.to_uppercase())
        .collect();
    // #79-followup: box2d / box3d are PostGIS BLOB-backed bounding-
    // box types. The catalog doesn't (yet) enumerate them as
    // `[[types]] kind = "logical"`, but the shim exposes bbox-taking
    // scalars (`box2d(GEOMETRY)`, `st_astext(box2d(...))`, etc.) so
    // the DuckDB binder needs a resolvable type entry. Inject them
    // unconditionally when GEOMETRY is present — they always ride
    // with the geometry surface. Mirrors the sibling
    // `datalink-shim-duckdb-emit::register::render_logical_types`
    // static-emit path (has_geometry branch adds BOX2D/BOX3D).
    if logical_types.iter().any(|n| n == "GEOMETRY") {
        logical_types.push("BOX2D".into());
        logical_types.push("BOX3D".into());
    }
    logical_types.sort();
    logical_types.dedup();

    let lib_src = lib_rs(
        &opts.provider_id,
        &opts.extension_root,
        &catalog.meta.extension,
        &version,
        functions.iter().collect::<Vec<_>>().as_slice(),
        &sig_map,
        &agg_sig_map,
        &table_sig_map,
        &scalar_aliases,
        &logical_types,
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
    format!("{sub}-duckdb-bridge-dynlink")
}

fn cargo_toml(crate_name: &str, version: &str) -> String {
    format!(
        r#"[package]
name = "{crate_name}"
version = "{version}"
edition = "2021"
description = "Phase A dynlink-mode duckdb bridge — routes SQL dispatch through compose:dynlink/linker against a resident provider."
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
opt-level = "z"
lto = true
codegen-units = 1
strip = true
"#,
    )
}

/// Convert a snake_case sub-ext name into a kebab-case WIT package
/// segment that satisfies the component-model rule "each `-`-separated
/// segment must start with `[a-z]`". Underscores become dashes; digit-
/// starting segments (`3d`, `2d`, `4d`) get word-form treatment
/// (`3d` → `threed`, etc.) so `postgis_3d` yields `postgis-threed`
/// not `postgis-3d` (which wit-bindgen rejects).
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

fn world_wit(sub_ext: &str) -> String {
    let pkg = kebab_safe_pkg_name(sub_ext);
    format!(
        r#"package duckdb-bridge:{pkg}@0.1.0;

/// Phase A dynlink-mode duckdb bridge.
///
/// The bridge imports `compose:dynlink/linker` for outbound
/// dispatch to a resident provider and exports the canonical
/// duckdb:extension@4.0.0 guest + callback-dispatch pair (the
/// same shape every DuckDB extension component declares). Only
/// `callback-dispatch::call-scalar` is wired to the provider at
/// Phase A scope; every other method returns
/// `duckerror::unsupported` so the composite world instantiates
/// against `duckdb-loader` without a missing-export failure.
world bridge {{
    import compose:dynlink/linker@0.1.0;

    // Minimal contract-side imports — the guest needs `runtime`
    // to register its scalars during `load`. The default path uses
    // `runtime.scalar-registry.register` with per-fn arity + per-arg
    // logicaltypes derived from the sibling shim-interface DB (see
    // `emit_dynlink::load_scalar_sigs`). `runtime-ext` stays imported
    // for a future variadic opt-in surface (`register-scalar-ex`);
    // the emitted lib.rs currently doesn't call it at the default
    // register site, but the import is retained so a Phase B commit
    // can wire per-arm variadic registrations without a WIT churn.
    import duckdb:extension/runtime@4.0.0;
    import duckdb:extension/runtime-ext@4.0.0;
    import duckdb:extension/logging@4.0.0;

    // #64 / #67: `catalog.register-logical-type` + `register-cast`
    // are how the bridge announces spatial types (GEOMETRY /
    // GEOGRAPHY / RASTER / TOPOLOGY) to the DuckDB binder so
    // callers can write `'x'::GEOGRAPHY` and typed scalars can
    // overload-resolve against BLOB-signature registrations.
    import duckdb:extension/catalog@4.0.0;

    export duckdb:extension/guest@4.0.0;
    export duckdb:extension/callback-dispatch@4.0.0;
}}
"#,
    )
}

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

    // duckdb:extension package. Every .wit file under
    // ~/git/ducklink/wit/duckdb-extension/ carries the same
    // `package duckdb:extension@4.0.0;` header; copying the
    // interface files preserves that. The `worlds/` subdirectory
    // is skipped — the bridge synthesises its own world.
    let duckdb_from = std::env::var("DUCKLINK_WIT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("git/ducklink/wit/duckdb-extension")
        });
    if !duckdb_from.is_dir() {
        return Err(anyhow!(
            "duckdb:extension WIT source missing: {} (set DUCKLINK_WIT)",
            duckdb_from.display()
        ));
    }
    let duckdb_dst = deps_dir.join("duckdb");
    fs::create_dir_all(&duckdb_dst)?;
    for entry in fs::read_dir(&duckdb_from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src = entry.path();
        if ty.is_file() {
            let dst = duckdb_dst.join(entry.file_name());
            copy_kebab_fixed(&src, &dst)?;
        }
        // Skip the `worlds/` subdirectory; the bridge world lives
        // at wit/world.wit and is synthesised above.
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
         Phase A dynlink-mode duckdb bridge for `{sub_ext}` (target `{target}`).\n\
         \n\
         The bridge imports only `compose:dynlink/linker` and dispatches SQL\n\
         scalar arms as CBOR envelopes through the resident provider\n\
         `{provider_id}`. Aggregate / cast / table / pragma / column paths\n\
         return `duckerror::unsupported` at Phase A scope.\n"
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
    sig_map: &HashMap<String, ScalarSig>,
    agg_sig_map: &HashMap<String, AggregateSig>,
    table_sig_map: &HashMap<String, TableSig>,
    scalar_aliases: &[(String, String)],
    logical_types: &[String],
) -> String {
    let mut scalar_names: Vec<&str> = functions
        .iter()
        .filter(|(k, _)| *k == FnKind::Scalar)
        .map(|(_, n)| n.as_str())
        .collect();
    scalar_names.sort();
    scalar_names.dedup();

    // Phase 9.3.next: enumerate aggregates, windows, and table
    // functions from the catalog dispatch set. Aggregates and
    // windows share the runtime `aggregate-registry` capability
    // (DuckDB doesn't have a separate window-registry — the engine
    // treats window = aggregate + frame); table functions land in
    // the `table-registry` capability.
    // Window-only functions are lifted from the `window_functions`
    // table into the aggregate list too — DuckDB's window path is
    // aggregate + PARTITION/ORDER frame, so they SHOULD register the
    // same way. In practice, window-only postgis clustering fns
    // (st_cluster_dbscan, st_cluster_kmeans, ...) trap the guest at
    // WindowAggregateExecutor::Sink time — the compose:dynlink
    // dispatch shape doesn't match what a window-frame call expects
    // (per-row result stream vs one-shot aggregate).
    //
    // Detect and drop them: functions present in `window_functions`
    // BUT NOT in `aggregates` at the catalog level are window-only
    // and skipping keeps LOAD-time registration clean + prevents the
    // downstream guest trap. True aggregate/window duals (same name
    // in both tables — infrequent) still register under aggregate
    // semantics.
    let aggregate_only: std::collections::BTreeSet<&str> = functions
        .iter()
        .filter(|(k, _)| *k == FnKind::Aggregate)
        .map(|(_, n)| n.as_str())
        .collect();
    let mut aggregate_names: Vec<&str> = functions
        .iter()
        .filter(|(k, n)| {
            (*k == FnKind::Aggregate)
                || (*k == FnKind::Window && aggregate_only.contains(n.as_str()))
        })
        .map(|(_, n)| n.as_str())
        .collect();
    aggregate_names.sort();
    aggregate_names.dedup();
    let mut window_only_skipped = 0usize;
    for (kind, name) in functions.iter() {
        if *kind == FnKind::Window && !aggregate_only.contains(name.as_str()) {
            window_only_skipped += 1;
        }
    }
    if window_only_skipped > 0 {
        eprintln!(
            "[duckdb-dynlink-emit] note: {window_only_skipped} window-only function(s) \
             skipped from aggregate registration (compose:dynlink dispatch doesn't yet \
             support DuckDB's per-row window frame semantics — traps the guest at \
             WindowAggregateExecutor::Sink). Aggregate-form duals still register."
        );
    }
    let mut table_names: Vec<&str> = functions
        .iter()
        .filter(|(k, _)| *k == FnKind::Table)
        .map(|(_, n)| n.as_str())
        .collect();
    table_names.sort();
    table_names.dedup();

    // Emit the compact→canonical alias table as a `match` arm body
    // consumed by the `canonical_for` helper below. Only aliases
    // that appear as scalars in this bridge's dispatch set contribute
    // an arm — if the alias isn't a name we register with DuckDB,
    // there's no dispatch site that could reach it and the arm would
    // be dead code. Aliases whose canonical form is missing from the
    // dispatch set are dropped too (there's no arm to route to).
    let scalar_name_set: std::collections::BTreeSet<&str> =
        scalar_names.iter().copied().collect();
    let aggregate_name_set: std::collections::BTreeSet<&str> =
        aggregate_names.iter().copied().collect();
    let table_name_set: std::collections::BTreeSet<&str> =
        table_names.iter().copied().collect();
    let mut alias_arms = String::new();
    // Aliases across scalar / aggregate / table kinds fold into one
    // `canonical_for` match: DuckDB dispatches on bare name and the
    // bridge translates to the provider's canonical WIT method name.
    // A given alias is retained iff BOTH sides register with DuckDB
    // (else the arm is unreachable). Cross-kind mapping is fine —
    // e.g. `st_dumppoints` (table) → `st_dump_points` (table).
    for (alias, canonical) in scalar_aliases {
        let alias_registered = scalar_name_set.contains(alias.as_str())
            || aggregate_name_set.contains(alias.as_str())
            || table_name_set.contains(alias.as_str());
        let canonical_registered = scalar_name_set.contains(canonical.as_str())
            || aggregate_name_set.contains(canonical.as_str())
            || table_name_set.contains(canonical.as_str());
        if alias_registered && canonical_registered {
            let a = alias.replace('"', "\\\"");
            let c = canonical.replace('"', "\\\"");
            alias_arms.push_str(&format!("        \"{a}\" => \"{c}\",\n"));
        }
    }

    // Build the arm_idx ↔ name lookup. `arm_idx` starts at 0
    // and is dense over the sorted scalar name set; the runtime
    // handle allocated by `NEXT_HANDLE.fetch_add(1)` at register
    // time is inserted into `handle_table` keyed by handle →
    // arm_idx. `scalar_name_by_arm_idx(arm_idx)` maps back to the
    // provider method name.
    let mut scalar_name_arms = String::new();
    let mut scalar_register_calls = String::new();
    let mut missing_sig_count = 0usize;
    for (idx, name) in scalar_names.iter().enumerate() {
        let arm_idx = idx as u32;
        let escaped = name.replace('"', "\\\"");
        scalar_name_arms.push_str(&format!(
            "        {arm_idx} => Some(\"{escaped}\"),\n"
        ));

        // Per-fn arg list: look up the sqlite signature, map each
        // token to a `Logicaltype` variant. When the DB is absent or
        // a name is missing, fall back to arity-1 Blob (a single
        // opaque arg) so the emit still produces a valid crate — the
        // provider dispatch keys on name, so the arg-count mismatch
        // is a compile-time-registration issue, not a call-time
        // marshalling one.
        let (args_expr, return_lit) = if let Some(sig) = sig_map.get(*name) {
            let mut args = String::from("vec![\n");
            for (i, tok) in sig.param_tokens.iter().enumerate() {
                let lit = token_to_logicaltype_lit(tok);
                args.push_str(&format!(
                    "                runtime::Funcarg {{ name: Some(\"arg{i}\".into()), logical: {lit} }},\n"
                ));
            }
            args.push_str("            ]");
            let ret_lit = token_to_logicaltype_lit(&sig.return_token);
            (args, ret_lit)
        } else {
            missing_sig_count += 1;
            (
                String::from(
                    "vec![\n                runtime::Funcarg { name: Some(\"arg0\".into()), logical: Logicaltype::Blob },\n            ]",
                ),
                "Logicaltype::Blob",
            )
        };

        // Base scalar-registry.register path (as opposed to
        // `runtime-ext.register-scalar-ex`, which is the additive
        // 2.2.0 variadic surface). The `runtime-ext` import is kept
        // in world.wit as a future opt-in when a specific arm needs
        // varargs; the default emit uses the base register call so
        // DuckDB binds the SQL at parse time against the correct
        // arity + arg logicaltypes.
        scalar_register_calls.push_str(&format!(
            r#"    {{
        let handle = NEXT_HANDLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        handle_table()
            .lock()
            .expect("scalar handle mutex poisoned")
            .insert(handle, {arm_idx}usize);
        let callback = runtime::ScalarCallback::new(handle);
        let args: Vec<runtime::Funcarg> = {args_expr};
        let opts = runtime::Funcopts {{
            description: Some("{escaped} (sqlink-shim-codegen --dynlink)".into()),
            tags: vec!["{escaped}".into()],
            attributes: Funcflags::DETERMINISTIC | Funcflags::STATELESS,
        }};
        registry.register(
            "{escaped}",
            &args,
            &{return_lit},
            callback,
            Some(&opts),
        )?;
    }}
"#,
        ));
    }
    if missing_sig_count > 0 {
        eprintln!(
            "[duckdb-dynlink-emit] warning: {missing_sig_count} scalar(s) had no shim-interface \
             signature (arity-1 Blob fallback used). If `--interface` was passed, this indicates \
             the catalog and DB are out of sync."
        );
    }

    // ── Aggregate + window emission (Phase 9.3.next) ──
    //
    // Windows share the aggregate-registry surface with `is_window`
    // implicit in the callback shape — DuckDB's engine handles the
    // OVER clause by re-dispatching the same registered aggregate
    // over the frame. Both kinds are folded into the same handle
    // table + arm-idx map (`aggregate_name_by_arm_idx`) so a single
    // `call_aggregate_col` dispatch arm handles both surfaces.
    let mut aggregate_name_arms = String::new();
    let mut aggregate_register_calls = String::new();
    let mut missing_agg_sig_count = 0usize;
    let mut skipped_agg_collision_count = 0usize;
    for (idx, name) in aggregate_names.iter().enumerate() {
        let arm_idx = idx as u32;
        let escaped = name.replace('"', "\\\"");
        aggregate_name_arms.push_str(&format!(
            "        {arm_idx} => Some(\"{escaped}\"),\n"
        ));
        // Skip aggregate registration when the same name is ALSO a
        // scalar — DuckDB rejects a duplicate registration under the
        // same name and the whole aggregate register call fails at
        // load time. `st_makeline`, `st_extent`, etc. are dual-role
        // PostGIS names (scalar for `st_makeline(g1, g2)`, aggregate
        // for `st_makeline(g) OVER (...)`) that both live in the
        // interface DB. The scalar form wins for `SELECT
        // st_makeline(...) FROM t` grammatically; the aggregate can
        // still be invoked through its `-agg` / `-aggregate` alias
        // (which the DB carries as a separate name — e.g. `st_makelineagg`).
        //
        // Emit the arm-idx name mapping either way so
        // `aggregate_name_by_arm_idx` stays consistent with the arm
        // count that the runtime handle table indexes.
        if scalar_name_set.contains(*name) {
            skipped_agg_collision_count += 1;
            continue;
        }
        let args_expr = if let Some(sig) = agg_sig_map.get(*name) {
            let mut args = String::from("vec![\n");
            for (i, tok) in sig.param_tokens.iter().enumerate() {
                let lit = token_to_logicaltype_lit(tok);
                args.push_str(&format!(
                    "                runtime::Funcarg {{ name: Some(\"arg{i}\".into()), logical: {lit} }},\n"
                ));
            }
            args.push_str("            ]");
            args
        } else {
            missing_agg_sig_count += 1;
            String::from(
                "vec![\n                runtime::Funcarg { name: Some(\"arg0\".into()), logical: Logicaltype::Blob },\n            ]",
            )
        };
        aggregate_register_calls.push_str(&format!(
            r#"    {{
        let handle = NEXT_HANDLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        aggregate_handle_table()
            .lock()
            .expect("aggregate handle mutex poisoned")
            .insert(handle, {arm_idx}usize);
        let callback = runtime::AggregateCallback::new(handle);
        let args: Vec<runtime::Funcarg> = {args_expr};
        let opts = runtime::Funcopts {{
            description: Some("{escaped} (sqlink-shim-codegen --dynlink)".into()),
            tags: vec!["{escaped}".into()],
            attributes: Funcflags::DETERMINISTIC | Funcflags::STATELESS,
        }};
        registry.register(
            "{escaped}",
            &args,
            &Logicaltype::Blob,
            callback,
            Some(&opts),
        )?;
    }}
"#,
        ));
    }
    if missing_agg_sig_count > 0 {
        eprintln!(
            "[duckdb-dynlink-emit] warning: {missing_agg_sig_count} aggregate(s) had no \
             shim-interface signature (arity-1 Blob fallback used)."
        );
    }
    if skipped_agg_collision_count > 0 {
        eprintln!(
            "[duckdb-dynlink-emit] note: {skipped_agg_collision_count} aggregate(s) skipped \
             because a scalar with the same name is already registered (DuckDB rejects \
             duplicate function names). Use the aggregate's dedicated `-agg` / `-aggregate` \
             variant to invoke the aggregate form."
        );
    }

    // ── Table-function emission (Phase 9.3.next) ──
    //
    // Each table-function name registers via `runtime.table-registry`
    // with a single BLOB output column; `call_table` invokes the
    // provider through the CBOR envelope and unwraps a List-of-Bytes
    // response into a per-row Resultset.
    let mut table_name_arms = String::new();
    let mut table_register_calls = String::new();
    let mut missing_table_sig_count = 0usize;
    for (idx, name) in table_names.iter().enumerate() {
        let arm_idx = idx as u32;
        let escaped = name.replace('"', "\\\"");
        table_name_arms.push_str(&format!(
            "        {arm_idx} => Some(\"{escaped}\"),\n"
        ));
        let args_expr = if let Some(sig) = table_sig_map.get(*name) {
            let mut args = String::from("vec![\n");
            for (i, tok) in sig.param_tokens.iter().enumerate() {
                let lit = token_to_logicaltype_lit(tok);
                args.push_str(&format!(
                    "                runtime::Funcarg {{ name: Some(\"arg{i}\".into()), logical: {lit} }},\n"
                ));
            }
            args.push_str("            ]");
            args
        } else {
            missing_table_sig_count += 1;
            String::from(
                "vec![\n                runtime::Funcarg { name: Some(\"arg0\".into()), logical: Logicaltype::Blob },\n            ]",
            )
        };
        table_register_calls.push_str(&format!(
            r#"    {{
        let handle = NEXT_HANDLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        table_handle_table()
            .lock()
            .expect("table handle mutex poisoned")
            .insert(handle, {arm_idx}usize);
        let callback = runtime::TableCallback::new(handle);
        let args: Vec<runtime::Funcarg> = {args_expr};
        let columns: Vec<runtime::Columndef> = vec![runtime::Columndef {{
            name: "result".into(),
            logical: Logicaltype::Blob,
        }}];
        registry.register(
            "{escaped}",
            &args,
            &columns,
            callback,
            None,
        )?;
    }}
"#,
        ));
    }
    if missing_table_sig_count > 0 {
        eprintln!(
            "[duckdb-dynlink-emit] warning: {missing_table_sig_count} table_function(s) had no \
             shim-interface signature (arity-1 Blob fallback used)."
        );
    }

    // Conditional emit of the register + dispatch machinery. When
    // there are no aggregates / no tables in this bridge, we still
    // emit the (empty) helper fns so the guest.load() call site
    // stays uniform.
    let _has_aggs = !aggregate_names.is_empty();
    let _has_tables = !table_names.is_empty();
    let agg_names_nonempty = if aggregate_names.is_empty() { "false" } else { "true" };
    let table_names_nonempty = if table_names.is_empty() { "false" } else { "true" };
    let agg_count = aggregate_names.len();
    let table_count = table_names.len();

    let extension_root = extension_root.to_string();
    let catalog_extension = catalog_extension.to_string();

    // #64 / #67: build the `register_logical_types()` fn body + the
    // paired identity-cast machinery. When the catalog declares no
    // primary spatial type, `logical_types` is empty and every one
    // of these expands to `""` — the emitted lib.rs then mirrors
    // the pre-fix behaviour (no logical-type call, `call_cast`
    // stays Unsupported).
    // #79-followup (postgis-shim BLOB↔GEOMETRY ambiguity):
    // DuckDB 1.5+ natively provides `GEOMETRY`, `GEOGRAPHY`, `BOX2D`,
    // and `BOX3D` types plus a family of `st_*(GEOMETRY) -> …` scalar
    // overloads. The shim registers its own overloads with `BLOB`
    // parameters (WKB carriers).
    //
    // Registering the shim's logical-type aliases via `CREATE TYPE
    // <NAME> AS BLOB` (the mechanism `catalog::register_logical_type`
    // uses on the DuckDB-wasm target) causes DuckDB to auto-add
    // implicit BLOB↔<NAME> casts with equal cost to the direct
    // BLOB→BLOB match. That makes the binder unable to disambiguate
    // `st_astext(BLOB)` against `st_astext(GEOMETRY)` — every call
    // becomes an ambiguous "no function matches" error at bind time,
    // even for scalars whose only registered overload is `(BLOB)`.
    // The identity casts we then register (Implicit or Explicit) do
    // not help — the CREATE TYPE side of the alias registration is
    // itself the source of the ambiguity.
    //
    // Fix (minimal, tractable within the surface budget): skip the
    // shim's own logical-type + identity-cast registration entirely.
    // The dispatch side keeps working:
    //
    //   * `SELECT ST_AsText('POINT(1 2)'::GEOMETRY)` — DuckDB's
    //     native GEOMETRY handles the `expr::GEOMETRY` surface and
    //     the native `st_astext(GEOMETRY) -> VARCHAR` returns the
    //     right value.
    //   * `SELECT ST_AsText(st_geomfromtext('POINT(1 2)'))` — the
    //     shim's `st_geomfromtext(TEXT) -> BLOB` returns BLOB and
    //     the shim's `st_astext(BLOB) -> VARCHAR` binds directly
    //     (no ambiguity: the shim's BLOB alias family is unregistered
    //     so nothing competes with a plain BLOB → BLOB match).
    //   * `SELECT ST_MakePoint(1.0, 2.0)` — the shim's
    //     `st_makepoint(FLOAT64, FLOAT64) -> BLOB` binds directly.
    //
    // Known regression (accepted trade-off): SQL that uses
    // shim-specific spatial-type aliases (`::BOX2D`, `::TOPOGEOMETRY`,
    // `::RASTER`) fails to resolve the type name because we no longer
    // register those aliases. This costs ~16 test-cases in the
    // spatial-catalog smoke suite. The blocking 218-case
    // binder_error_no_function_matches_overload cluster (the entire
    // shim's scalar surface) is unblocked, so net verified count is
    // strictly higher than the pre-fix state.
    //
    // Follow-up: a proper fix registers each alias with a physical
    // type OTHER than BLOB (or gates registration behind a probe of
    // DuckDB's own type catalog) so CREATE TYPE doesn't spawn
    // implicit BLOB↔<alias> casts. Requires either a new WIT surface
    // in `catalog.wit` or a physical-type registry on the runtime
    // side.
    // Emit best-effort explicit casts for the type-aliases we would
    // have registered. We deliberately do NOT call
    // `catalog::register_logical_type` here — DuckDB's CREATE TYPE
    // machinery installs its own implicit BLOB↔<alias> casts that
    // collide with the shim's BLOB overloads at bind time (see the
    // long comment above). Registering only the casts skips the
    // CREATE TYPE step; when the target type already exists (DuckDB
    // 1.5+ ships native GEOMETRY / GEOGRAPHY / BOX2D / BOX3D) the
    // cast resolves against the native type and `::GEOMETRY` /
    // `::BOX2D` / etc. from a shim-returned BLOB works via the
    // identity trampoline. When the target type is unknown to
    // DuckDB (RASTER, TOPOGEOMETRY on stock DuckDB) the register_cast
    // call fails; we log and continue.
    let has_logical_types = !logical_types.is_empty();
    let (
        logical_types_fn,
        identity_cast_helpers,
        logical_types_load_call,
        call_cast_body,
    ) = if has_logical_types {
        let mut body = String::from(
            "\nfn register_logical_types() -> Result<(), Duckerror> {\n",
        );
        for name in logical_types {
            let esc = name.replace('"', "\\\"");
            body.push_str(&format!(
                "    // {esc} — cast-only registration (see emit_dynlink.rs comment).\n"
            ));
            for (from_ty, to_ty) in [
                (esc.clone(), "BLOB".to_string()),
                ("BLOB".to_string(), esc.clone()),
            ] {
                body.push_str(&format!(
                    "    {{\n\
                     \x20       let handle = NEXT_HANDLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);\n\
                     \x20       identity_cast_handles().lock().expect(\"identity cast handles mutex poisoned\").insert(handle);\n\
                     \x20       let callback = runtime::CastCallback::new(handle);\n\
                     \x20       let spec = catalog::CastSpec {{ from: \"{from_ty}\".into(), to: \"{to_ty}\".into(), kind: catalog::CastKind::Explicit }};\n\
                     \x20       if let Err(e) = catalog::register_cast(&spec, callback) {{\n\
                     \x20           eprintln!(\"[dynlink-emit] register-cast({from_ty} -> {to_ty}) skipped: {{}}\", e);\n\
                     \x20       }}\n\
                     \x20   }}\n",
                ));
            }
        }
        body.push_str("    Ok(())\n}\n");

        let helpers = String::from(
            "\n\
             fn identity_cast_handles() -> &'static std::sync::Mutex<std::collections::HashSet<u32>> {\n\
             \x20   static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<u32>>> = std::sync::OnceLock::new();\n\
             \x20   T.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))\n\
             }\n",
        );

        let load_call = String::from(
            "        // Best-effort: a cast-registration failure is\n\
             \x20       // non-fatal (typically means the target alias\n\
             \x20       // is unknown to DuckDB stock, e.g. RASTER).\n\
             \x20       let _ = register_logical_types();\n",
        );

        let cast_body = String::from(
            "        if identity_cast_handles()\n\
             \x20           .lock()\n\
             \x20           .expect(\"identity cast handles mutex poisoned\")\n\
             \x20           .contains(&handle)\n\
             \x20       {\n\
             \x20           return Ok(value);\n\
             \x20       }\n\
             \x20       Err(Duckerror::Internal(\"call_cast: handle not identity (Phase A)\".to_string()))\n",
        );

        (body, helpers, load_call, cast_body)
    } else {
        // Even when no logical types are advertised, `call_cast_col`
        // is emitted unconditionally and references
        // `identity_cast_handles()`. Keep the helper defined (as an
        // empty set) so the reference compiles; the set stays empty
        // so `contains(&handle)` always returns false and we fall
        // through to the "unsupported" error path.
        let helpers = String::from(
            "\n\
             fn identity_cast_handles() -> &'static std::sync::Mutex<std::collections::HashSet<u32>> {\n\
             \x20   static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<u32>>> = std::sync::OnceLock::new();\n\
             \x20   T.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))\n\
             }\n",
        );
        (
            String::new(),
            helpers,
            String::new(),
            String::from(
                "        let _ = (handle, value);\n\
                 \x20       Err(Duckerror::Internal(\"call_cast: unsupported (Phase A)\".to_string()))\n",
            ),
        )
    };

    format!(
        r##"//! Auto-generated by `datalink_shim_duckdb_dynlink_emit::emit_dynlink`
//! (Phase A, opaque-blob scalar dispatch). Do NOT edit by hand — regenerate.
#![allow(unused_imports, dead_code)]
#![allow(unsafe_op_in_unsafe_fn)]

use std::sync::atomic::AtomicU32;

mod bindings {{
    wit_bindgen::generate!({{
        path: "wit",
        world: "bridge",
        generate_all,
    }});
}}

use bindings::duckdb::extension::types::{{
    Capabilitykind, Duckerror, Duckvalue, Funcflags, Invokeinfo, Logicaltype, Resultset,
}};
use bindings::duckdb::extension::runtime;
use bindings::duckdb::extension::runtime_ext;
use bindings::duckdb::extension::catalog;
use bindings::exports::duckdb::extension::guest::{{self as guest_export, Guest as GuestGuest, Loadresult}};
use bindings::exports::duckdb::extension::callback_dispatch::{{
    self as cb_export, Guest as CallbackGuest,
}};

use bindings::compose::dynlink::linker;

const PROVIDER_ID: &str = "{provider_id}";
const EXTENSION_ROOT: &str = "{extension_root}";
const CATALOG_EXTENSION: &str = "{catalog_extension}";
const CATALOG_VERSION: &str = "{version}";

fn resolve() -> Result<linker::Instance, Duckerror> {{
    linker::resolve_by_id(&PROVIDER_ID.to_string())
        .map_err(|e| Duckerror::Internal(format!("dynlink resolve('{{}}'): {{:?}}", PROVIDER_ID, e)))
}}

// -----------------------------------------------------------
// CBOR envelope (mirrors provider crate's Request/Response).
// -----------------------------------------------------------

#[derive(Debug, Clone)]
enum CborValue {{
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
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
            CborValue::Uint(u) => s.serialize_u64(*u),
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
    Uint(u64),
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
                f.write_str("a CBOR value")
            }}
            fn visit_unit<E: Error>(self) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Null) }}
            fn visit_none<E: Error>(self) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Null) }}
            fn visit_bool<E: Error>(self, v: bool) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Bool(v)) }}
            fn visit_i64<E: Error>(self, v: i64) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Int(v)) }}
            fn visit_u64<E: Error>(self, v: u64) -> Result<ResponseValue, E> {{
                if v <= i64::MAX as u64 {{ Ok(ResponseValue::Int(v as i64)) }}
                else {{ Ok(ResponseValue::Uint(v)) }}
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
                    "Uint" => ResponseValue::Uint(m.next_value()?),
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

fn encode_request(args: Vec<CborValue>) -> Result<Vec<u8>, Duckerror> {{
    let mut out = Vec::new();
    ciborium::into_writer(&Request {{ version: 1, args }}, &mut out)
        .map_err(|e| Duckerror::Internal(format!("cbor encode: {{}}", e)))?;
    Ok(out)
}}

fn decode_response(bytes: &[u8]) -> Result<Response, Duckerror> {{
    ciborium::from_reader(bytes)
        .map_err(|e| Duckerror::Internal(format!("cbor decode: {{}}", e)))
}}

fn call(method: &str, args: Vec<CborValue>) -> Result<ResponseValue, Duckerror> {{
    let inst = resolve()?;
    let payload = encode_request(args)?;
    let bytes = inst
        .invoke(&method.to_string(), &payload)
        .map_err(|e| Duckerror::Internal(format!("{{}}: invoke: {{:?}}", method, e)))?;
    let resp = decode_response(&bytes)?;
    if let Some(err) = resp.err {{
        return Err(Duckerror::Internal(format!("{{}}: {{}}", method, err)));
    }}
    Ok(resp.ok.unwrap_or(ResponseValue::Null))
}}

// -----------------------------------------------------------
// duckvalue / ResponseValue marshalling.
// -----------------------------------------------------------

fn duckv_to_cbor(v: &Duckvalue) -> CborValue {{
    // Arm names mirror the WIT-bindgen output for the
    // `duckdb:extension@4.0.0/types/duckvalue` variant. See
    // ~/git/ducklink/wit/duckdb-extension/types.wit:97-120.
    match v {{
        Duckvalue::Null => CborValue::Null,
        Duckvalue::Boolean(b) => CborValue::Bool(*b),
        Duckvalue::Int8(i) => CborValue::Int(*i as i64),
        Duckvalue::Int16(i) => CborValue::Int(*i as i64),
        Duckvalue::Int32(i) => CborValue::Int(*i as i64),
        Duckvalue::Int64(i) => CborValue::Int(*i),
        Duckvalue::Uint8(u) => CborValue::Int(*u as i64),
        Duckvalue::Uint16(u) => CborValue::Int(*u as i64),
        Duckvalue::Uint32(u) => CborValue::Int(*u as i64),
        Duckvalue::Uint64(u) => CborValue::Uint(*u),
        Duckvalue::Float32(f) => CborValue::Float(*f as f64),
        Duckvalue::Float64(f) => CborValue::Float(*f),
        Duckvalue::Text(s) => CborValue::Text(s.clone()),
        Duckvalue::Blob(b) => CborValue::Bytes(b.clone()),
        // Date / Time / Timestamp / Timestamptz ferry as int64
        // instants (canonical DuckDB storage).
        Duckvalue::Timestamp(i) => CborValue::Int(*i),
        Duckvalue::Timestamptz(i) => CborValue::Int(*i),
        Duckvalue::Date(i) => CborValue::Int(*i as i64),
        Duckvalue::Time(i) => CborValue::Int(*i),
        // Structured arms (Decimal / Interval / Uuid / Complex)
        // ferry as null at Phase A. The provider owns their
        // encoding when Phase B threads structured logical types.
        Duckvalue::Decimal(_)
        | Duckvalue::Interval(_)
        | Duckvalue::Uuid(_)
        | Duckvalue::Complex(_) => CborValue::Null,
    }}
}}

fn response_to_duckv(v: ResponseValue) -> Duckvalue {{
    match v {{
        ResponseValue::Null => Duckvalue::Null,
        ResponseValue::Bool(b) => Duckvalue::Boolean(b),
        ResponseValue::Int(i) => Duckvalue::Int64(i),
        ResponseValue::Uint(u) => Duckvalue::Uint64(u),
        ResponseValue::Float(f) => Duckvalue::Float64(f),
        ResponseValue::Text(t) => Duckvalue::Text(t),
        ResponseValue::Bytes(b) => Duckvalue::Blob(b),
        ResponseValue::List(_) => Duckvalue::Null,
    }}
}}

// -----------------------------------------------------------
// Columnar hot-path helpers (call_scalar_batch_col).
//
// Simplified vs. the fully-typed postgis-ducklink-bridge
// reference: since dynlink Phase A dispatch is opaque-blob,
// we always lift/lower through Duckvalue::Blob columns. That
// keeps the per-row hot path scalar-Blob-only; multi-shape
// dispatch is a follow-up when the catalog carries logical
// types.
// -----------------------------------------------------------

use bindings::duckdb::extension::column_types;

fn cv_is_valid(validity: &[u8], i: usize) -> bool {{
    if validity.is_empty() {{ return true; }}
    let byte = i / 8;
    let bit = i % 8;
    (validity.get(byte).copied().unwrap_or(0) >> bit) & 1 == 1
}}

fn colvec_get(cv: &column_types::Colvec, i: usize) -> Duckvalue {{
    if !cv_is_valid(cv.validity.as_slice(), i) {{
        return Duckvalue::Null;
    }}
    match &cv.data {{
        column_types::Column::Boolean(xs) => xs.get(i).copied().map(Duckvalue::Boolean).unwrap_or(Duckvalue::Null),
        column_types::Column::Int64(xs) => xs.get(i).copied().map(Duckvalue::Int64).unwrap_or(Duckvalue::Null),
        column_types::Column::Uint64(xs) => xs.get(i).copied().map(Duckvalue::Uint64).unwrap_or(Duckvalue::Null),
        column_types::Column::Float64(xs) => xs.get(i).copied().map(Duckvalue::Float64).unwrap_or(Duckvalue::Null),
        column_types::Column::Int32(xs) => xs.get(i).copied().map(Duckvalue::Int32).unwrap_or(Duckvalue::Null),
        column_types::Column::Int8(xs) => xs.get(i).copied().map(Duckvalue::Int8).unwrap_or(Duckvalue::Null),
        column_types::Column::Int16(xs) => xs.get(i).copied().map(Duckvalue::Int16).unwrap_or(Duckvalue::Null),
        column_types::Column::Uint8(xs) => xs.get(i).copied().map(Duckvalue::Uint8).unwrap_or(Duckvalue::Null),
        column_types::Column::Uint16(xs) => xs.get(i).copied().map(Duckvalue::Uint16).unwrap_or(Duckvalue::Null),
        column_types::Column::Uint32(xs) => xs.get(i).copied().map(Duckvalue::Uint32).unwrap_or(Duckvalue::Null),
        column_types::Column::Float32(xs) => xs.get(i).copied().map(Duckvalue::Float32).unwrap_or(Duckvalue::Null),
        column_types::Column::Timestamp(xs) => xs.get(i).copied().map(Duckvalue::Timestamp).unwrap_or(Duckvalue::Null),
        column_types::Column::Date(xs) => xs.get(i).copied().map(Duckvalue::Date).unwrap_or(Duckvalue::Null),
        column_types::Column::Time(xs) => xs.get(i).copied().map(Duckvalue::Time).unwrap_or(Duckvalue::Null),
        column_types::Column::Timestamptz(xs) => xs.get(i).copied().map(Duckvalue::Timestamptz).unwrap_or(Duckvalue::Null),
        column_types::Column::Text(xs) => xs.get(i).cloned().map(Duckvalue::Text).unwrap_or(Duckvalue::Null),
        column_types::Column::Blob(xs) => xs.get(i).cloned().map(Duckvalue::Blob).unwrap_or(Duckvalue::Null),
        // Decimal / Interval / Uuid / Complex column arms are
        // rendered as Null in the Phase A opaque-blob path.
        _ => Duckvalue::Null,
    }}
}}

fn validate_colvec_rows(args: &[column_types::Colvec]) -> Result<usize, Duckerror> {{
    let n_rows = if args.is_empty() {{ 0 }} else {{ args[0].rows as usize }};
    for (j, cv) in args.iter().enumerate() {{
        if cv.rows as usize != n_rows {{
            return Err(Duckerror::Internal(format!(
                "columnar dispatch: arg-column {{}} has rows={{}} but expected {{}}",
                j, cv.rows, n_rows
            )));
        }}
    }}
    Ok(n_rows)
}}

fn materialize_row(args: &[column_types::Colvec], i: usize, out: &mut Vec<Duckvalue>) {{
    out.clear();
    if out.capacity() < args.len() {{
        out.reserve(args.len() - out.capacity());
    }}
    for cv in args {{
        out.push(colvec_get(cv, i));
    }}
}}

/// Lower a `Vec<Duckvalue>` (per-row scalar returns) into a
/// typed `Colvec`. Uses a TWO-PASS approach so that NULL rows
/// preceding the first non-NULL row still land in the eventual
/// arm's buffer as zero placeholders (single-pass "pick on first
/// non-NULL" left the pre-arm nulls in `blobs`, giving a
/// buffer.len() < rows off-by-one when the picked arm was not
/// Blob — every subsequent lookup was misaligned).
///
/// Pass 1 walks `&values` to determine the arm (and rejects
/// heterogeneous batches / unsupported Decimal/Interval/Uuid/
/// Complex arms). Pass 2 consumes `values` and pushes real
/// values or zero placeholders into that arm's buffer so the
/// buffer index matches the row index; the validity bitmap
/// carries the actual NULL rows. A colvec of all-NULLs is
/// lowered as Blob (chosen arbitrarily: no data buffer is ever
/// addressed since the validity bitmap zeros every row).
fn values_to_colvec(values: Vec<Duckvalue>) -> Result<column_types::Colvec, Duckerror> {{
    let n = values.len();
    let rows = n as u32;
    let mut bits: Vec<u8> = vec![0u8; (n + 7) / 8];
    let mut any_null = false;

    // Discriminator: which arm we picked, populated on first
    // non-NULL row. Repeated per-arm buffers avoid an enum tag
    // in the hot path and let the compiler prove exhaustiveness.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Arm {{
        Unknown,
        Boolean,
        Int8, Int16, Int32, Int64,
        Uint8, Uint16, Uint32, Uint64,
        Float32, Float64,
        Text, Blob,
        Date, Time, Timestamp, Timestamptz,
    }}

    fn arm_of(v: &Duckvalue) -> Option<Arm> {{
        Some(match v {{
            Duckvalue::Boolean(_) => Arm::Boolean,
            Duckvalue::Int8(_) => Arm::Int8,
            Duckvalue::Int16(_) => Arm::Int16,
            Duckvalue::Int32(_) => Arm::Int32,
            Duckvalue::Int64(_) => Arm::Int64,
            Duckvalue::Uint8(_) => Arm::Uint8,
            Duckvalue::Uint16(_) => Arm::Uint16,
            Duckvalue::Uint32(_) => Arm::Uint32,
            Duckvalue::Uint64(_) => Arm::Uint64,
            Duckvalue::Float32(_) => Arm::Float32,
            Duckvalue::Float64(_) => Arm::Float64,
            Duckvalue::Text(_) => Arm::Text,
            Duckvalue::Blob(_) => Arm::Blob,
            Duckvalue::Date(_) => Arm::Date,
            Duckvalue::Time(_) => Arm::Time,
            Duckvalue::Timestamp(_) => Arm::Timestamp,
            Duckvalue::Timestamptz(_) => Arm::Timestamptz,
            Duckvalue::Null
            | Duckvalue::Decimal(_)
            | Duckvalue::Interval(_)
            | Duckvalue::Uuid(_)
            | Duckvalue::Complex(_) => return None,
        }})
    }}

    fn mismatch(expected: &Arm, actual: &Duckvalue) -> Duckerror {{
        Duckerror::Internal(format!(
            "values_to_colvec: column arm mismatch (expected={{:?}}, saw variant with tag {{:?}}); \
             heterogeneous rows in a single columnar batch are unsupported",
            expected, core::mem::discriminant(actual),
        ))
    }}

    // ---- Pass 1: pick the arm across every non-NULL row. ----
    let mut arm = Arm::Unknown;
    for (i, v) in values.iter().enumerate() {{
        if matches!(v, Duckvalue::Null) {{
            any_null = true;
            continue;
        }}
        // Refuse unsupported (Decimal/Interval/Uuid/Complex)
        // returns explicitly — the old code silently NULLed them.
        let this_arm = arm_of(v).ok_or_else(|| Duckerror::Internal(format!(
            "values_to_colvec: unsupported column arm for row {{}} (Decimal/Interval/Uuid/Complex \
             lowering is not yet implemented in the dynlink emit)",
            i,
        )))?;
        if arm == Arm::Unknown {{
            arm = this_arm;
        }} else if arm != this_arm {{
            return Err(mismatch(&arm, v));
        }}
    }}

    // ---- Pass 2: allocate the chosen arm's buffer at exact
    // length and materialize (real values for non-NULL rows,
    // zero placeholders for NULL rows).
    let mut booleans: Vec<bool> = Vec::new();
    let mut int8s: Vec<i8> = Vec::new();
    let mut int16s: Vec<i16> = Vec::new();
    let mut int32s: Vec<i32> = Vec::new();
    let mut int64s: Vec<i64> = Vec::new();
    let mut uint8s: Vec<u8> = Vec::new();
    let mut uint16s: Vec<u16> = Vec::new();
    let mut uint32s: Vec<u32> = Vec::new();
    let mut uint64s: Vec<u64> = Vec::new();
    let mut float32s: Vec<f32> = Vec::new();
    let mut float64s: Vec<f64> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    let mut dates: Vec<i32> = Vec::new();
    let mut times: Vec<i64> = Vec::new();
    let mut timestamps: Vec<i64> = Vec::new();
    let mut timestamptzs: Vec<i64> = Vec::new();

    for (i, v) in values.into_iter().enumerate() {{
        if matches!(v, Duckvalue::Null) {{
            // Placeholder so the buffer index tracks the row
            // index; the validity bit stays 0.
            match arm {{
                Arm::Unknown | Arm::Blob => blobs.push(Vec::new()),
                Arm::Boolean => booleans.push(false),
                Arm::Int8 => int8s.push(0),
                Arm::Int16 => int16s.push(0),
                Arm::Int32 => int32s.push(0),
                Arm::Int64 => int64s.push(0),
                Arm::Uint8 => uint8s.push(0),
                Arm::Uint16 => uint16s.push(0),
                Arm::Uint32 => uint32s.push(0),
                Arm::Uint64 => uint64s.push(0),
                Arm::Float32 => float32s.push(0.0),
                Arm::Float64 => float64s.push(0.0),
                Arm::Text => texts.push(String::new()),
                Arm::Date => dates.push(0),
                Arm::Time => times.push(0),
                Arm::Timestamp => timestamps.push(0),
                Arm::Timestamptz => timestamptzs.push(0),
            }}
            continue;
        }}
        bits[i / 8] |= 1u8 << (i % 8);
        match v {{
            Duckvalue::Boolean(b) => booleans.push(b),
            Duckvalue::Int8(x) => int8s.push(x),
            Duckvalue::Int16(x) => int16s.push(x),
            Duckvalue::Int32(x) => int32s.push(x),
            Duckvalue::Int64(x) => int64s.push(x),
            Duckvalue::Uint8(x) => uint8s.push(x),
            Duckvalue::Uint16(x) => uint16s.push(x),
            Duckvalue::Uint32(x) => uint32s.push(x),
            Duckvalue::Uint64(x) => uint64s.push(x),
            Duckvalue::Float32(x) => float32s.push(x),
            Duckvalue::Float64(x) => float64s.push(x),
            Duckvalue::Text(s) => texts.push(s),
            Duckvalue::Blob(b) => blobs.push(b),
            Duckvalue::Date(x) => dates.push(x),
            Duckvalue::Time(x) => times.push(x),
            Duckvalue::Timestamp(x) => timestamps.push(x),
            Duckvalue::Timestamptz(x) => timestamptzs.push(x),
            // Null / unsupported: already handled above (or
            // rejected in pass 1).
            _ => unreachable!("arm_of admitted a variant we didn't push"),
        }}
    }}
    let validity = if any_null {{ bits }} else {{ Vec::new() }};
    let data = match arm {{
        Arm::Unknown | Arm::Blob => column_types::Column::Blob(blobs),
        Arm::Boolean => column_types::Column::Boolean(booleans),
        Arm::Int8 => column_types::Column::Int8(int8s),
        Arm::Int16 => column_types::Column::Int16(int16s),
        Arm::Int32 => column_types::Column::Int32(int32s),
        Arm::Int64 => column_types::Column::Int64(int64s),
        Arm::Uint8 => column_types::Column::Uint8(uint8s),
        Arm::Uint16 => column_types::Column::Uint16(uint16s),
        Arm::Uint32 => column_types::Column::Uint32(uint32s),
        Arm::Uint64 => column_types::Column::Uint64(uint64s),
        Arm::Float32 => column_types::Column::Float32(float32s),
        Arm::Float64 => column_types::Column::Float64(float64s),
        Arm::Text => column_types::Column::Text(texts),
        Arm::Date => column_types::Column::Date(dates),
        Arm::Time => column_types::Column::Time(times),
        Arm::Timestamp => column_types::Column::Timestamp(timestamps),
        Arm::Timestamptz => column_types::Column::Timestamptz(timestamptzs),
    }};
    Ok(column_types::Colvec {{ rows, validity, data }})
}}

fn scalar_name_by_arm_idx(arm: usize) -> Option<&'static str> {{
    match arm as u32 {{
{scalar_name_arms}        _ => None,
    }}
}}

/// SQL-name aliasing: translate a compact-form SQL name (e.g.
/// `st_geomfromtext`) to its WIT-canonical (long) form
/// (`st_geom_from_text`) before kebab-casing to the provider
/// method name. The catalog carries both spellings as scalars in
/// `leaf.scalars` (both must register with DuckDB so callers can
/// use either), and as first-class `[[aliases]]` entries. The
/// provider dispatch only matches the WIT-canonical form; the
/// bridge's `dispatch_call_scalar` calls `canonical_for(name)` so
/// both spellings route to the same provider arm.
fn canonical_for(name: &str) -> &str {{
    match name {{
{alias_arms}        other => other,
    }}
}}

// ────────────────────────────────────────────────────────────
// Handle table + register block.
//
// Every scalar the catalog names gets exactly one runtime
// handle (allocated by NEXT_HANDLE.fetch_add at register time)
// and one dense arm_idx (assigned by codegen). `handle_table`
// maps handle → arm_idx; the dispatch arms (call_scalar +
// call_scalar_batch_col) look up the arm and delegate to the
// per-arm provider method through the CBOR envelope.
// ────────────────────────────────────────────────────────────

fn handle_table() -> &'static std::sync::Mutex<std::collections::HashMap<u32, usize>> {{
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, usize>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}}

// Phase 9.3.next: aggregate + table dispatch handle tables.
// Kept structurally uniform with the scalar handle_table so a
// future consolidation into a single enum-tagged table is a
// one-liner. Empty when the catalog has no aggregates / tables.
fn aggregate_handle_table() -> &'static std::sync::Mutex<std::collections::HashMap<u32, usize>> {{
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, usize>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}}

fn table_handle_table() -> &'static std::sync::Mutex<std::collections::HashMap<u32, usize>> {{
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, usize>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}}
{identity_cast_helpers}
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);
{logical_types_fn}

fn register_scalars() -> Result<(), Duckerror> {{
    // Acquire the base scalar-registry capability from the host.
    // Fix (#834 followup): the previous implementation used
    // `runtime-ext.register-scalar-ex` with an empty arg list
    // (`args: Vec::new()` + `varargs: Some(&Logicaltype::Blob)`)
    // — variadic-Blob for every scalar. That erased the per-fn
    // arity + arg logicaltypes DuckDB needs at bind time (e.g.
    // `st_distance(GEOMETRY, GEOMETRY) -> DOUBLE` bound as
    // `st_distance(BLOB...) -> BLOB`, so callers threading typed
    // arguments couldn't overload-resolve). The base
    // `runtime.scalar-registry.register` path emits the actual
    // arity + per-arg + return logicaltype from the sibling
    // shim-interface DB (see `emit_dynlink::load_scalar_sigs`).
    let capability = runtime::get_capability(Capabilitykind::Scalar)
        .ok_or_else(|| Duckerror::Internal("host did not expose scalar capability".into()))?;
    let registry = match capability {{
        runtime::Capability::Scalar(r) => r,
        _ => {{
            return Err(Duckerror::Internal(
                "scalar capability returned unexpected variant".into(),
            ));
        }}
    }};
{scalar_register_calls}    Ok(())
}}

fn register_aggregates() -> Result<(), Duckerror> {{
    // Best-effort: hosts that don't expose the aggregate capability
    // still let scalars register (the scalar-only bridges shipped in
    // Phase A). Log-and-continue so the emit stays useful in both
    // shapes.
    let Some(capability) = runtime::get_capability(Capabilitykind::Aggregate) else {{
        if {agg_names_nonempty} {{
            eprintln!("[dynlink-emit] host did not expose aggregate capability; skipping {agg_count} aggregate(s)");
        }}
        return Ok(());
    }};
    let registry = match capability {{
        runtime::Capability::Aggregate(r) => r,
        _ => {{
            return Err(Duckerror::Internal(
                "aggregate capability returned unexpected variant".into(),
            ));
        }}
    }};
{aggregate_register_calls}    Ok(())
}}

fn register_tables() -> Result<(), Duckerror> {{
    let Some(capability) = runtime::get_capability(Capabilitykind::Table) else {{
        if {table_names_nonempty} {{
            eprintln!("[dynlink-emit] host did not expose table capability; skipping {table_count} table-function(s)");
        }}
        return Ok(());
    }};
    let registry = match capability {{
        runtime::Capability::Table(r) => r,
        _ => {{
            return Err(Duckerror::Internal(
                "table capability returned unexpected variant".into(),
            ));
        }}
    }};
{table_register_calls}    Ok(())
}}

// -----------------------------------------------------------
// Guest impls.
// -----------------------------------------------------------

struct Component;

impl GuestGuest for Component {{
    fn load() -> Result<Loadresult, Duckerror> {{
{logical_types_load_call}        register_scalars()?;
        register_aggregates()?;
        register_tables()?;
        Ok(Loadresult {{
            name: EXTENSION_ROOT.to_string(),
            version: Some(CATALOG_VERSION.to_string()),
            requires: vec![Capabilitykind::Scalar],
        }})
    }}
    fn reconfigure(_keys: Vec<String>) -> Result<bool, Duckerror> {{ Ok(false) }}
    fn shutdown() -> Result<bool, Duckerror> {{ Ok(false) }}
}}

fn dispatch_call_scalar(
    handle: u32,
    args: Vec<Duckvalue>,
) -> Result<Duckvalue, Duckerror> {{
    let arm_idx = handle_table()
        .lock()
        .expect("scalar handle mutex poisoned")
        .get(&handle)
        .copied()
        .ok_or_else(|| Duckerror::Internal(format!("unknown scalar handle {{}}", handle)))?;
    let name = scalar_name_by_arm_idx(arm_idx)
        .ok_or_else(|| Duckerror::Internal(format!("unknown scalar arm {{}}", arm_idx)))?;
    // Null-propagation: any NULL argument short-circuits to NULL.
    if args.iter().any(|v| matches!(v, Duckvalue::Null)) {{
        return Ok(Duckvalue::Null);
    }}
    // Alias translation: SQL name → WIT-canonical → provider method
    // (kebab-case). Compact-form SQL names (e.g. `st_geomfromtext`)
    // route through `canonical_for` to their long form
    // (`st_geom_from_text`) before kebab-casing; non-alias names
    // pass through unchanged. See `canonical_for` above.
    let canonical = canonical_for(name);
    let method = canonical.replace('_', "-");
    let cbor_args: Vec<CborValue> = args.iter().map(duckv_to_cbor).collect();
    let resp = call(&method, cbor_args)?;
    Ok(response_to_duckv(resp))
}}

fn aggregate_name_by_arm_idx(arm: usize) -> Option<&'static str> {{
    match arm as u32 {{
{aggregate_name_arms}        _ => None,
    }}
}}

fn table_name_by_arm_idx(arm: usize) -> Option<&'static str> {{
    match arm as u32 {{
{table_name_arms}        _ => None,
    }}
}}

/// Aggregate dispatch (Phase 9.3.next). Whole-group call: the host
/// hands us every row's arg columns; we lift each row to a CBOR list
/// and ship the accumulated list-of-lists to the provider under the
/// aggregate's canonical name. The provider owns the semantic
/// fold (average, extent, cluster, …); the bridge is a CBOR tunnel.
fn dispatch_call_aggregate(
    handle: u32,
    args: Vec<bindings::duckdb::extension::column_types::Colvec>,
) -> Result<Duckvalue, Duckerror> {{
    let arm_idx = aggregate_handle_table()
        .lock()
        .expect("aggregate handle mutex poisoned")
        .get(&handle)
        .copied()
        .ok_or_else(|| Duckerror::Internal(format!("unknown aggregate handle {{}}", handle)))?;
    let name = aggregate_name_by_arm_idx(arm_idx)
        .ok_or_else(|| Duckerror::Internal(format!("unknown aggregate arm {{}}", arm_idx)))?;
    let n_rows = validate_colvec_rows(&args)?;
    let n_args = args.len();
    // Columnar wire shape for aggregate dispatch: each column becomes a
    // separate positional arg to the provider, containing a FLAT list
    // of that column's per-row CBOR values. For a single-arg aggregate
    // like `st_extent(geom)`, the provider sees `args=[List<Bytes>]` —
    // arg[0] is the flat geometry list, arg[0][0] is a Bytes geometry
    // (matches the shim's expected shape per the arg-shape checker's
    // error message on prior versions of this dispatch).
    //
    // Previous wire (broken) was `args=[List<List<CborValue>>]` — one
    // arg containing a list of per-row lists. The shim's arg-type
    // checker on `st-extent` etc. read `arg[0][0]` as List (a row) and
    // failed shape mismatch (expected Bytes for the geometry).
    //
    // SQL-aggregate NULL-row semantics: skip rows where ANY arg is
    // NULL (matches sum() / avg() defaults) — do the skip BEFORE
    // transposing to columns so all columns see the same row subset.
    let mut cols: Vec<Vec<CborValue>> = (0..n_args)
        .map(|_| Vec::with_capacity(n_rows))
        .collect();
    let mut row_buf: Vec<Duckvalue> = Vec::with_capacity(n_args);
    for i in 0..n_rows {{
        materialize_row(&args, i, &mut row_buf);
        if row_buf.iter().any(|v| matches!(v, Duckvalue::Null)) {{
            continue;
        }}
        for (col_idx, v) in row_buf.iter().enumerate() {{
            cols[col_idx].push(duckv_to_cbor(v));
        }}
    }}
    // Empty non-null row set → aggregate is NULL (matches SQL
    // `SELECT SUM(x) FROM t WHERE 1=0` = NULL). Skip the provider
    // call so the shim doesn't need to special-case empty inputs.
    if cols.first().map_or(true, |c| c.is_empty()) {{
        return Ok(Duckvalue::Null);
    }}
    let canonical = canonical_for(name);
    let method = canonical.replace('_', "-");
    let cbor_args: Vec<CborValue> = cols.into_iter().map(CborValue::List).collect();
    let resp = call(&method, cbor_args)?;
    Ok(response_to_duckv(resp))
}}

/// Table-function dispatch (Phase 9.3.next). Single-shot call: the
/// host hands us the argv values; we ship them through the CBOR
/// envelope and expect a List-of-Bytes (one BLOB per row) response.
/// Any other shape is best-effort wrapped as a single Blob row so
/// the provider stays free to evolve its wire without breaking the
/// bridge.
fn dispatch_call_table(
    handle: u32,
    args: Vec<Duckvalue>,
) -> Result<Resultset, Duckerror> {{
    let arm_idx = table_handle_table()
        .lock()
        .expect("table handle mutex poisoned")
        .get(&handle)
        .copied()
        .ok_or_else(|| Duckerror::Internal(format!("unknown table handle {{}}", handle)))?;
    let name = table_name_by_arm_idx(arm_idx)
        .ok_or_else(|| Duckerror::Internal(format!("unknown table arm {{}}", arm_idx)))?;
    let canonical = canonical_for(name);
    let method = canonical.replace('_', "-");
    let cbor_args: Vec<CborValue> = args.iter().map(duckv_to_cbor).collect();
    let resp = call(&method, cbor_args)?;
    let rows: Vec<Vec<Duckvalue>> = match resp {{
        ResponseValue::List(items) => items
            .into_iter()
            .map(|item| match item {{
                ResponseValue::Bytes(b) => vec![Duckvalue::Blob(b)],
                ResponseValue::List(cols) => cols.into_iter().map(response_to_duckv).collect(),
                other => vec![response_to_duckv(other)],
            }})
            .collect(),
        ResponseValue::Null => Vec::new(),
        other => vec![vec![response_to_duckv(other)]],
    }};
    Ok(rows)
}}

impl CallbackGuest for Component {{
    fn call_scalar(
        handle: u32,
        args: Vec<Duckvalue>,
        _ctx: Invokeinfo,
    ) -> Result<Duckvalue, Duckerror> {{
        dispatch_call_scalar(handle, args)
    }}

    fn call_scalar_batch_col(
        handle: u32,
        args: Vec<bindings::duckdb::extension::column_types::Colvec>,
        ctx: Invokeinfo,
    ) -> Result<bindings::duckdb::extension::column_types::Colvec, Duckerror> {{
        // Columnar HOT path: convert per-row, delegate to the cold
        // row-major dispatch, then rebuild a colvec. Mirrors the
        // postgis-ducklink-bridge (~/git/postgis-ducklink-bridge/
        // src/lib.rs:2231-2261) reference discipline.
        let n_rows = validate_colvec_rows(&args)?;
        let n_args = args.len();
        let base = ctx.rowindex.unwrap_or(0);
        let mut out: Vec<Duckvalue> = Vec::with_capacity(n_rows);
        let mut row_buf: Vec<Duckvalue> = Vec::with_capacity(n_args);
        for i in 0..n_rows {{
            materialize_row(&args, i, &mut row_buf);
            let _row_ctx = Invokeinfo {{
                rowindex: Some(base + i as u64),
                iswindow: ctx.iswindow,
            }};
            let row_args = core::mem::take(&mut row_buf);
            out.push(dispatch_call_scalar(handle, row_args)?);
        }}
        values_to_colvec(out)
    }}

    fn call_aggregate_col(
        handle: u32,
        args: Vec<bindings::duckdb::extension::column_types::Colvec>,
    ) -> Result<Duckvalue, Duckerror> {{
        dispatch_call_aggregate(handle, args)
    }}

    fn call_cast_col(
        handle: u32,
        arg: bindings::duckdb::extension::column_types::Colvec,
    ) -> Result<bindings::duckdb::extension::column_types::Colvec, Duckerror> {{
        // Identity casts (GEOMETRY↔BLOB / BOX2D↔BLOB / ...) are
        // physical-representation-preserving: the source and target
        // are both physically BLOB, only the logical tag differs.
        // Return the input column unchanged.
        if identity_cast_handles()
            .lock()
            .expect("identity cast handles mutex poisoned")
            .contains(&handle)
        {{
            return Ok(arg);
        }}
        Err(Duckerror::Internal("call_cast_col: handle not identity (Phase A)".to_string()))
    }}

    fn call_table(handle: u32, args: Vec<Duckvalue>) -> Result<Resultset, Duckerror> {{
        dispatch_call_table(handle, args)
    }}

    fn call_pragma(_handle: u32, _args: Vec<Duckvalue>) -> Result<Option<Duckvalue>, Duckerror> {{
        Err(Duckerror::Internal("call_pragma: unsupported (Phase A)".to_string()))
    }}

    fn call_cast(handle: u32, value: Duckvalue) -> Result<Duckvalue, Duckerror> {{
{call_cast_body}    }}
}}

bindings::export!(Component with_types_in bindings);
"##,
    )
}
