//! Emit `src/lib.rs` for the wasm-component bridge (DuckDB target).
//!
//! Step 4 scalar-first cut. The generated file:
//!   1. `wit_bindgen::generate!` against the DuckDB-extension world
//!      (path: "wit", world: "bridge").
//!   2. `use bindings::duckdb::extension::{types, runtime}` so the
//!      dispatch arms reference `types::Duckvalue` / `runtime::*`
//!      identifiers verbatim.
//!   3. The handle-table block (handle_table::render()).
//!   4. `impl guest::Guest for $Bridge` (lifecycle).
//!   5. `impl callback_dispatch::Guest for $Bridge` with the six
//!      arms (scalar wired, others Unsupported).
//!   6. `register_scalars()` body.
//!   7. `bindings::export!(...)` at file scope.
//!
//! The dispatch is keyed by `usize` arm-index instead of the
//! SQLite emit's `u64 func_id` because DuckDB's host hands the
//! extension a per-registration `u32 handle` instead. We allocate
//! that handle at `register_scalars()` time and store
//! `handle -> arm_index` in `handle_table`.

use anyhow::Result;

use shim_bridge_codegen_core::BridgePlan;
use datalink_shim_codegen_core::interface_db::{self, ListPrimElem, ParamShape, RetShape};
use datalink_shim_codegen_core::record_registry::{self, RecordType};

use crate::dispatch;
use crate::emit_wit;
use crate::handle_table;
use crate::lifecycle;
use crate::register;

/// Generate `src/lib.rs`.
pub fn lib_rs(plan: &BridgePlan, crate_name: &str) -> Result<String> {
    let primary = plan
        .extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or("shim");
    let bridge_struct = pascal_case(primary) + "Bridge";

    let wit_deps_root = emit_wit::source_shim_deps_dir(primary)?;
    let shim_packages = emit_wit::discover_shim_packages(&wit_deps_root)?;
    let shim_wit_dir = pick_primary_shim_dir(primary, &wit_deps_root, &shim_packages)
        .unwrap_or_else(|| wit_deps_root.clone());

    // Per-shim record-type registry. Drives the per-record wit-
    // value marshaling helpers below (`arg_witvalue_<snake>`,
    // `parse_json_list_record_<snake>`, `ret_to_witvalue_<snake>`)
    // plus the wit-bindgen `additional_derives` ignore filter.
    let records: Vec<RecordType> = record_registry::build(&shim_packages, primary)
        .into_iter()
        .filter(|r| emit_wit::package_belongs_to_primary(&r.package, primary))
        .collect();

    let (scalar_entries, scalar_unwired) =
        interface_db::build_full(plan, &shim_wit_dir, &records)?;

    // Aggregate entries — Phase 1 of the aggregate/UDTF batch.
    // Wires the postgis & mobilitydb dissolve-shape aggregates
    // (`list<borrow<geometry>>` / `list<borrow<raster>>` → result)
    // against DuckDB's `call_aggregate` whole-group fold ABI.
    let (aggregate_entries, aggregate_unwired) =
        interface_db::build_aggregate_registry(plan, &shim_wit_dir, &records)?;

    // UDTF (table-function) entries — Phase 3 of the batch. Wires
    // the row-yielding table functions (st-dump, st-dump-points,
    // st-subdivide, st-asx3d, mobilitydb temporal-joins, ...)
    // against DuckDB's `call_table` whole-rowset return ABI.
    let (udtf_entries, udtf_unwired) =
        interface_db::build_udtf_registry(plan, &shim_wit_dir, &records)?;

    // Report on what fell through so the maintainer sees coverage
    // at codegen time.
    let total_unwired = scalar_unwired.len();
    if total_unwired > 0 {
        eprintln!(
            "[duckdb-target] {total_unwired} scalar(s) not wired:"
        );
        for u in &scalar_unwired {
            eprintln!("  - {}: {}", u.sql_name, u.reason);
        }
    }
    if !aggregate_unwired.is_empty() {
        eprintln!(
            "[duckdb-target] {} aggregate(s) not wired:",
            aggregate_unwired.len(),
        );
        for u in &aggregate_unwired {
            eprintln!("  - {}: {}", u.sql_name, u.reason);
        }
    }
    if !udtf_unwired.is_empty() {
        eprintln!(
            "[duckdb-target] {} table function(s) not wired:",
            udtf_unwired.len(),
        );
        for u in &udtf_unwired {
            eprintln!("  - {}: {}", u.sql_name, u.reason);
        }
    }

    // wit-bindgen's `additional_derives` adds serde::Serialize +
    // serde::Deserialize to EVERY generated type. Contract types
    // (`duckdb:extension/*` flags / variants / records) and
    // helper-component types can't derive serde out-of-the-box;
    // pass their kebab names in `additional_derives_ignore` to
    // restrict the derive to the primary shim's records.
    let duckdb_contract_pkg = emit_wit::discover_duckdb_extension_package()?;
    let mut derives_ignore: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for r in &duckdb_contract_pkg.records {
        derives_ignore.insert(r.kebab_name.clone());
    }
    for v in &duckdb_contract_pkg.variants {
        derives_ignore.insert(v.kebab_name.clone());
    }
    for e in &duckdb_contract_pkg.enums {
        derives_ignore.insert(e.kebab_name.clone());
    }
    for f in &duckdb_contract_pkg.flags {
        derives_ignore.insert(f.kebab_name.clone());
    }
    for pkg in &shim_packages {
        if emit_wit::package_belongs_to_primary(&pkg.ns_name, primary) {
            // Primary-shim variants + flags can't derive serde
            // either; only primary records stay OFF the ignore
            // list so they DO pick up Serialize/Deserialize.
            for v in &pkg.variants {
                derives_ignore.insert(v.kebab_name.clone());
            }
            for f in &pkg.flags {
                derives_ignore.insert(f.kebab_name.clone());
            }
            continue;
        }
        for r in &pkg.records {
            derives_ignore.insert(r.kebab_name.clone());
        }
        for v in &pkg.variants {
            derives_ignore.insert(v.kebab_name.clone());
        }
        for e in &pkg.enums {
            derives_ignore.insert(e.kebab_name.clone());
        }
        for f in &pkg.flags {
            derives_ignore.insert(f.kebab_name.clone());
        }
    }
    let derives_ignore_lits: String = derives_ignore
        .iter()
        .map(|n| format!("            \"{}\",\n", n))
        .collect();

    // Track which WIT module aliases are referenced by the
    // emitted arms so the `use` lines align with what's actually
    // needed. Each dispatch arm references `<module>::<func>`.
    let mut used_aliases: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for (entry, _fallible) in &scalar_entries {
        used_aliases
            .entry(entry.shape.wit_module.clone())
            .or_insert_with(|| entry.shape.wit_package.clone());
        // Some return shapes compose with other interfaces'
        // helpers — record those aliases too. Both live in
        // postgis:wasm.
        match &entry.shape.ret {
            interface_db::RetShape::BboxBlob => {
                used_aliases
                    .entry("pg_ctor".to_string())
                    .or_insert_with(|| "postgis:wasm".to_string());
            }
            interface_db::RetShape::IsValidDetailText => {
                used_aliases
                    .entry("pg_out".to_string())
                    .or_insert_with(|| "postgis:wasm".to_string());
            }
            interface_db::RetShape::Enum {
                wit_module,
                wit_package,
                ..
            } => {
                used_aliases
                    .entry(wit_module.clone())
                    .or_insert_with(|| wit_package.clone());
            }
            _ => {}
        }
        for p in &entry.shape.params {
            if let interface_db::ParamShape::Enum {
                wit_module,
                wit_package,
                ..
            } = p
            {
                used_aliases
                    .entry(wit_module.clone())
                    .or_insert_with(|| wit_package.clone());
            }
        }
    }
    // Aggregate WIT modules — typically `pg_agg`
    // (postgis-aggregates) and `pg_rast_agg`
    // (postgis-raster-aggregates). Same package as the scalar set
    // but a distinct module alias.
    for entry in &aggregate_entries {
        used_aliases
            .entry(entry.shape.wit_module.clone())
            .or_insert_with(|| entry.shape.wit_package.clone());
    }
    // UDTF WIT modules — typically `pg_table` / `pg_dump` /
    // mobilitydb temporal-join modules.
    for entry in &udtf_entries {
        used_aliases
            .entry(entry.shape.wit_module.clone())
            .or_insert_with(|| entry.shape.wit_package.clone());
    }

    // Discover the primary shim package so we can gate resource
    // type imports + per-resource helper emissions on its WIT
    // surface (geometry/raster/topology).
    let shim_pkg = shim_packages
        .iter()
        .find(|p| emit_wit::package_belongs_to_primary(&p.ns_name, primary))
        .cloned();
    let shim_has_geometry_resource = shim_pkg
        .as_ref()
        .map(|p| p.resources.iter().any(|r| r.kebab_name == "geometry"))
        .unwrap_or(false);
    let shim_has_postgis_error = shim_pkg
        .as_ref()
        .map(|p| p.variants.iter().any(|v| v.kebab_name == "postgis-error"))
        .unwrap_or(false);
    let shim_has_raster_resource = shim_pkg
        .as_ref()
        .map(|p| p.resources.iter().any(|r| r.kebab_name == "raster"))
        .unwrap_or(false);
    let shim_has_raster_error = shim_pkg
        .as_ref()
        .map(|p| p.variants.iter().any(|v| v.kebab_name == "raster-error"))
        .unwrap_or(false);
    let shim_has_topology_resource = shim_pkg
        .as_ref()
        .map(|p| p.resources.iter().any(|r| r.kebab_name == "topology"))
        .unwrap_or(false);
    let shim_has_topology_error = shim_pkg
        .as_ref()
        .map(|p| p.variants.iter().any(|v| v.kebab_name == "topology-error"))
        .unwrap_or(false);

    let mut s = String::new();
    s.push_str(HEADER);
    s.push_str(&format!(
        r##"//! Generated by sqlink-shim-codegen (--target duckdb).
//!
//! Scalars are wired against the `duckdb:extension@2.2.0`
//! contract; aggregates / table functions / pragmas / casts
//! return `Duckerror::Unsupported`. See AGENTS.md (in
//! datalink/crates/datalink-shim-duckdb-emit) for the migration
//! plan that landed this target.

#![allow(unused_imports, dead_code)]

extern crate alloc;

use alloc::format;
use alloc::string::{{String, ToString}};
use alloc::vec::Vec;

mod bindings {{
    wit_bindgen::generate!({{
        path: "wit",
        world: "bridge",
        generate_all,
        // Derive serde::Serialize + serde::Deserialize on
        // generated record types so the JSON-direct decode path
        // in the per-record helpers below resolves against the
        // upstream type (`serde_json::from_str::<UPSTREAM>(...)`).
        additional_derives: [serde::Serialize, serde::Deserialize],
        // Contract types + helper-component types + primary-shim
        // variants/flags can't derive serde out-of-the-box.
        additional_derives_ignore: [
{derives_ignore_lits}        ],
    }});
}}

use bindings::duckdb::extension::types;
use bindings::duckdb::extension::runtime;
use bindings::exports::duckdb::extension::callback_dispatch;
use bindings::exports::duckdb::extension::guest;

"##,
        derives_ignore_lits = derives_ignore_lits,
    ));
    let _ = primary;

    // `use bindings::<pkg_ns>::<pkg_name>::<module> as <alias>;`
    // lines for the upstream shim modules referenced by the
    // dispatch arms.
    use datalink_shim_codegen_core::wit_parse;
    for (alias, pkg) in &used_aliases {
        if let Some(module_ident) = wit_parse::alias_to_wit_module_ident(alias) {
            let (pkg_ns, pkg_name) = split_pkg(pkg);
            s.push_str(&format!(
                "use bindings::{pkg_ns}::{pkg_name}::{module_ident} as {alias};\n",
                pkg_ns = sanitize_module(&pkg_ns),
                pkg_name = sanitize_module(&pkg_name),
                module_ident = module_ident,
                alias = alias,
            ));
        }
    }
    // Resource types + variant error idents pulled in only when
    // the primary shim's WIT actually declares them. Same gating
    // as sqlite-emit.
    if shim_has_geometry_resource && shim_has_postgis_error {
        if let Some(p) = shim_pkg.as_ref() {
            let (pkg_ns, pkg_name) = split_pkg(&p.ns_name);
            s.push_str(&format!(
                "use bindings::{pkg_ns}::{pkg_name}::postgis_types::{{Geography, Geometry, PostgisError}};\n",
                pkg_ns = sanitize_module(&pkg_ns),
                pkg_name = sanitize_module(&pkg_name),
            ));
        }
    }
    if shim_has_raster_resource && shim_has_raster_error {
        if let Some(p) = shim_pkg.as_ref() {
            let (pkg_ns, pkg_name) = split_pkg(&p.ns_name);
            s.push_str(&format!(
                "use bindings::{pkg_ns}::{pkg_name}::postgis_raster_types::{{Raster, RasterError}};\n",
                pkg_ns = sanitize_module(&pkg_ns),
                pkg_name = sanitize_module(&pkg_name),
            ));
        }
    }
    if shim_has_topology_resource && shim_has_topology_error {
        if let Some(p) = shim_pkg.as_ref() {
            let (pkg_ns, pkg_name) = split_pkg(&p.ns_name);
            s.push_str(&format!(
                "use bindings::{pkg_ns}::{pkg_name}::postgis_topology_types::{{Topology, TopologyError}};\n",
                pkg_ns = sanitize_module(&pkg_ns),
                pkg_name = sanitize_module(&pkg_name),
            ));
        }
    }
    s.push('\n');

    s.push_str(&format!(
        r##"struct {bridge_struct};

"##,
    ));

    s.push_str(DUCKVALUE_HELPERS);

    // Compose helper-prelude bodies driven by the shim's WIT
    // surface. Each block is independent so a postgis bridge
    // picks up all three; a non-resource shim gets none.
    let mut helpers_block = String::new();
    if shim_has_geometry_resource && shim_has_postgis_error {
        helpers_block.push_str(POSTGIS_HELPERS_BODY);
    }
    if shim_has_raster_resource && shim_has_raster_error {
        if let Some(p) = shim_pkg.as_ref() {
            let (pkg_ns, pkg_name) = split_pkg(&p.ns_name);
            helpers_block.push_str(&render_raster_helpers(
                &sanitize_module(&pkg_ns),
                &sanitize_module(&pkg_name),
            ));
        }
    }
    if shim_has_topology_resource && shim_has_topology_error {
        if let Some(p) = shim_pkg.as_ref() {
            let (pkg_ns, pkg_name) = split_pkg(&p.ns_name);
            helpers_block.push_str(&render_topology_helpers(
                &sanitize_module(&pkg_ns),
                &sanitize_module(&pkg_name),
            ));
        }
    }

    // Tuple-list helpers — one per unique tuple-element signature
    // referenced by a wired ListTuple param.
    let tuple_sigs = collect_tuple_list_sigs(&scalar_entries);
    helpers_block.push_str(&render_tuple_list_helpers(&tuple_sigs));

    // Per-record wit-value helpers — only for records referenced
    // by a wired param or return.
    let referenced_records =
        collect_referenced_records(&scalar_entries, &aggregate_entries);
    let helper_records: Vec<RecordType> = records
        .iter()
        .filter(|r| referenced_records.contains(&r.kebab_name))
        .cloned()
        .collect();
    helpers_block.push_str(&render_wit_value_helpers(&helper_records));

    s.push_str(&helpers_block);

    s.push_str(handle_table::render());
    s.push_str(&lifecycle::render(
        &bridge_struct,
        plan,
        !aggregate_entries.is_empty(),
        !udtf_entries.is_empty(),
    ));

    // call_scalar dispatch: build the per-arm match
    let mut scalar_arms = String::new();
    let scalar_arm_count = build_scalar_arms(&mut scalar_arms, &scalar_entries);
    let _ = scalar_arm_count;

    // call_aggregate dispatch: build the per-arm match
    let mut aggregate_arms = String::new();
    let aggregate_arm_count =
        build_aggregate_arms(&mut aggregate_arms, &aggregate_entries);
    let _ = aggregate_arm_count;

    // call_table dispatch: build the per-arm match
    let mut table_arms = String::new();
    let table_arm_count = build_table_arms(&mut table_arms, &udtf_entries);
    let _ = table_arm_count;

    s.push_str(&format!(
        r##"
impl callback_dispatch::Guest for {bridge_struct} {{
    fn call_scalar_batch(
        handle: u32,
        rows: Vec<Vec<types::Duckvalue>>,
        ctx: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {{
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, args) in rows.into_iter().enumerate() {{
            let row_ctx = types::Invokeinfo {{
                rowindex: Some(base + i as u64),
                iswindow: ctx.iswindow,
            }};
            out.push(<Self as callback_dispatch::Guest>::call_scalar(handle, args, row_ctx)?);
        }}
        Ok(out)
    }}

    fn call_scalar(
        handle: u32,
        args: Vec<types::Duckvalue>,
        _ctx: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {{
        let arm_idx = handle_table()
            .lock()
            .expect("scalar handle mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal(
                "unknown scalar handle".into()
            ))?;
        // Default DuckDB null semantics: a NULL arg short-circuits
        // to NULL before the function runs. The base
        // `runtime.scalar-registry.register` API the codegen uses
        // here doesn't take a null-handling arm (that's
        // `runtime-ext.register-scalar-ex`), so DuckDB enforces
        // the propagate path engine-side. We defensively check
        // here too in case the host plumbs a NULL through.
        if args.iter().any(|v| matches!(v, types::Duckvalue::Null)) {{
            return Ok(types::Duckvalue::Null);
        }}
        match arm_idx {{
{scalar_arms}            _ => Err(types::Duckerror::Internal(format!(
                "unknown scalar arm index {{}}", arm_idx
            ))),
        }}
    }}

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {{
        let arm_idx = table_handle_table()
            .lock()
            .expect("table handle mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal(
                "unknown table function handle".into()
            ))?;
        match arm_idx {{
{table_arms}            _ => Err(types::Duckerror::Internal(format!(
                "unknown table function arm index {{}}", arm_idx
            ))),
        }}
    }}
    fn call_aggregate(
        handle: u32,
        rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {{
        let arm_idx = aggregate_handle_table()
            .lock()
            .expect("aggregate handle mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal(
                "unknown aggregate handle".into()
            ))?;
        match arm_idx {{
{aggregate_arms}            _ => Err(types::Duckerror::Internal(format!(
                "unknown aggregate arm index {{}}", arm_idx
            ))),
        }}
    }}
    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {{
        Err(types::Duckerror::Unsupported(
            format!("{primary}: pragmas not wired (Step 4 scalar-first cut)")
        ))
    }}
    fn call_cast(
        _handle: u32,
        _value: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {{
        Err(types::Duckerror::Unsupported(
            format!("{primary}: casts not wired (Step 4 scalar-first cut)")
        ))
    }}
}}
"##,
        bridge_struct = bridge_struct,
        scalar_arms = scalar_arms,
        aggregate_arms = aggregate_arms,
        table_arms = table_arms,
        primary = primary,
    ));

    // register_scalars() body. Threading `scalar_entries` here
    // lets register::render mirror build_scalar_arms's sql_name
    // dedupe (so the handle→arm_idx map points at real arms) and
    // derive per-arg Logicaltype widths from the ParamShape IR.
    s.push_str(&register::render(plan, &scalar_entries)?);

    // register_aggregates() body. Emitted only when any aggregate
    // entry was classified; otherwise lifecycle::load skips the
    // call. Mirrors the scalar registration shape but against the
    // `Capabilitykind::Aggregate` registry.
    if !aggregate_entries.is_empty() {
        s.push_str(&register::render_aggregates(plan, &aggregate_entries)?);
    }
    // register_tables() body. Same shape as aggregates against the
    // `Capabilitykind::Table` registry.
    if !udtf_entries.is_empty() {
        s.push_str(&register::render_tables(plan, &udtf_entries)?);
    }

    // Export macro at file scope
    s.push_str(&format!(
        "\nbindings::export!({bridge_struct} with_types_in bindings);\n"
    ));

    let _ = crate_name; // surface in README/CARGO; not used in lib.rs body
    let _ = bridge_struct;
    Ok(s)
}

/// Build the per-arm scalar dispatch match arms. Each arm-index
/// gets one body produced by `dispatch::emit_scalar_arm_body`.
/// The arm-index ordering MUST match the iteration order in
/// `register::render` — both walk the BridgePlan's scalars in
/// declaration order and number each (canonical + alias) one
/// after the next.
fn build_scalar_arms(
    out: &mut String,
    scalar_entries: &[(interface_db::DispatchEntry, bool)],
) -> usize {
    // Map sql_name -> arm_index. Some interface DB entries may
    // surface multiple times if the same SQL function appears via
    // aliases; first writer wins (matches sqlite-emit's
    // `seen_ids.insert(id)` first-writer pattern).
    let mut arm_for: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    let mut next: usize = 0;
    // First pass: build the (sql_name -> arm_index) map matching
    // register::render's iteration order. We need to iterate the
    // BridgePlan here, not scalar_entries — register iterates the
    // plan to allocate handles, and scalar_entries is the SUBSET
    // that the wit-side classifier recognised.
    //
    // To stay in sync with register::render WITHOUT requiring a
    // shared iteration pass, we mirror its scalar-by-scalar walk
    // through the entries' sql_name order. Since scalar_entries
    // is built by the same plan-walk classifier inside
    // build_full, the iteration order matches the plan order for
    // every entry the classifier produced. We assign arm indices
    // in that order; the dispatch match arm key is the assigned
    // index.
    for (entry, _fallible) in scalar_entries {
        let key: &str = entry.sql_name.as_str();
        arm_for.entry(key).or_insert_with(|| {
            let i = next;
            next += 1;
            i
        });
    }

    // Track which arm indices have already been emitted so we
    // don't write the same `<arm_idx>usize => { ... }` arm twice
    // when two SQL-name entries (canonical + alias, or two
    // aliases) resolve to the same arm index. The match would
    // accept it but rustc warns `unreachable_patterns` on the
    // second emission. Mirrors sqlite-emit's `seen_ids` pattern
    // in `emit_scalar_impl`.
    let mut emitted: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    for (entry, fallible) in scalar_entries {
        let key: &str = entry.sql_name.as_str();
        let arm_idx = match arm_for.get(key) {
            Some(&i) => i,
            None => continue,
        };
        if !emitted.insert(arm_idx) {
            // Same arm-index already emitted; skip to avoid the
            // unreachable-pattern compiler warning.
            continue;
        }
        // Emit the arm body. The dispatch loop matches on
        // `arm_idx` (usize); we render `<arm_idx>usize => { ... }`.
        let body = dispatch::emit_scalar_arm_body(
            &entry.shape,
            *fallible,
            &entry.sql_name,
            "                ",
        );
        out.push_str(&format!(
            "            {arm_idx}usize => {{\n{body}\n            }}\n",
        ));
    }
    next
}

/// Build the per-arm aggregate dispatch match arms. Each unique
/// SQL name (canonical + each alias resolves to a separate
/// registration but they share an arm-index since they all
/// invoke the same WIT upstream). Mirrors `build_scalar_arms`'s
/// sql_name dedupe so the handle→arm_idx map produced by
/// `register::render_aggregates` lines up with these arms.
fn build_aggregate_arms(
    out: &mut String,
    agg_entries: &[interface_db::AggregateEntry],
) -> usize {
    let mut arm_for: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    let mut next: usize = 0;
    for entry in agg_entries {
        let key: &str = entry.sql_name.as_str();
        arm_for.entry(key).or_insert_with(|| {
            let i = next;
            next += 1;
            i
        });
    }
    let mut emitted: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    for entry in agg_entries {
        let key: &str = entry.sql_name.as_str();
        let arm_idx = match arm_for.get(key) {
            Some(&i) => i,
            None => continue,
        };
        if !emitted.insert(arm_idx) {
            continue;
        }
        let body = dispatch::emit_aggregate_arm_body(
            &entry.shape,
            &entry.sql_name,
            "                ",
        );
        out.push_str(&format!(
            "            {arm_idx}usize => {{\n{body}\n            }}\n",
        ));
    }
    next
}

/// Build the per-arm UDTF dispatch match arms. Same dedupe
/// pattern as build_scalar_arms / build_aggregate_arms.
fn build_table_arms(
    out: &mut String,
    udtf_entries: &[interface_db::UdtfEntry],
) -> usize {
    let mut arm_for: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    let mut next: usize = 0;
    for entry in udtf_entries {
        let key: &str = entry.sql_name.as_str();
        arm_for.entry(key).or_insert_with(|| {
            let i = next;
            next += 1;
            i
        });
    }
    let mut emitted: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    for entry in udtf_entries {
        let key: &str = entry.sql_name.as_str();
        let arm_idx = match arm_for.get(key) {
            Some(&i) => i,
            None => continue,
        };
        if !emitted.insert(arm_idx) {
            continue;
        }
        let body = dispatch::emit_udtf_call_body(
            &entry.shape,
            &entry.sql_name,
            "                ",
        );
        out.push_str(&format!(
            "            {arm_idx}usize => {{\n{body}\n            }}\n",
        ));
    }
    next
}

const HEADER: &str =
    "// === GENERATED by sqlink-shim-codegen (target=duckdb)  do not edit by hand ===\n\n";

/// Duckvalue ↔ Rust helpers ferried into the bridge's lib.rs
/// prelude. Mirror of the `arg_text` / `arg_blob` / `arg_f64` /
/// `arg_i64` set in sqlite-emit but unpacking from
/// `types::Duckvalue` arms.
const DUCKVALUE_HELPERS: &str = r##"// ─── Duckvalue arg helpers ───
//
// Mirror of sqlite-emit's `arg_text`/`arg_blob`/`arg_i64` set
// but unpacking from `types::Duckvalue` arms. The DuckDB FROZEN
// set is wider than SQLite's: integer / unsigned / date / etc.
// arms all coerce to a common Rust primitive via the four
// helpers below.

fn dv_text<'a>(args: &'a [types::Duckvalue], idx: usize, name: &str) -> Result<&'a str, types::Duckerror> {
    match args.get(idx) {
        Some(types::Duckvalue::Text(s)) => Ok(s.as_str()),
        _ => Err(types::Duckerror::Invalidargument(format!(
            "{name}: arg {idx} must be VARCHAR"
        ))),
    }
}

fn dv_blob<'a>(args: &'a [types::Duckvalue], idx: usize, name: &str) -> Result<&'a [u8], types::Duckerror> {
    match args.get(idx) {
        Some(types::Duckvalue::Blob(b)) => Ok(b.as_slice()),
        Some(types::Duckvalue::Text(s)) => Ok(s.as_bytes()),
        _ => Err(types::Duckerror::Invalidargument(format!(
            "{name}: arg {idx} must be BLOB"
        ))),
    }
}

fn dv_f64(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<f64, types::Duckerror> {
    match args.get(idx) {
        Some(types::Duckvalue::Float64(v)) => Ok(*v),
        Some(types::Duckvalue::Float32(v)) => Ok(*v as f64),
        Some(types::Duckvalue::Int64(v)) => Ok(*v as f64),
        Some(types::Duckvalue::Int32(v)) => Ok(*v as f64),
        Some(types::Duckvalue::Uint64(v)) => Ok(*v as f64),
        Some(types::Duckvalue::Uint32(v)) => Ok(*v as f64),
        Some(types::Duckvalue::Int16(v)) => Ok(*v as f64),
        Some(types::Duckvalue::Int8(v)) => Ok(*v as f64),
        Some(types::Duckvalue::Uint16(v)) => Ok(*v as f64),
        Some(types::Duckvalue::Uint8(v)) => Ok(*v as f64),
        _ => Err(types::Duckerror::Invalidargument(format!(
            "{name}: arg {idx} must be DOUBLE"
        ))),
    }
}

fn dv_i64(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<i64, types::Duckerror> {
    match args.get(idx) {
        Some(types::Duckvalue::Int64(v)) => Ok(*v),
        Some(types::Duckvalue::Int32(v)) => Ok(*v as i64),
        Some(types::Duckvalue::Uint64(v)) => Ok(*v as i64),
        Some(types::Duckvalue::Uint32(v)) => Ok(*v as i64),
        Some(types::Duckvalue::Int16(v)) => Ok(*v as i64),
        Some(types::Duckvalue::Int8(v)) => Ok(*v as i64),
        Some(types::Duckvalue::Uint16(v)) => Ok(*v as i64),
        Some(types::Duckvalue::Uint8(v)) => Ok(*v as i64),
        Some(types::Duckvalue::Boolean(b)) => Ok(if *b { 1 } else { 0 }),
        Some(types::Duckvalue::Float64(v)) => Ok(*v as i64),
        Some(types::Duckvalue::Float32(v)) => Ok(*v as i64),
        _ => Err(types::Duckerror::Invalidargument(format!(
            "{name}: arg {idx} must be INTEGER"
        ))),
    }
}

fn dv_bool(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<bool, types::Duckerror> {
    match args.get(idx) {
        Some(types::Duckvalue::Boolean(b)) => Ok(*b),
        Some(types::Duckvalue::Int64(v)) => Ok(*v != 0),
        Some(types::Duckvalue::Int32(v)) => Ok(*v != 0),
        _ => Err(types::Duckerror::Invalidargument(format!(
            "{name}: arg {idx} must be BOOLEAN"
        ))),
    }
}

/// Generic error formatter for fallible upstream WIT calls. The
/// dispatch arm wraps the upstream error via this fn into a
/// String the Duckerror::Invalidargument arm carries.
fn shim_err_string<E: core::fmt::Debug>(e: E) -> String {
    format!("{:?}", e)
}

// ── JSON-as-TEXT primitive `list<X>` param helpers ──
//
// Mirror of sqlite-emit's `parse_json_list_<suffix>` set but
// error-wrapped into `types::Duckerror::Invalidargument`. The SQL
// caller passes a JSON-array literal in the TEXT/VARCHAR arg
// (e.g. `'[1.0, 2.0, 3.0]'`); the helper decodes via serde_json
// into a `Vec<T>` which the dispatch arm passes to the WIT
// function as `&[T]`.

fn parse_json_list_f64(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<f64>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<f64>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of f64 ({e})")))
}

fn parse_json_list_i32(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<i32>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<i32>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of s32 ({e})")))
}

fn parse_json_list_i64(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<i64>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<i64>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of s64 ({e})")))
}

fn parse_json_list_u32(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<u32>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<u32>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of u32 ({e})")))
}

fn parse_json_list_u64(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<u64>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<u64>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of u64 ({e})")))
}

fn parse_json_list_u8(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<u8>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<u8>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of u8 ({e})")))
}

fn parse_json_list_bool(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<bool>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<bool>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of bool ({e})")))
}

fn parse_json_list_string(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<String>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<String>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of string ({e})")))
}

"##;

/// PostGIS-specific helpers — emitted only when the shim's WIT
/// declares `resource geometry` + `variant postgis-error`. Mirror
/// of sqlite-emit's `POSTGIS_HELPERS_BODY` but error-wrapped into
/// `types::Duckerror::Invalidargument`.
const POSTGIS_HELPERS_BODY: &str = r#"
// ─── PostGIS WKB decoders ───

fn from_wkb(bytes: &[u8], name: &str) -> Result<Geometry, types::Duckerror> {
    Geometry::from_wkb(bytes).map_err(|e| types::Duckerror::Invalidargument(format!("{name}: {}", postgis_err_string(e))))
}

fn geog_from_wkb(bytes: &[u8], name: &str) -> Result<Geography, types::Duckerror> {
    Geography::from_wkb(bytes).map_err(|e| types::Duckerror::Invalidargument(format!("{name}: {}", postgis_err_string(e))))
}

fn postgis_err_string(e: PostgisError) -> String {
    match e {
        PostgisError::InvalidGeometry(s)
        | PostgisError::ParseError(s)
        | PostgisError::UnsupportedOperation(s)
        | PostgisError::NumericError(s)
        | PostgisError::SridMismatch(s)
        | PostgisError::General(s) => s,
    }
}
"#;

/// Raster-resource prelude helpers — gated on the shim WIT
/// declaring `resource raster` + `variant raster-error`.
fn render_raster_helpers(pkg_ns: &str, pkg_name: &str) -> String {
    format!(
        r#"
// ─── PostGIS raster decoders ───

fn from_raster_binary(bytes: &[u8], name: &str) -> Result<Raster, types::Duckerror> {{
    bindings::{pkg_ns}::{pkg_name}::postgis_raster_types::from_binary(bytes)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{{}}: {{}}", name, raster_err_string(e))))
}}

fn raster_err_string(e: RasterError) -> String {{
    match e {{
        RasterError::ParseError(s)
        | RasterError::OutOfBounds(s)
        | RasterError::TypeMismatch(s)
        | RasterError::General(s) => s,
    }}
}}
"#,
        pkg_ns = pkg_ns,
        pkg_name = pkg_name,
    )
}

/// Topology-resource prelude helpers — gated on the shim WIT
/// declaring `resource topology` + `variant topology-error`.
fn render_topology_helpers(pkg_ns: &str, pkg_name: &str) -> String {
    format!(
        r#"
// ─── PostGIS topology decoders ───

fn from_topology_bytes(bytes: &[u8], name: &str) -> Result<Topology, types::Duckerror> {{
    bindings::{pkg_ns}::{pkg_name}::postgis_topology_types::from_bytes(bytes)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{{}}: {{}}", name, topology_err_string(e))))
}}

fn topology_err_string(e: TopologyError) -> String {{
    match e {{
        TopologyError::InvalidTopology(s) | TopologyError::General(s) => s,
        TopologyError::NodeNotFound(id) => format!("node not found: {{}}", id),
        TopologyError::EdgeNotFound(id) => format!("edge not found: {{}}", id),
        TopologyError::FaceNotFound(id) => format!("face not found: {{}}", id),
    }}
}}
"#,
        pkg_ns = pkg_ns,
        pkg_name = pkg_name,
    )
}

/// Walk the wired entries and collect every record name that
/// appears in a `WitValueRecord` / `ListRecord` param or
/// `WitValueRecord` / `OptionWitValueRecord` /
/// `FirstWitValueRecord` return. Drives the per-record helper
/// emission filter.
fn collect_referenced_records(
    scalar_entries: &[(interface_db::DispatchEntry, bool)],
    aggregate_entries: &[interface_db::AggregateEntry],
) -> std::collections::BTreeSet<String> {
    let mut out: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for (entry, _f) in scalar_entries {
        for p in &entry.shape.params {
            match p {
                ParamShape::WitValueRecord { kebab_name, .. } => {
                    out.insert(kebab_name.clone());
                }
                ParamShape::ListRecord { kebab_name, .. } => {
                    out.insert(kebab_name.clone());
                }
                _ => {}
            }
        }
        match &entry.shape.ret {
            RetShape::WitValueRecord { kebab_name, .. }
            | RetShape::OptionWitValueRecord { kebab_name, .. }
            | RetShape::FirstWitValueRecord { kebab_name, .. } => {
                out.insert(kebab_name.clone());
            }
            _ => {}
        }
    }
    // #607 Phase 2 + #612 (OQ1): AccKind::Record aggregates
    // reference TWO per-record codec sites — `arg_witvalue_<in>`
    // for the input record's decoder + `ret_to_witvalue_<out>`
    // for the output record's encoder. Same-record aggregates have
    // matching kebabs (so only one codec block is emitted via the
    // BTreeSet dedupe); different-record (#612) cases need both.
    //
    // #614: `RecordToScalar` only references the INPUT-side
    // `arg_witvalue_<in>` helper — the output is a primitive
    // scalar wrap, not a record codec call.
    for entry in aggregate_entries {
        match &entry.shape.accumulator_kind {
            interface_db::AccKind::Record { input, output } => {
                out.insert(input.kebab_name.clone());
                out.insert(output.kebab_name.clone());
            }
            interface_db::AccKind::RecordToScalar { input, .. } => {
                out.insert(input.kebab_name.clone());
            }
            interface_db::AccKind::Geom | interface_db::AccKind::Raster => {}
        }
    }
    out
}

/// Collect every unique tuple-element signature that appears in a
/// `ParamShape::ListTuple`. Drives `render_tuple_list_helpers`.
fn collect_tuple_list_sigs(
    scalar_entries: &[(interface_db::DispatchEntry, bool)],
) -> std::collections::BTreeSet<Vec<ListPrimElem>> {
    let mut out: std::collections::BTreeSet<Vec<ListPrimElem>> =
        std::collections::BTreeSet::new();
    for (entry, _f) in scalar_entries {
        for p in &entry.shape.params {
            if let Some(sig) = p.list_tuple_sig() {
                out.insert(sig.to_vec());
            }
        }
    }
    out
}

/// Render `parse_json_list_tuple_<sig>` helpers — one per unique
/// tuple-element signature referenced by a wired ListTuple param.
fn render_tuple_list_helpers(
    sigs: &std::collections::BTreeSet<Vec<ListPrimElem>>,
) -> String {
    if sigs.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    s.push_str(
        "\n// ─── list<tuple<...>> param helpers ───\n\
         // One per unique tuple-element signature referenced by a wired\n\
         // dispatch arm. SQL passes a JSON-array of arrays as the TEXT/VARCHAR\n\
         // arg (e.g. `'[[1, 10], [20, 30]]'`); serde_json parses tuples\n\
         // as fixed-length JSON arrays so the upstream\n\
         // `Vec<(T1, T2, ...)>` binding round-trips directly.\n",
    );
    for elements in sigs {
        let suffix = interface_db::list_tuple_sig_suffix(elements);
        let elems_joined = elements
            .iter()
            .map(|e| e.rust_elem())
            .collect::<Vec<_>>()
            .join(", ");
        let rust_tuple = if elements.len() == 1 {
            format!("({},)", elems_joined)
        } else {
            format!("({})", elems_joined)
        };
        let wit_label = elements
            .iter()
            .map(|e| match e {
                ListPrimElem::F64 => "f64",
                ListPrimElem::F32 => "f32",
                ListPrimElem::S32 => "s32",
                ListPrimElem::S64 => "s64",
                ListPrimElem::U32 => "u32",
                ListPrimElem::U64 => "u64",
                ListPrimElem::U8 => "u8",
                ListPrimElem::Bool => "bool",
                ListPrimElem::String => "string",
            })
            .collect::<Vec<_>>()
            .join(", ");
        s.push_str(&format!(
            "fn parse_json_list_tuple_{suffix}(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<{rust_tuple}>, types::Duckerror> {{\n\
             \x20   let text = dv_text(args, idx, name)?;\n\
             \x20   serde_json::from_str::<Vec<{rust_tuple}>>(text)\n\
             \x20       .map_err(|e| types::Duckerror::Invalidargument(format!(\"{{name}}: arg {{idx}} must be JSON array of [{wit_label}] tuples ({{e}})\")))\n\
             }}\n\n",
        ));
    }
    s
}

/// Render per-record wit-value marshaling helpers. Each record
/// referenced by a wired dispatch arm gets:
///
///   - `arg_witvalue_<snake>(args, idx, name) -> Result<UPSTREAM, Duckerror>`
///     unwraps a `Duckvalue::Complex` carrier, parses
///     `serde_json::from_str::<UPSTREAM>(&cmplx.json)` straight
///     into the upstream type. Works for any record whose upstream
///     binding derives serde::Deserialize via the wit-bindgen
///     `additional_derives` arg.
///
///   - `parse_json_list_record_<snake>(args, idx, name) -> Result<Vec<UPSTREAM>, Duckerror>`
///     parses a JSON-array TEXT arg into `Vec<UPSTREAM>`.
///
///   - `ret_to_witvalue_<snake>(upstream) -> Result<Duckvalue, Duckerror>`
///     serializes via serde_json and wraps in `Duckvalue::Complex`
///     with the record's symbolic name in `type_expr`.
fn render_wit_value_helpers(records: &[RecordType]) -> String {
    if records.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    s.push_str(
        "\n// ─── Per-record wit-value marshaling helpers ───\n\
         // Records ride on `Duckvalue::Complex { type_expr, json }`.\n\
         // Decode parses `json` straight into UPSTREAM via\n\
         // serde_json::from_str (UPSTREAM derives serde derives via\n\
         // wit-bindgen's `additional_derives` arg above). Encode\n\
         // round-trips UPSTREAM → JSON → Complex carrier.\n",
    );
    for r in records {
        let snake = r.snake_name();
        let pascal = pascal_case(&r.kebab_name);
        let upstream_iface_snake = sanitize_module(&r.interface);
        let (pkg_ns, pkg_name) = split_pkg(&r.package);
        let upstream_path = format!(
            "bindings::{ns}::{name}::{iface}::{pascal}",
            ns = sanitize_module(&pkg_ns),
            name = sanitize_module(&pkg_name),
            iface = upstream_iface_snake,
            pascal = pascal,
        );
        let symbolic = r.symbolic_name.replace('"', "\\\"");
        let kebab = &r.kebab_name;
        s.push_str(&format!(
            "fn arg_witvalue_{snake}(\n\
             \x20   args: &[types::Duckvalue],\n\
             \x20   idx: usize,\n\
             \x20   name: &str,\n\
             ) -> Result<{upstream_path}, types::Duckerror> {{\n\
             \x20   let cmplx = match args.get(idx) {{\n\
             \x20       Some(types::Duckvalue::Complex(c)) => c,\n\
             \x20       _ => return Err(types::Duckerror::Invalidargument(\n\
             \x20           format!(\"{{name}}: arg {{idx}} must be COMPLEX (wit-value record)\"))),\n\
             \x20   }};\n\
             \x20   serde_json::from_str::<{upstream_path}>(&cmplx.json)\n\
             \x20       .map_err(|e| types::Duckerror::Invalidargument(\n\
             \x20           format!(\"{{name}}: decode arg {{idx}}: {{}}\", e)))\n\
             }}\n\n",
        ));
        s.push_str(&format!(
            "fn parse_json_list_record_{snake}(\n\
             \x20   args: &[types::Duckvalue],\n\
             \x20   idx: usize,\n\
             \x20   name: &str,\n\
             ) -> Result<Vec<{upstream_path}>, types::Duckerror> {{\n\
             \x20   let text = dv_text(args, idx, name)?;\n\
             \x20   serde_json::from_str::<Vec<{upstream_path}>>(text)\n\
             \x20       .map_err(|e| types::Duckerror::Invalidargument(\n\
             \x20           format!(\"{{name}}: arg {{idx}} must be JSON array of {kebab} ({{e}})\")))\n\
             }}\n\n",
            kebab = kebab,
        ));
        s.push_str(&format!(
            "fn ret_to_witvalue_{snake}(\n\
             \x20   upstream: {upstream_path},\n\
             ) -> Result<types::Duckvalue, types::Duckerror> {{\n\
             \x20   let json = serde_json::to_string(&upstream)\n\
             \x20       .map_err(|e| types::Duckerror::Internal(format!(\"encode {snake} wit-value: {{}}\", e)))?;\n\
             \x20   Ok(types::Duckvalue::Complex(types::Complexvalue {{\n\
             \x20       type_expr: \"{symbolic}\".into(),\n\
             \x20       json,\n\
             \x20   }}))\n\
             }}\n\n",
            symbolic = symbolic,
        ));
    }
    s
}


/// Discover which subdir of `wit_deps_root` holds the primary
/// shim's upstream WIT package. Same heuristic as sqlite-emit's
/// `pick_primary_shim_dir`.
fn pick_primary_shim_dir(
    primary: &str,
    wit_deps_root: &std::path::Path,
    shim_packages: &[datalink_shim_codegen_core::wit_parse::WitPackage],
) -> Option<std::path::PathBuf> {
    use datalink_shim_codegen_core::wit_parse;
    for pkg in shim_packages {
        let ns = pkg.ns_name.split(':').next().unwrap_or("");
        if ns == primary {
            if let Ok(rd) = std::fs::read_dir(wit_deps_root) {
                for e in rd.flatten() {
                    if !e.path().is_dir() {
                        continue;
                    }
                    if let Ok(Some(p)) = wit_parse::parse_package_dir(&e.path()) {
                        if p.ns_name == pkg.ns_name {
                            return Some(e.path());
                        }
                    }
                }
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(wit_deps_root) {
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let name = e.file_name();
            let sname = name.to_string_lossy();
            if sname.starts_with(primary) {
                return Some(e.path());
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(wit_deps_root) {
        let mut paths: Vec<std::path::PathBuf> =
            rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        paths.sort();
        for p in paths {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            match name {
                "sqlite-extension" | "sfcgal-component" | "duckdb-extension" => continue,
                _ => return Some(p),
            }
        }
    }
    None
}

fn split_pkg(pkg: &str) -> (String, String) {
    match pkg.find(':') {
        Some(i) => (pkg[..i].to_string(), pkg[i + 1..].to_string()),
        None => (pkg.to_string(), String::new()),
    }
}

fn sanitize_module(s: &str) -> String {
    s.replace('-', "_")
}

fn pascal_case(s: &str) -> String {
    let mut out = String::new();
    let mut up = true;
    for c in s.chars() {
        if c == '-' || c == '_' || c.is_whitespace() {
            up = true;
            continue;
        }
        if up {
            for u in c.to_uppercase() {
                out.push(u);
            }
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}
