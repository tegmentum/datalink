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

    // #661: window-function classification + wiring. The bridge
    // world unconditionally exports `aggregate-incr-dispatch` (see
    // `DUCKDB_EXPORTS`); `build_window_dispatch_impl` emits the
    // `aggregate_incr_dispatch::Guest` impl with all 5 trait
    // methods. The 4 state-machine arms are stubs (Unsupported);
    // `call_aggregate_window` dispatches by handle to per-arm
    // bodies emitted from each classified `WindowEntry`. The
    // host-side runtime wrapper (ducklink-runtime's
    // `aggregate_window`) drives `call-aggregate-window` from a
    // DuckDB window aggregate's per-output-row dispatch.
    let (window_entries, window_unwired) =
        interface_db::build_window_registry(plan, &shim_wit_dir)?;

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
    if !window_unwired.is_empty() {
        eprintln!(
            "[duckdb-target] {} window function(s) not wired:",
            window_unwired.len(),
        );
        for u in &window_unwired {
            eprintln!("  - {}: {}", u.sql_name, u.reason);
        }
    }
    if !window_entries.is_empty() {
        eprintln!(
            "[duckdb-target] {} window function(s) wired via aggregate-incr-dispatch (#661):",
            window_entries.len(),
        );
        for e in &window_entries {
            eprintln!("  - {}", e.sql_name);
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
            }
            | interface_db::RetShape::OptionEnum {
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
//! Scalars / aggregates / casts / table-functions wire against
//! the `duckdb:extension@4.0.0` callback-dispatch surface. The
//! hot-path columnar methods (call-scalar-batch-col /
//! call-aggregate-col / call-cast-col, #653) lift their colvec
//! args to row-major up-front and route through the cold-path
//! row-major bodies. The call-pragma arm (#617 stub, refactored
//! in #625 into `build_pragma_dispatch_impl`) returns
//! `Duckerror::Unsupported` until a real pragma surface lands.
//! Window functions (#626) are classified by
//! `build_window_registry` so the maintainer sees coverage at
//! codegen time, but the bridge world does not yet export the
//! optional `aggregate-incr-dispatch` interface (which carries
//! `call-aggregate-window`) is wired via #661 -- the bridge world
//! exports `aggregate-incr-dispatch` so wit-bindgen generates the
//! 5-method trait; `build_window_dispatch_impl` emits the impl,
//! with `call_aggregate_window` routing per-handle to the
//! classified `WindowEntry` arm body.

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
use bindings::duckdb::extension::catalog;
use bindings::duckdb::extension::column_types;
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
    s.push_str(COLVEC_HELPERS);

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

    // #724: mixed-tuple helpers where at least one element is a
    // same-shim record (e.g. `string_tfloat_sequence`).
    let tuple_mixed_sigs = collect_tuple_list_mixed_sigs(&scalar_entries);
    helpers_block.push_str(&render_tuple_list_mixed_helpers(&tuple_mixed_sigs));

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

    // Cast surface — drives both lifecycle's `register_casts()?;`
    // call AND the call_cast forwarder below. Empty surface keeps
    // the original Unsupported stub so an unwired bridge stays
    // load-safe.
    let has_casts = plan
        .extensions
        .iter()
        .any(|e| !e.cast_rewrites.is_empty());

    s.push_str(handle_table::render());
    s.push_str(&lifecycle::render(
        &bridge_struct,
        plan,
        !aggregate_entries.is_empty(),
        !udtf_entries.is_empty(),
        has_casts,
        !window_entries.is_empty(),
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
    // === HOT PATH (duckdb:extension@4.0.0 columnar dispatch) ===
    //
    // The major-4 contract replaced the row-major call_scalar_batch /
    // call_aggregate / (and added call_cast_col) with columnar
    // variants that cross the ABI as `list<colvec>`. Correctness-first
    // migration: convert columns -> rows on demand, route through the
    // row-major cold paths, then rebuild a colvec on the way out.
    //
    // #659 micro-opts on the lift/lower paths:
    //   * Opt 1: scalar-batch / cast paths skip the
    //     `Vec<Vec<Duckvalue>>` intermediate -- they materialize one
    //     row at a time via `materialize_row` / `colvec_get` (the
    //     aggregate path still uses `colvecs_to_rows` because its
    //     arm bodies iterate `rows` directly).
    //   * Opt 2: `values_to_colvec` fuses validity + type-sniff
    //     into a single pass and pre-allocates the typed `Vec<T>`
    //     from the captured arm-kind.
    //   * Opt 3: scalar-batch reuses a single per-row buffer across
    //     iterations -- the buffer's capacity is preserved by
    //     `materialize_row` (`std::mem::take` then re-grow to
    //     `args.len()`).
    //
    // The contract envisions bulk memcpy on fixed-width columns;
    // that is a future optimisation -- mobilitydb-style workloads
    // are dominated by per-row binary/text decode in the arm
    // bodies, so the savings here are on the lift/lower prologue
    // rather than the arm dispatch itself.

    fn call_scalar_batch_col(
        handle: u32,
        args: Vec<column_types::Colvec>,
        ctx: types::Invokeinfo,
    ) -> Result<column_types::Colvec, types::Duckerror> {{
        let n_rows = validate_colvec_rows(&args)?;
        let n_args = args.len();
        let base = ctx.rowindex.unwrap_or(0);
        let mut out: Vec<types::Duckvalue> = Vec::with_capacity(n_rows);
        // Pooled per-row buffer (#659 Opt 3). `materialize_row`
        // clears + re-grows on each iteration, preserving capacity
        // across rows; we mem::take it into the Guest call (which
        // consumes the Vec by value), then re-grow on the next
        // iteration. The per-row alloc is unavoidable as long as
        // call_scalar's WIT-derived signature takes
        // `args: Vec<Duckvalue>` by value -- the capacity hint at
        // least keeps the alloc right-sized.
        let mut row_buf: Vec<types::Duckvalue> = Vec::with_capacity(n_args);
        for i in 0..n_rows {{
            materialize_row(&args, i, &mut row_buf);
            let row_ctx = types::Invokeinfo {{
                rowindex: Some(base + i as u64),
                iswindow: ctx.iswindow,
            }};
            let row_args = core::mem::take(&mut row_buf);
            out.push(<Self as callback_dispatch::Guest>::call_scalar(handle, row_args, row_ctx)?);
        }}
        values_to_colvec(out)
    }}

    fn call_aggregate_col(
        handle: u32,
        args: Vec<column_types::Colvec>,
    ) -> Result<types::Duckvalue, types::Duckerror> {{
        let rows = colvecs_to_rows(&args)?;
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

    fn call_cast_col(
        handle: u32,
        arg: column_types::Colvec,
    ) -> Result<column_types::Colvec, types::Duckerror> {{
        // #659 Opt 1: lift one cell at a time via `colvec_get`
        // rather than building the full `Vec<Duckvalue>` upfront.
        let n_rows = arg.rows as usize;
        let mut out: Vec<types::Duckvalue> = Vec::with_capacity(n_rows);
        for i in 0..n_rows {{
            let v = colvec_get(&arg, i);
            out.push(<Self as callback_dispatch::Guest>::call_cast(handle, v)?);
        }}
        values_to_colvec(out)
    }}

    // === COLD SINGLETON PATHS (row-major, duckdb:extension@4.0.0) ===

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

{pragma_arm}{cast_arm}}}
{window_arm}
"##,
        bridge_struct = bridge_struct,
        scalar_arms = scalar_arms,
        aggregate_arms = aggregate_arms,
        table_arms = table_arms,
        pragma_arm = build_pragma_dispatch_impl(primary),
        window_arm = build_window_dispatch_impl(primary, &bridge_struct, &window_entries),
        cast_arm = if has_casts {
            // #624: call_cast forwards into call_scalar at the
            // matching arm. `register_casts()` slotted the cast
            // handle into the SAME scalar `handle_table` so the
            // forward looks up arm_idx via the existing lookup.
            // One arm-index space; cast is just an alternate entry
            // point for the same scalar body.
            String::from(
                "    fn call_cast(\n\
                 \x20       handle: u32,\n\
                 \x20       value: types::Duckvalue,\n\
                 \x20   ) -> Result<types::Duckvalue, types::Duckerror> {\n\
                 \x20       <Self as callback_dispatch::Guest>::call_scalar(\n\
                 \x20           handle,\n\
                 \x20           alloc::vec![value],\n\
                 \x20           types::Invokeinfo { rowindex: None, iswindow: false },\n\
                 \x20       )\n\
                 \x20   }\n",
            )
        } else {
            format!(
                "    fn call_cast(\n\
                 \x20       _handle: u32,\n\
                 \x20       _value: types::Duckvalue,\n\
                 \x20   ) -> Result<types::Duckvalue, types::Duckerror> {{\n\
                 \x20       Err(types::Duckerror::Unsupported(\n\
                 \x20           format!(\"{primary}: casts not wired (no cast_rewrites in IR)\")\n\
                 \x20       ))\n\
                 \x20   }}\n",
                primary = primary,
            )
        },
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
    // register_casts() body (#624). Walks plan.extensions[*].
    // cast_rewrites and registers each with the host's catalog
    // interface; each cast handle slots into the shared scalar
    // `handle_table` so call_cast forwards into call_scalar at the
    // matching arm.
    if has_casts {
        s.push_str(&register::render_casts(plan, &scalar_entries)?);
    }

    // register_windows() body (#661). Each classified window
    // function lands in the aggregate-registry (the @4.0.0 contract
    // has no window-registry resource); the returned handle slots
    // into `window_handle_table` so the `call_aggregate_window` arm
    // of the `aggregate_incr_dispatch::Guest` impl routes back to
    // the per-arm dispatch body.
    if !window_entries.is_empty() {
        s.push_str(&register::render_windows(&window_entries)?);
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
    // Phase 1A: AggregateEntry carries a canonical sql_name plus an
    // inline `aliases` Vec. The pre-Phase-1A iteration produced one
    // (canonical or alias) entry per dispatch row; we now expand the
    // alias list inline at the use site to keep the arm/handle
    // index identical to `register::render_aggregates`.
    let mut arm_for: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut next: usize = 0;
    for entry in agg_entries {
        for name in std::iter::once(entry.sql_name.clone())
            .chain(entry.aliases.iter().cloned())
        {
            arm_for.entry(name).or_insert_with(|| {
                let i = next;
                next += 1;
                i
            });
        }
    }
    let mut emitted: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    for entry in agg_entries {
        for name in std::iter::once(entry.sql_name.as_str())
            .chain(entry.aliases.iter().map(|s| s.as_str()))
        {
            let arm_idx = match arm_for.get(name) {
                Some(&i) => i,
                None => continue,
            };
            if !emitted.insert(arm_idx) {
                continue;
            }
            let body = dispatch::emit_aggregate_arm_body(
                &entry.shape,
                name,
                "                ",
            );
            out.push_str(&format!(
                "            {arm_idx}usize => {{\n{body}\n            }}\n",
            ));
        }
    }
    next
}

/// #625: build the `call_pragma` arm of the
/// `callback_dispatch::Guest` impl.
///
/// The `duckdb:extension@4.0.0` callback-dispatch trait surface
/// requires a `call-pragma` method on every bridge. Real
/// dispatch will route by handle into a per-pragma arm body,
/// parallel to `call_scalar` / `call_aggregate` / `call_table`,
/// once the substrate carrying pragma metadata lands. That
/// substrate is missing today on three axes:
///
///  1. **Extractor**: `shim-interface-core` walks each shim's
///     WIT + Rust source for scalar / aggregate / UDTF / cast
///     surfaces but has no pragma extraction pass. No upstream
///     shim (`postgis`, `mobilitydb`, the query.farm cores)
///     advertises a pragma in its sources, so the extractor
///     has no test corpus to drive a pass against.
///  2. **Interface DB**: the schema has tables for
///     `scalars` / `aggregates` / `table_functions` /
///     `cast_rewrites` / `system_catalog_tables` /
///     `spatial_indexes`, but no `pragmas` table. A new
///     `pragmas` table + `BridgePlan.pragmas: Vec<PragmaEntry>`
///     field (mirroring `cast_rewrites`) is the substrate
///     shape the dispatch will read from.
///  3. **Register emission**: `register::render` emits
///     `register_scalars` / `register_aggregates` /
///     `register_tables` / `register_casts` bodies that thread
///     each handle into the matching `handle_table` map. A
///     `register::render_pragmas` body keyed off
///     `pragma-registry.register-call` (advertised on the
///     `runtime` interface) is the missing emission step.
///
/// Mirrors the honest-stub pattern of #620 (system-catalog) and
/// #621 (index-plugin): the trait method is present so a probing
/// host gets a diagnostic rather than a missing-export crash,
/// and the error string names both the primary shim and the
/// dispatch arm so the failure mode is unambiguous in logs.
///
/// When the first shim registers a pragma, the helper grows
/// the same shape as `build_scalar_arms` / `build_aggregate_arms`
/// (a `pragma_handle_table` lookup → arm-index → arm body
/// rendered by `dispatch::emit_pragma_arm_body`) and the static
/// `_handle` / `_args` bindings become live.
fn build_pragma_dispatch_impl(primary: &str) -> String {
    format!(
        "    // #625: pragma dispatch placeholder. The
    // duckdb:extension@4.0.0 callback-dispatch surface
    // requires a call-pragma arm, but the codegen has no
    // substrate carrying pragma metadata yet --
    // shim-interface-core does not extract pragmas and the
    // interface DB schema has no `pragmas` table. Real
    // per-arm dispatch (parallel to `call_scalar` /
    // `call_aggregate`) lands once a shim registers its first
    // pragma; until then the host gets an Unsupported
    // diagnostic naming the primary shim and dispatch arm.
    // See `build_pragma_dispatch_impl` for the substrate
    // extension path.
    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {{
        Err(types::Duckerror::Unsupported(format!(
            \"{primary} pragma dispatch: not implemented \
             (no pragmas registered by this bridge)\"
        )))
    }}
",
        primary = primary,
    )
}

/// #661: cross-component window-dispatch builder.
///
/// Returns the string emitted AFTER the `impl callback_dispatch
/// ::Guest for $Bridge { ... }` block: a SEPARATE
/// `impl aggregate_incr_dispatch::Guest for $Bridge { ... }` block
/// with all 5 trait methods. The 4 state-machine arms (init /
/// update / combine / finalize) are stubs returning Unsupported;
/// `call_aggregate_window` dispatches by handle to per-arm bodies
/// emitted from `dispatch::emit_window_arm_body`.
///
/// The substrate is missing on three axes — each owns one
/// upstream coordination step:
///
///  1. **Bridge world surface** (`emit_wit::DUCKDB_EXPORTS` +
///     `world bridge` in the rendered `wit/world.wit`): the
///     export list must grow `aggregate-incr-dispatch` (or the
///     bridge must switch to the `duckdb-extension-aggregate-
///     incr` superset world). Without that, wit-bindgen does
///     not emit the `aggregate_incr_dispatch::Guest` trait
///     and there is no `fn call_aggregate_window` to fill in.
///  2. **ducklink-runtime dispatch wrapper**
///     (`crates/ducklink-runtime/src/extension.rs`): the
///     host-side currently has `call_call_aggregate_init /
///     update / combine / finalize / col` wrappers around the
///     extension's `aggregate-incr-dispatch` export, but no
///     `call_call_aggregate_window` wrapper. The runtime needs
///     a method that lifts a `Vec<Vec<Duckvalue>>` partition +
///     `WindowFrame { start, end }` to the wit-bindgen guest
///     trait and lowers the per-row result back to
///     `Duckvalue`. Mirrors the existing aggregate-incr
///     wrappers' shape.
///  3. **Emit body + register pass**: once (1) is in place,
///     this helper grows the same shape as
///     `build_aggregate_arms` (a `window_handle_table` lookup
///     into per-arm bodies that decode the partition rows +
///     extra args, call the upstream WIT function, and slice
///     the partition's `list<Y>` return down to the frame
///     range). `register::render_windows` registers each
///     window function against `runtime.window-registry`
///     (or whichever registration surface the bridge world's
///     additive imports settle on) and slots the handle into
///     `window_handle_table`. The classified `window_entries`
///     produced by `interface_db::build_window_registry`
///     already carry the per-row `WindowReturn` discriminant
///     (`OptionU32` / `U32` / `GeomBlob`) so the per-arm
///     decode follows the existing scalar/aggregate emit
///     recipes.
///
/// Mirrors the honest-stub pattern of #617 (pragma trait
/// method exists, body returns Unsupported), #620
/// (system-catalog), #621 (datafission index-plugin) and
/// #625 (pragma helper extraction). The difference here is
/// that the trait method does NOT exist in the current world
/// — so the helper produces no generated text until the
/// substrate (1) lands. The shim WIT, register pass, and
/// host runtime wrapper land in lockstep across
/// ducklink + datalink in a follow-up.
fn build_window_dispatch_impl(
    primary: &str,
    bridge_struct: &str,
    window_entries: &[interface_db::WindowEntry],
) -> String {
    // #661: the bridge world now exports `aggregate-incr-dispatch`
    // (see `DUCKDB_EXPORTS`), so wit-bindgen generates an
    // `aggregate_incr_dispatch::Guest` trait that MUST be
    // implemented. We emit it unconditionally so a bridge with
    // zero classified window entries (e.g. mobilitydb) still
    // compiles and loads.
    //
    // The 4 state-machine methods (init/update/combine/finalize)
    // are stubs returning Unsupported -- the @4.0.0 postgis pilot
    // is whole-partition compute, not state-machine. They survive
    // as stubs so the trait surface is complete.
    //
    // `call_aggregate_window` is the alternative-path arm: per
    // output row the host would hand the bridge the whole
    // partition's rows + a WindowFrame. The arm looks up the
    // per-handle arm-index from `window_handle_table` and routes
    // to the per-window-function body emitted by
    // `dispatch::emit_window_arm_body`.
    //
    // DORMANT (#662): the 10 postgis window functions (#661)
    // execute end-to-end via the vanilla C aggregate API --
    // DuckDB-core registers all aggregates (window-flagged or not)
    // via the C aggregate registration surface, and DuckDB's
    // window engine drives them through the existing init/update/
    // combine/finalize callbacks. The corresponding host-side
    // wrapper (`ducklink-runtime` `aggregate_window`) currently
    // has zero callers, so this generated `call_aggregate_window`
    // body is also dormant in practice. Per
    // duckdb-wasm/docs/v3-core-shim-plan.md: WINDOW executes via
    // the aggregate path; this is an alternative dispatch path,
    // not the primary one. The arm survives generation so the
    // alternative path can be activated by ducklink-host wiring
    // without re-emitting the bridge.

    let mut window_arms = String::new();
    let arm_count = build_window_arms(&mut window_arms, window_entries);

    // When there are no window entries, the `partition` / `frame`
    // bindings are unused and `_partition` / `_frame` keeps rustc
    // quiet. The `arm_idx` lookup chain is unconditional so the
    // trait method shape stays uniform.
    let (partition_bind, frame_bind) = if arm_count == 0 {
        ("_partition", "_frame")
    } else {
        ("partition", "frame")
    };

    format!(
        r##"
impl bindings::exports::duckdb::extension::aggregate_incr_dispatch::Guest for {bridge_struct} {{
    // #661: 4 state-machine arms stubbed -- the postgis pilot is
    // whole-partition compute (call_aggregate_window only). A
    // future incremental aggregate registration would replace each
    // body with a per-handle dispatch arm; the trait surface stays
    // the same.
    fn call_aggregate_init(
        _handle: u32,
    ) -> Result<u32, types::Duckerror> {{
        Err(types::Duckerror::Unsupported(format!(
            "{primary}: aggregate-incr init not wired (no incremental aggregates registered)"
        )))
    }}

    fn call_aggregate_update(
        _handle: u32,
        _state: u32,
        _rows: Vec<Vec<types::Duckvalue>>,
    ) -> Result<(), types::Duckerror> {{
        Err(types::Duckerror::Unsupported(format!(
            "{primary}: aggregate-incr update not wired"
        )))
    }}

    fn call_aggregate_combine(
        _handle: u32,
        _target: u32,
        _source: u32,
    ) -> Result<(), types::Duckerror> {{
        Err(types::Duckerror::Unsupported(format!(
            "{primary}: aggregate-incr combine not wired"
        )))
    }}

    fn call_aggregate_finalize(
        _handle: u32,
        _state: u32,
    ) -> Result<types::Duckvalue, types::Duckerror> {{
        Err(types::Duckerror::Unsupported(format!(
            "{primary}: aggregate-incr finalize not wired"
        )))
    }}

    fn call_aggregate_window(
        handle: u32,
        {partition_bind}: Vec<Vec<types::Duckvalue>>,
        {frame_bind}: bindings::exports::duckdb::extension::aggregate_incr_dispatch::WindowFrame,
    ) -> Result<types::Duckvalue, types::Duckerror> {{
        let arm_idx = window_handle_table()
            .lock()
            .expect("window handle mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal(
                "unknown window handle".into()
            ))?;
        match arm_idx {{
{window_arms}            _ => Err(types::Duckerror::Internal(format!(
                "unknown window arm index {{}}", arm_idx
            ))),
        }}
    }}
}}
"##,
    )
}

/// #661: build the per-arm window-function dispatch match arms.
/// One body per unique SQL name (canonical + alias). Mirrors
/// `build_aggregate_arms`'s sql_name dedupe so the handle-to-arm
/// index map produced by `register::render_windows` lines up.
fn build_window_arms(
    out: &mut String,
    window_entries: &[interface_db::WindowEntry],
) -> usize {
    let mut arm_for: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    let mut next: usize = 0;
    for entry in window_entries {
        arm_for.entry(entry.sql_name.as_str()).or_insert_with(|| {
            let i = next;
            next += 1;
            i
        });
    }
    let mut emitted: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    for entry in window_entries {
        let key: &str = entry.sql_name.as_str();
        let arm_idx = match arm_for.get(key) {
            Some(&i) => i,
            None => continue,
        };
        if !emitted.insert(arm_idx) {
            continue;
        }
        let body = dispatch::emit_window_arm_body(
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

// #674: `list<list<u8>>` param helper — batched WKB blobs for the
// postgis `st_*_batch` family. SQL passes JSON text matching
// `Vec<Vec<u8>>` (nested arrays of byte integers); symmetric with
// the `list<list<X>>` return convention.
fn parse_json_list_list_u8(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<Vec<u8>>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<Vec<u8>>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of byte arrays ({e})")))
}

// #695: `list<list<X>>` param helpers for primitive non-u8
// elements. Today's surface: postgis raster `st-set-values`
// (`list<list<f64>>`) + flatgeobuf coord-list constructors.
// Symmetric with the `RetShape::JsonText { ListListPrim }` return.
fn parse_json_list_list_f64(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<Vec<f64>>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<Vec<f64>>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of f64 arrays ({e})")))
}

fn parse_json_list_list_i32(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<Vec<i32>>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<Vec<i32>>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of s32 arrays ({e})")))
}

fn parse_json_list_list_i64(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<Vec<i64>>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<Vec<i64>>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of s64 arrays ({e})")))
}

fn parse_json_list_list_u32(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<Vec<u32>>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<Vec<u32>>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of u32 arrays ({e})")))
}

fn parse_json_list_list_u64(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<Vec<u64>>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<Vec<u64>>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of u64 arrays ({e})")))
}

fn parse_json_list_list_bool(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<Vec<bool>>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<Vec<bool>>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of bool arrays ({e})")))
}

fn parse_json_list_list_string(args: &[types::Duckvalue], idx: usize, name: &str) -> Result<Vec<Vec<String>>, types::Duckerror> {
    let text = dv_text(args, idx, name)?;
    serde_json::from_str::<Vec<Vec<String>>>(text)
        .map_err(|e| types::Duckerror::Invalidargument(format!("{name}: arg {idx} must be JSON array of string arrays ({e})")))
}

"##;

/// Columnar <-> row-major conversion helpers (#653).
///
/// The duckdb:extension@4.0.0 callback-dispatch hot-path methods
/// (call-scalar-batch-col / call-aggregate-col / call-cast-col)
/// transport their args as `list<colvec>` instead of the previous
/// `list<list<duckvalue>>` rowbatch. The hot-path methods in the
/// generated bridge convert columns -> rows up-front, dispatch via
/// the cold-path row-major arm bodies, then convert results back
/// into a columnar return for scalar / cast (aggregate finalises
/// to a single duckvalue so no col-rebuild is needed).
///
/// Correctness-first: the conversion is element-wise. The contract
/// envisions bulk-memcpy on fixed-width columns; that is a future
/// optimisation -- the row-major dispatch already pays the per-cell
/// cost via the existing arm bodies.
const COLVEC_HELPERS: &str = r##"// --- Colvec <-> row-major helpers (#653 columnar callback-dispatch,
//     #659 micro-opts on the lift/lower hot path) ---

/// Validity-bitmap probe. An empty bitmap means "all rows valid"
/// (matching DuckDB's null-validity pointer convention); otherwise
/// bit i of the little-endian byte stream is set iff row i is non-NULL.
fn cv_is_valid(validity: &[u8], i: usize) -> bool {
    if validity.is_empty() {
        return true;
    }
    match validity.get(i / 8) {
        Some(byte) => (byte & (1u8 << (i % 8))) != 0,
        None => true,
    }
}

/// (#659 Opt 1) Lift a single cell from a colvec into a Duckvalue
/// without materializing the whole column. Used by the per-row
/// dispatch loops in `call_scalar_batch_col` / `call_cast_col` to
/// avoid the `Vec<Vec<Duckvalue>>` intermediate. Variable-width
/// arms (Text / Blob / Complex) clone; fixed-width arms copy.
fn colvec_get(cv: &column_types::Colvec, i: usize) -> types::Duckvalue {
    if !cv_is_valid(cv.validity.as_slice(), i) {
        return types::Duckvalue::Null;
    }
    let v = match &cv.data {
        column_types::Column::Boolean(xs)     => xs.get(i).copied().map(types::Duckvalue::Boolean),
        column_types::Column::Int64(xs)       => xs.get(i).copied().map(types::Duckvalue::Int64),
        column_types::Column::Uint64(xs)      => xs.get(i).copied().map(types::Duckvalue::Uint64),
        column_types::Column::Float64(xs)     => xs.get(i).copied().map(types::Duckvalue::Float64),
        column_types::Column::Int32(xs)       => xs.get(i).copied().map(types::Duckvalue::Int32),
        column_types::Column::Timestamp(xs)   => xs.get(i).copied().map(types::Duckvalue::Timestamp),
        column_types::Column::Int8(xs)        => xs.get(i).copied().map(types::Duckvalue::Int8),
        column_types::Column::Int16(xs)       => xs.get(i).copied().map(types::Duckvalue::Int16),
        column_types::Column::Uint8(xs)       => xs.get(i).copied().map(types::Duckvalue::Uint8),
        column_types::Column::Uint16(xs)      => xs.get(i).copied().map(types::Duckvalue::Uint16),
        column_types::Column::Uint32(xs)      => xs.get(i).copied().map(types::Duckvalue::Uint32),
        column_types::Column::Float32(xs)     => xs.get(i).copied().map(types::Duckvalue::Float32),
        column_types::Column::Date(xs)        => xs.get(i).copied().map(types::Duckvalue::Date),
        column_types::Column::Time(xs)        => xs.get(i).copied().map(types::Duckvalue::Time),
        column_types::Column::Timestamptz(xs) => xs.get(i).copied().map(types::Duckvalue::Timestamptz),
        column_types::Column::Decimal(xs) => xs.get(i).cloned().map(|d| {
            types::Duckvalue::Decimal(types::Decimalvalue {
                lower: d.lower, upper: d.upper, width: d.width, scale: d.scale,
            })
        }),
        column_types::Column::Interval(xs) => xs.get(i).cloned().map(|d| {
            types::Duckvalue::Interval(types::Intervalvalue {
                months: d.months, days: d.days, micros: d.micros,
            })
        }),
        column_types::Column::Uuid(xs) => xs.get(i).cloned().map(|d| {
            types::Duckvalue::Uuid(types::Uuidvalue { hi: d.hi, lo: d.lo })
        }),
        column_types::Column::Text(xs)  => xs.get(i).cloned().map(types::Duckvalue::Text),
        column_types::Column::Blob(xs)  => xs.get(i).cloned().map(types::Duckvalue::Blob),
        column_types::Column::Complex(xs) => xs.get(i).cloned().map(|c| {
            types::Duckvalue::Complex(types::Complexvalue {
                type_expr: c.type_expr, json: c.json,
            })
        }),
    };
    v.unwrap_or(types::Duckvalue::Null)
}

/// (#659 Opt 1) Validate that every input colvec shares the same
/// row count and return it. Mismatched lengths return an Internal
/// error -- the host-side dispatcher should never deliver ragged
/// column slices.
fn validate_colvec_rows(args: &[column_types::Colvec]) -> Result<usize, types::Duckerror> {
    let n_rows = if args.is_empty() { 0 } else { args[0].rows as usize };
    for (j, cv) in args.iter().enumerate() {
        if cv.rows as usize != n_rows {
            return Err(types::Duckerror::Internal(format!(
                "columnar dispatch: arg-column {j} has rows={} but expected {n_rows}",
                cv.rows,
            )));
        }
    }
    Ok(n_rows)
}

/// (#659 Opt 1 + Opt 3) Materialize row `i` from a slice of colvecs
/// into the provided buffer. The buffer is cleared first; capacity
/// is preserved across calls, so the dispatch loop can reuse a
/// single allocation across rows. This avoids the
/// `Vec<Vec<Duckvalue>>` intermediate (one column-Vec per arg plus
/// one row-Vec per row) that the original `colvecs_to_rows`
/// allocated.
fn materialize_row(args: &[column_types::Colvec], i: usize, out: &mut Vec<types::Duckvalue>) {
    out.clear();
    if out.capacity() < args.len() {
        out.reserve(args.len() - out.capacity());
    }
    for cv in args {
        out.push(colvec_get(cv, i));
    }
}

/// Lift a single `colvec` to a row-major `Vec<Duckvalue>`.
/// (Retained for the aggregate dispatch path; the scalar-batch and
/// cast paths now use `colvec_get` directly via `materialize_row`.)
fn colvec_to_values(cv: &column_types::Colvec) -> Vec<types::Duckvalue> {
    let n = cv.rows as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(colvec_get(cv, i));
    }
    out
}

/// Lift `args: Vec<Colvec>` to `Vec<Vec<Duckvalue>>`. (Retained for
/// the aggregate dispatch path -- aggregate arms iterate `rows`
/// directly. The scalar-batch and cast paths use `validate_colvec_rows`
/// + `materialize_row` to skip the intermediate `Vec<Vec<_>>`.)
fn colvecs_to_rows(args: &[column_types::Colvec]) -> Result<Vec<Vec<types::Duckvalue>>, types::Duckerror> {
    let n_rows = validate_colvec_rows(args)?;
    let n_args = args.len();
    let mut rows: Vec<Vec<types::Duckvalue>> = Vec::with_capacity(n_rows);
    for i in 0..n_rows {
        let mut row: Vec<types::Duckvalue> = Vec::with_capacity(n_args);
        for cv in args {
            row.push(colvec_get(cv, i));
        }
        rows.push(row);
    }
    Ok(rows)
}

/// (#659 Opt 2) Lower a row-major `Vec<Duckvalue>` back into a
/// `colvec`. Fuses the original two passes (`build_validity_bitmap`
/// then `iter().find()` for the type sniff) into a single sweep,
/// then dispatches once on the captured arm-kind to pre-allocate
/// the typed `Vec<T>`. An all-NULL / empty input falls back to a
/// boolean column (the validity bitmap carries the NULLs, so the
/// data arm is just a placeholder of the right length).
///
/// Note: the underlying lift-arm signature (handle -> Duckvalue)
/// forces a per-cell tag dispatch in the build loops. Threading
/// the arm's `RetShape` (known at codegen time) through a per-handle
/// shape registry would let us skip the tag check entirely, but
/// that touches register.rs + the dispatch surface and is out of
/// scope for #659. The sniff path here keeps the change local to
/// emit_lib.rs while still removing one full pass over `values`.
fn values_to_colvec(values: Vec<types::Duckvalue>) -> Result<column_types::Colvec, types::Duckerror> {
    // Local arm-kind tag captured during the validity pass. Cheaper
    // than holding a `&Duckvalue` borrow into `values` once we want
    // to consume `values` for the variable-width arms.
    #[derive(Clone, Copy)]
    enum K {
        Bool, I64, U64, F64, I32, Ts, I8, I16, U8, U16, U32, F32,
        Date, Time, Tstz, Decimal, Interval, Uuid, Text, Blob, Complex,
    }
    fn kind_of(v: &types::Duckvalue) -> Option<K> {
        match v {
            types::Duckvalue::Null         => None,
            types::Duckvalue::Boolean(_)   => Some(K::Bool),
            types::Duckvalue::Int64(_)     => Some(K::I64),
            types::Duckvalue::Uint64(_)    => Some(K::U64),
            types::Duckvalue::Float64(_)   => Some(K::F64),
            types::Duckvalue::Int32(_)     => Some(K::I32),
            types::Duckvalue::Timestamp(_) => Some(K::Ts),
            types::Duckvalue::Int8(_)      => Some(K::I8),
            types::Duckvalue::Int16(_)     => Some(K::I16),
            types::Duckvalue::Uint8(_)     => Some(K::U8),
            types::Duckvalue::Uint16(_)    => Some(K::U16),
            types::Duckvalue::Uint32(_)    => Some(K::U32),
            types::Duckvalue::Float32(_)   => Some(K::F32),
            types::Duckvalue::Date(_)      => Some(K::Date),
            types::Duckvalue::Time(_)      => Some(K::Time),
            types::Duckvalue::Timestamptz(_) => Some(K::Tstz),
            types::Duckvalue::Decimal(_)   => Some(K::Decimal),
            types::Duckvalue::Interval(_)  => Some(K::Interval),
            types::Duckvalue::Uuid(_)      => Some(K::Uuid),
            types::Duckvalue::Text(_)      => Some(K::Text),
            types::Duckvalue::Blob(_)      => Some(K::Blob),
            types::Duckvalue::Complex(_)   => Some(K::Complex),
        }
    }

    let n = values.len();
    let rows = n as u32;

    // Single-pass validity build + first-non-null sniff.
    let mut bits: Vec<u8> = alloc::vec![0u8; (n + 7) / 8];
    let mut any_null = false;
    let mut kind: Option<K> = None;
    for (i, v) in values.iter().enumerate() {
        if matches!(v, types::Duckvalue::Null) {
            any_null = true;
        } else {
            bits[i / 8] |= 1u8 << (i % 8);
            if kind.is_none() { kind = kind_of(v); }
        }
    }
    let validity = if any_null { bits } else { Vec::new() };

    // Typed pre-allocation per arm. `Vec::with_capacity(n)` +
    // pushes is semantically identical to `iter().map().collect()`
    // for a slice iterator (which already calls `size_hint`), but
    // hoisting the allocation up to the arm head makes the pattern
    // obvious for future per-shape specialization.
    let data = match kind {
        None => column_types::Column::Boolean(alloc::vec![false; n]),
        Some(K::Bool) => {
            let mut xs: Vec<bool> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Boolean(b) = v { *b } else { false });
            }
            column_types::Column::Boolean(xs)
        }
        Some(K::I64) => {
            let mut xs: Vec<i64> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Int64(x) = v { *x } else { 0 });
            }
            column_types::Column::Int64(xs)
        }
        Some(K::U64) => {
            let mut xs: Vec<u64> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Uint64(x) = v { *x } else { 0 });
            }
            column_types::Column::Uint64(xs)
        }
        Some(K::F64) => {
            let mut xs: Vec<f64> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Float64(x) = v { *x } else { 0.0 });
            }
            column_types::Column::Float64(xs)
        }
        Some(K::I32) => {
            let mut xs: Vec<i32> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Int32(x) = v { *x } else { 0 });
            }
            column_types::Column::Int32(xs)
        }
        Some(K::Ts) => {
            let mut xs: Vec<i64> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Timestamp(x) = v { *x } else { 0 });
            }
            column_types::Column::Timestamp(xs)
        }
        Some(K::I8) => {
            let mut xs: Vec<i8> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Int8(x) = v { *x } else { 0 });
            }
            column_types::Column::Int8(xs)
        }
        Some(K::I16) => {
            let mut xs: Vec<i16> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Int16(x) = v { *x } else { 0 });
            }
            column_types::Column::Int16(xs)
        }
        Some(K::U8) => {
            let mut xs: Vec<u8> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Uint8(x) = v { *x } else { 0 });
            }
            column_types::Column::Uint8(xs)
        }
        Some(K::U16) => {
            let mut xs: Vec<u16> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Uint16(x) = v { *x } else { 0 });
            }
            column_types::Column::Uint16(xs)
        }
        Some(K::U32) => {
            let mut xs: Vec<u32> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Uint32(x) = v { *x } else { 0 });
            }
            column_types::Column::Uint32(xs)
        }
        Some(K::F32) => {
            let mut xs: Vec<f32> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Float32(x) = v { *x } else { 0.0 });
            }
            column_types::Column::Float32(xs)
        }
        Some(K::Date) => {
            let mut xs: Vec<i32> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Date(x) = v { *x } else { 0 });
            }
            column_types::Column::Date(xs)
        }
        Some(K::Time) => {
            let mut xs: Vec<i64> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Time(x) = v { *x } else { 0 });
            }
            column_types::Column::Time(xs)
        }
        Some(K::Tstz) => {
            let mut xs: Vec<i64> = Vec::with_capacity(n);
            for v in &values {
                xs.push(if let types::Duckvalue::Timestamptz(x) = v { *x } else { 0 });
            }
            column_types::Column::Timestamptz(xs)
        }
        Some(K::Decimal) => {
            let mut xs: Vec<column_types::Decimalvalue> = Vec::with_capacity(n);
            for v in &values {
                xs.push(match v {
                    types::Duckvalue::Decimal(d) => column_types::Decimalvalue {
                        lower: d.lower, upper: d.upper, width: d.width, scale: d.scale,
                    },
                    _ => column_types::Decimalvalue { lower: 0, upper: 0, width: 0, scale: 0 },
                });
            }
            column_types::Column::Decimal(xs)
        }
        Some(K::Interval) => {
            let mut xs: Vec<column_types::Intervalvalue> = Vec::with_capacity(n);
            for v in &values {
                xs.push(match v {
                    types::Duckvalue::Interval(d) => column_types::Intervalvalue {
                        months: d.months, days: d.days, micros: d.micros,
                    },
                    _ => column_types::Intervalvalue { months: 0, days: 0, micros: 0 },
                });
            }
            column_types::Column::Interval(xs)
        }
        Some(K::Uuid) => {
            let mut xs: Vec<column_types::Uuidvalue> = Vec::with_capacity(n);
            for v in &values {
                xs.push(match v {
                    types::Duckvalue::Uuid(d) => column_types::Uuidvalue { hi: d.hi, lo: d.lo },
                    _ => column_types::Uuidvalue { hi: 0, lo: 0 },
                });
            }
            column_types::Column::Uuid(xs)
        }
        Some(K::Text) => {
            let mut xs: Vec<String> = Vec::with_capacity(n);
            for v in values.into_iter() {
                xs.push(match v {
                    types::Duckvalue::Text(s) => s,
                    _ => String::new(),
                });
            }
            column_types::Column::Text(xs)
        }
        Some(K::Blob) => {
            let mut xs: Vec<Vec<u8>> = Vec::with_capacity(n);
            for v in values.into_iter() {
                xs.push(match v {
                    types::Duckvalue::Blob(b) => b,
                    _ => Vec::new(),
                });
            }
            column_types::Column::Blob(xs)
        }
        Some(K::Complex) => {
            let mut xs: Vec<column_types::Complexvalue> = Vec::with_capacity(n);
            for v in values.into_iter() {
                xs.push(match v {
                    types::Duckvalue::Complex(c) => column_types::Complexvalue {
                        type_expr: c.type_expr, json: c.json,
                    },
                    _ => column_types::Complexvalue {
                        type_expr: String::new(), json: String::new(),
                    },
                });
            }
            column_types::Column::Complex(xs)
        }
    };

    Ok(column_types::Colvec { data, validity, rows })
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
    // #614 + #640: `RecordToScalar` / `RecordToTuple` only reference
    // the INPUT-side `arg_witvalue_<in>` helper — the output is a
    // primitive scalar wrap (#614) or a JSON-encoded primitive tuple
    // (#640), not a record codec call.
    for entry in aggregate_entries {
        match &entry.shape.accumulator_kind {
            interface_db::AccKind::Record { input, output }
            | interface_db::AccKind::RecordSetToRecordSet { input, output } => {
                out.insert(input.kebab_name.clone());
                out.insert(output.kebab_name.clone());
            }
            interface_db::AccKind::RecordToScalar { input, .. }
            | interface_db::AccKind::RecordToTuple { input, .. }
            | interface_db::AccKind::RecordToListPrim { input, .. } => {
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

/// #724: collect mixed-tuple signatures (each element is a primitive
/// OR a same-shim record) for `ParamShape::ListTupleMixed`.
fn collect_tuple_list_mixed_sigs(
    scalar_entries: &[(interface_db::DispatchEntry, bool)],
) -> Vec<Vec<interface_db::ListTupleElem>> {
    let mut out: Vec<Vec<interface_db::ListTupleElem>> = Vec::new();
    for (entry, _f) in scalar_entries {
        for p in &entry.shape.params {
            if let Some(sig) = p.list_tuple_mixed_sig() {
                let suf = interface_db::list_tuple_mixed_sig_suffix(sig);
                if !out
                    .iter()
                    .any(|prev| interface_db::list_tuple_mixed_sig_suffix(prev) == suf)
                {
                    out.push(sig.to_vec());
                }
            }
        }
    }
    out
}

/// #724: render mixed-tuple `parse_json_list_tuple_<sig>` helpers.
/// Record elements resolve to upstream Rust paths carrying
/// `serde::Deserialize` via wit-bindgen's `additional_derives`.
fn render_tuple_list_mixed_helpers(
    sigs: &[Vec<interface_db::ListTupleElem>],
) -> String {
    if sigs.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    s.push_str(
        "\n// ─── #724 mixed-tuple list<tuple<primitive|record, ...>> helpers ───\n",
    );
    for sig in sigs {
        let suffix = interface_db::list_tuple_mixed_sig_suffix(sig);
        let rust_elems: Vec<String> = sig
            .iter()
            .map(|e| match e {
                interface_db::ListTupleElem::Prim(p) => p.rust_elem().to_string(),
                interface_db::ListTupleElem::Record(r) => {
                    let (pkg_ns, pkg_name) = split_pkg(&r.wit_package);
                    format!(
                        "bindings::{ns}::{name}::{iface}::{pascal}",
                        ns = sanitize_module(&pkg_ns),
                        name = sanitize_module(&pkg_name),
                        iface = sanitize_module(&r.wit_interface),
                        pascal = pascal_case(&r.kebab_name),
                    )
                }
            })
            .collect();
        let rust_tuple = if rust_elems.len() == 1 {
            format!("({},)", rust_elems[0])
        } else {
            format!("({})", rust_elems.join(", "))
        };
        let wit_label = sig
            .iter()
            .map(|e| match e {
                interface_db::ListTupleElem::Prim(p) => match p {
                    ListPrimElem::F64 => "f64",
                    ListPrimElem::F32 => "f32",
                    ListPrimElem::S32 => "s32",
                    ListPrimElem::S64 => "s64",
                    ListPrimElem::U32 => "u32",
                    ListPrimElem::U64 => "u64",
                    ListPrimElem::U8 => "u8",
                    ListPrimElem::Bool => "bool",
                    ListPrimElem::String => "string",
                }
                .to_string(),
                interface_db::ListTupleElem::Record(r) => r.kebab_name.clone(),
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
        // #710: `helper_snake` disambiguates when two records in the
        // same package share a kebab (mobilitydb's `stbox3d` lives in
        // both `stbox-ops` and `stbox3d-ops` with different field
        // orders). Non-colliding kebabs fall back to `snake_name()`.
        let snake = r.helper_snake();
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
        // #781: `list<list<R>>` param helper — parallel to the
        // flat-list `parse_json_list_record_<snake>` one level
        // deeper. Reads a TEXT arg holding
        // `serde_json::from_str::<Vec<Vec<UPSTREAM>>>` and hands
        // back a `Vec<Vec<UPSTREAM>>` for the dispatcher to pass
        // as `&arg{idx}` (coerces to `&[Vec<UPSTREAM>]`, the
        // wit-bindgen binding for `list<list<record>>`).
        s.push_str(&format!(
            "fn parse_json_list_list_record_{snake}(\n\
             \x20   args: &[types::Duckvalue],\n\
             \x20   idx: usize,\n\
             \x20   name: &str,\n\
             ) -> Result<Vec<Vec<{upstream_path}>>, types::Duckerror> {{\n\
             \x20   let text = dv_text(args, idx, name)?;\n\
             \x20   serde_json::from_str::<Vec<Vec<{upstream_path}>>>(text)\n\
             \x20       .map_err(|e| types::Duckerror::Invalidargument(\n\
             \x20           format!(\"{{name}}: arg {{idx}} must be JSON array of arrays of {kebab} ({{e}})\")))\n\
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
