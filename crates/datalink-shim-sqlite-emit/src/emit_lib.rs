//! Emit `src/lib.rs` for the wasm-component bridge.
//!
//! Phase 3: the dispatch registry is GENERATED at codegen time
//! by parsing the upstream postgis-wasm WIT files and joining
//! against the interface DB. Each recognised scalar produces a
//! match arm; aggregates use a per-context state map; UDTFs +
//! operators + casts route to dedicated dispatchers. Scalars
//! the codegen can't classify (option<...> params, list returns,
//! unknown types) get a stub arm with a "not in dispatcher
//! alphabet" diagnostic.
//!
//! The shape mirrors `extensions/postgis-bridge/src/lib.rs`:
//! `wit_bindgen::generate!` against `wit/world.wit`,
//! `impl MetadataGuest / ScalarFunctionGuest / AggregateGuest /
//! VtabGuest for $Bridge`. The metadata manifest reflects every
//! scalar/aggregate/UDTF the interface DB declares.

use std::collections::HashMap;

use anyhow::Result;

use shim_bridge_codegen_core::BridgePlan;
use crate::dispatch::{
    self, AggregateEntry, DispatchEntry, ParamShape, UdtfEntry, UnwiredScalar,
    WindowEntry, WindowReturn,
};
use crate::emit_wit;
use crate::vtab::{build_vtab_schema, visible_column_count};
use datalink_shim_codegen_core::force_link::render_force_link_upstream_imports;
use datalink_shim_codegen_core::record_registry::{self, RecordType};
use datalink_shim_codegen_core::wit_parse;

/// Generate `src/lib.rs`. `crate_name` is the bridge crate
/// name (e.g. "postgis-sqlink-bridge"); used only inside the
/// emitted stub error strings.
pub fn lib_rs(plan: &BridgePlan, crate_name: &str) -> Result<String> {
    let primary = plan
        .extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or("shim");

    // Bridge struct name in PascalCase.
    let bridge_struct = pascal_case(primary) + "Bridge";

    // Build the (scalar_name → func_id) lookup the dispatcher
    // needs. The id assignment must match the metadata emitter's
    // pass below — both walk plan.extensions in the same order
    // and assign scalar ids starting at 1.
    let scalar_id_for = build_scalar_id_index(plan);
    let agg_id_for = build_aggregate_id_index(plan);
    let udtf_id_for = build_udtf_id_index(plan);
    // #616 Phase 1: window functions occupy a distinct id range
    // (3_000_000+) and route through the AggregateGuest interface
    // (step/finalize/value/inverse) with `is_window: true` on the
    // manifest entry. Same demux-by-func-id pattern as aggregates.
    let window_id_for = build_window_id_index(plan);

    // Generate the dispatch registries from the WIT + interface DB.
    //
    // Phase D: discover the primary shim subdir under the deps root
    // instead of hardcoding `postgis-wasm`. The "primary shim" is
    // the package whose namespace matches the BridgePlan's primary
    // extension, or — failing that — the first non-(sqlite,
    // sfcgal, helper) subdir present.
    let wit_deps_root = emit_wit::source_shim_deps_dir(primary)?;
    let shim_packages = emit_wit::discover_shim_packages(&wit_deps_root)?;
    let shim_wit_dir = pick_primary_shim_dir(primary, &wit_deps_root, &shim_packages)
        .unwrap_or_else(|| wit_deps_root.clone());

    // Phase C: per-shim record-type registry. Every record in the
    // PRIMARY shim's WIT package becomes a `RecordType` whose
    // `type_id` (32-byte sha256) is the wit-value identity key.
    // Helper-component records (sfcgal-component types for postgis,
    // proj/dbscan/etc. for mobilitydb) skip the registry so the
    // bridge doesn't try to own their codecs.
    let records: Vec<RecordType> = record_registry::build(&shim_packages, primary)
        .into_iter()
        .filter(|r| emit_wit::package_belongs_to_primary(&r.package, primary))
        .collect();

    // Phase E: wit-bindgen's `additional_derives` adds the
    // requested derives to EVERY generated type. The contract
    // package (`sqlite:extension`) defines flag-typed records
    // (`function-flags`) and other records nested through it that
    // would fail to derive serde::Serialize / Deserialize out of
    // the box.  Helper-component packages (sfcgal, proj/dbscan,
    // ...) are likewise not part of the bridge's serde-ops
    // surface. Pass the names of those types in
    // `additional_derives_ignore` so the derive macro is skipped on
    // them and only the primary shim's records get
    // Serialize+Deserialize emitted.
    let contract_pkg = emit_wit::discover_contract_package()?;
    let mut derives_ignore: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    // Every contract-package record / variant / enum / flags type.
    for r in &contract_pkg.records {
        derives_ignore.insert(r.kebab_name.clone());
    }
    for v in &contract_pkg.variants {
        derives_ignore.insert(v.kebab_name.clone());
    }
    for e in &contract_pkg.enums {
        derives_ignore.insert(e.kebab_name.clone());
    }
    for f in &contract_pkg.flags {
        derives_ignore.insert(f.kebab_name.clone());
    }
    // Every non-primary shim package's record / variant / enum /
    // flags types.  Helper-component records aren't the bridge's
    // serde-ops responsibility (the helper components carry their
    // own codecs elsewhere); ignoring them here keeps
    // `additional_derives` strictly scoped to the primary records.
    //
    // #660: wit-bindgen matches `additional_derives_ignore` on KEBAB
    // NAME, not full path. When a helper package declares a record
    // whose name overlaps with a primary-shim record (e.g.
    // flatgeobuf-format's `bbox` vs postgis-wasm's `bbox`, or
    // flatgeobuf-format's `coordinate` vs postgis-wasm's `coord`),
    // adding the helper's name to the ignore list also suppresses
    // derives on the primary's copy and on the local serde-ops copy
    // exported by the bridge world. Skip helper records whose kebab
    // name overlaps with a primary-shim record so the local
    // serde-ops Bbox/Coord/etc still derive Serialize+Deserialize.
    let primary_record_names: std::collections::BTreeSet<String> = shim_packages
        .iter()
        .filter(|p| emit_wit::package_belongs_to_primary(&p.ns_name, primary))
        .flat_map(|p| p.records.iter().map(|r| r.kebab_name.clone()))
        .collect();
    for pkg in &shim_packages {
        if emit_wit::package_belongs_to_primary(&pkg.ns_name, primary) {
            continue;
        }
        for r in &pkg.records {
            if primary_record_names.contains(&r.kebab_name) {
                continue;
            }
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
    // Primary-shim variants + flags + enums also can't derive
    // serde out-of-box (the bitflags/variant macros don't ship
    // serde impls). The records we DO want to derive on (the ones
    // in `records`) are kept OFF the ignore list.
    for pkg in &shim_packages {
        if !emit_wit::package_belongs_to_primary(&pkg.ns_name, primary) {
            continue;
        }
        for v in &pkg.variants {
            derives_ignore.insert(v.kebab_name.clone());
        }
        for f in &pkg.flags {
            derives_ignore.insert(f.kebab_name.clone());
        }
        // Keep enums in scope — wit-bindgen's enum-generated types
        // are simple Rust enums with serde derives that work out
        // of the box. mobilitydb's `interpolation` enum (used
        // inside `tfloat-sequence`) needs serde for the record's
        // own derive to compile.
        let _ = &pkg.enums;
    }
    let derives_ignore_list: Vec<String> = derives_ignore.into_iter().collect();

    let (scalar_entries, mut scalar_unwired) =
        dispatch::build_full(plan, &shim_wit_dir, &records)?;
    let (agg_entries, mut agg_unwired) =
        dispatch::build_aggregate_registry(plan, &shim_wit_dir, &records)?;
    let (udtf_entries, mut udtf_unwired) =
        dispatch::build_udtf_registry(plan, &shim_wit_dir, &records)?;
    // #616 Phase 1: classify window functions against the same WIT
    // root the scalars/aggregates use.
    let (window_entries, mut window_unwired) =
        dispatch::build_window_registry(plan, &shim_wit_dir)?;

    // Collected diagnostics — surfaced at codegen time via stderr
    // so the maintainer can see in one place which functions
    // didn't get wired.
    let total_unwired = scalar_unwired.len()
        + agg_unwired.len()
        + udtf_unwired.len()
        + window_unwired.len();
    if total_unwired > 0 {
        eprintln!(
            "[wasm-target] {total_unwired} symbol(s) not wired (Phase 3):"
        );
        let mut all = Vec::new();
        all.append(&mut scalar_unwired);
        all.append(&mut agg_unwired);
        all.append(&mut udtf_unwired);
        all.append(&mut window_unwired);
        for UnwiredScalar { sql_name, reason } in &all {
            eprintln!("  - {sql_name}: {reason}");
        }
    }

    // Task #523: report which primary-shim records get the LOCAL→UPSTREAM
    // short-circuit emit (direct=true) vs which keep the round-trip
    // (direct=false). Lets the maintainer eyeball the structural-identity
    // heuristic's coverage at codegen time without having to grep the
    // emitted file.
    {
        let direct_count = records.iter().filter(|r| r.direct).count();
        let total = records.len();
        if total > 0 {
            eprintln!(
                "[wasm-target] wit-value codec short-circuit (#523): {}/{} \
                 record(s) decode straight into UPSTREAM (rest keep \
                 LOCAL→UPSTREAM ciborium round-trip)",
                direct_count, total,
            );
            let non_direct: Vec<&str> = records
                .iter()
                .filter(|r| !r.direct)
                .map(|r| r.kebab_name.as_str())
                .collect();
            if !non_direct.is_empty() {
                eprintln!(
                    "  - non-direct: {}",
                    non_direct.join(", "),
                );
            }
        }
    }

    // Track which WIT module aliases are actually used by the
    // emitted match arms so we only `use` what we need (otherwise
    // wit-bindgen drops unused interfaces from the world and the
    // `use` statements fail to resolve).
    //
    // Phase D: the aliases are owned `String` values rather than
    // `&'static str`. We also need to remember which WIT package
    // each alias belongs to so the `use` line points at the right
    // `bindings::<ns>::<name>::<module>` path.
    let mut used_aliases: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for (entry, _fallible) in &scalar_entries {
        used_aliases
            .entry(entry.shape.wit_module.clone())
            .or_insert_with(|| entry.shape.wit_package.clone());
        // Round 3: some return shapes compose with other
        // interfaces' helpers — record those aliases too. Both
        // helpers live in postgis:wasm.
        match &entry.shape.ret {
            dispatch::RetShape::BboxBlob => {
                used_aliases
                    .entry("pg_ctor".to_string())
                    .or_insert_with(|| "postgis:wasm".to_string());
            }
            dispatch::RetShape::IsValidDetailText => {
                used_aliases
                    .entry("pg_out".to_string())
                    .or_insert_with(|| "postgis:wasm".to_string());
            }
            // W3.3 (#543): enum returns reference the enum's
            // defining interface alias (e.g. `pg_rast_types` for
            // `pixel-type` even when the scalar lives in
            // `postgis-raster-accessors`/`pg_rast_acc`). Make sure
            // the alias's `use` line gets emitted.
            dispatch::RetShape::Enum {
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
        // W3.3 (#543): enum-typed params likewise reference their
        // declaring interface — register every enum param's alias.
        for p in &entry.shape.params {
            if let dispatch::ParamShape::Enum {
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
    for a in &agg_entries {
        used_aliases
            .entry(a.shape.wit_module.clone())
            .or_insert_with(|| a.shape.wit_package.clone());
    }
    for u in &udtf_entries {
        used_aliases
            .entry(u.shape.wit_module.clone())
            .or_insert_with(|| u.shape.wit_package.clone());
    }
    for w in &window_entries {
        used_aliases
            .entry(w.shape.wit_module.clone())
            .or_insert_with(|| w.shape.wit_package.clone());
    }

    let mut s = String::new();
    s.push_str(HEADER);
    s.push_str(&format!(
        r##"//! Generated by sqlink-shim-codegen (Phase 3).
//!
//! Bridges {primary} onto sqlite:extension/* as a wasm
//! component. Scalars / aggregates / UDTFs are dispatched by a
//! match arm generated for each interface-DB entry whose WIT
//! signature the codegen recognises; everything else returns the
//! Phase 3 stub error so the surface stays loadable.

#![allow(unused_imports, dead_code)]

extern crate alloc;

use alloc::format;
use alloc::string::{{String, ToString}};
use alloc::vec::Vec;
use core::cell::RefCell;
use std::collections::HashMap;

mod bindings {{
    wit_bindgen::generate!({{
        path: "wit",
        world: "bridge",
        generate_all,
        // Phase E: derive serde::Serialize + serde::Deserialize on
        // generated record types so ciborium can ferry them
        // through the `canon:cbor` profile. The codec bodies
        // emitted on SerdeOpsGuest below call
        // ciborium::ser::into_writer + ciborium::de::from_reader
        // on the wit-bindgen-generated types directly.
        additional_derives: [serde::Serialize, serde::Deserialize],
        // Types that can't (or shouldn't) derive serde:
        //   - sqlite:extension/* contract types (records like
        //     Manifest reference flags/variant types that don't
        //     ship serde impls).
        //   - Helper-component records (sfcgal, proj/dbscan, ...)
        //     aren't the bridge's serde-ops surface.
        //   - Primary-shim variants + flags (postgis-error etc.)
        //     don't auto-derive serde.
        additional_derives_ignore: [
{derives_ignore_lits}        ],
    }});
}}

use bindings::exports::sqlite::extension::aggregate_function::Guest as AggregateGuest;
use bindings::exports::sqlite::extension::metadata::{{
    AggregateFunctionSpec, Guest as MetadataGuest, Manifest, ScalarFunctionSpec, VtabSpec,
}};
use bindings::exports::sqlite::extension::scalar_function::Guest as ScalarFunctionGuest;
use bindings::exports::sqlite::extension::vtab::{{
    ConstraintUsage, Guest as VtabGuest, IndexInfo, IndexPlan, VtabRow,
}};
use bindings::sqlite::extension::types::{{FunctionFlags, SqlValue}};

// Imported `postgis:wasm/*` interfaces. Aliased to match the
// hand-written bridge's naming so emitted dispatch arms look the
// same shape. Only aliases referenced by emitted arms appear here
// so unused-import warnings stay clean.
"##,
        primary = primary,
        derives_ignore_lits = derives_ignore_list
            .iter()
            .map(|n| format!("            \"{n}\",\n"))
            .collect::<Vec<_>>()
            .join(""),
    ));

    // Emit the `use bindings::<pkg_ns>::<pkg_name>::<module> as <alias>;`
    // lines for every alias the dispatch arms actually reference.
    // Phase D: the package namespace + name come from the
    // `wit_package` field on each shape so non-postgis shims (e.g.
    // mobilitydb:temporal) route through their own bindings root.
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
    // Phase D3/D4: postgis resource types + error variant are emitted
    // only when the shim's WIT actually declares them. For non-postgis
    // shims (mobilitydb etc.) this `use` line is skipped along with
    // the `from_wkb` / `geog_from_wkb` / `postgis_err_string` helpers.
    //
    // #660: pick the PRIMARY shim package (postgis-wasm for postgis,
    // mobilitydb-temporal for mobilitydb, ...) rather than the first
    // non-contract package alphabetically. The latter would land on
    // a helper package like `flatgeobuf-format` whose WIT has no
    // `resource geometry`, leaving the resource flags false and the
    // helpers/use lines unemitted — the source of the 895 build errors
    // on regenerated postgis-sqlink-bridge.
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
    // Round-490: surface raster + topology resources / error variants
    // exactly the same way. Each is gated on the WIT actually
    // declaring `resource raster` / `resource topology` + the
    // corresponding error variant so non-postgis shims stay slim.
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

    // Round-490: compose the helper-prelude body from the per-resource
    // flags computed above. Each is independent: a postgis bridge
    // gets all three (geometry + raster + topology); a raster-only
    // shim would get just the raster helpers; etc.
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

    // W2 Phase 2 mop-up (#555): emit one `parse_json_list_tuple_<sig>`
    // helper per unique tuple-element signature referenced by a wired
    // entry. Today only `i32_i32` shows up (mobilitydb's datespanset
    // scalars).
    let tuple_list_helpers = render_tuple_list_helpers(
        &collect_tuple_list_sigs(&scalar_entries, &agg_entries, &udtf_entries),
    );

    // #607 Phase 1 + #614: only emit the witvalue-typed aggregator
    // state (thread-local Vec<WitValuePayload> + push/take helpers)
    // when there's at least one record-input aggregate (Record or
    // RecordToScalar) in the wired set. Postgis bridges (no record-
    // typed aggregates today) skip this block entirely — byte-
    // identical to the pre-#607 output.
    let has_record_agg = agg_entries.iter().any(|e| {
        matches!(
            e.shape.accumulator_kind,
            dispatch::AccKind::Record { .. }
                | dispatch::AccKind::RecordToScalar { .. }
                | dispatch::AccKind::RecordToTuple { .. }
        )
    });
    // #616 Phase 1: only emit window-function state machinery
    // (per-context buffered rows + per-window-id compute helpers)
    // when there's at least one wired window function. Mobilitydb
    // (0 windows) skips this block — byte-identical to pre-#616.
    let has_window = !window_entries.is_empty();
    let (window_state_decl, window_state_helpers, window_compute_helpers) =
        if has_window {
            (
                concat!(
                    "\n    /// #616 Phase 1: per-context buffered partition rows\n",
                    "    /// for window functions. Step() pushes each row's args;\n",
                    "    /// value() lazily computes on first call (caching the\n",
                    "    /// per-row result list) then walks a per-context cursor\n",
                    "    /// emitting labels[cursor] per row. inverse() no-ops\n",
                    "    /// (whole-partition compute is frame-invariant — DD2 in\n",
                    "    /// PLAN-window-substrate.md). finalize() drops state.\n",
                    "    static WINDOW_STATE: RefCell<HashMap<u64, WindowContext>> =\n",
                    "        RefCell::new(HashMap::new());"
                )
                .to_string(),
                concat!(
                    "\n/// #616 Phase 1: per-window-context state. `rows` holds\n",
                    "/// each row's incoming SqlValue args verbatim; `results`\n",
                    "/// caches the upstream cluster function's per-row output\n",
                    "/// after the first value() call; `cursor` walks one-per-\n",
                    "/// value() emission.\n",
                    "#[derive(Default)]\n",
                    "struct WindowContext {\n",
                    "    rows: Vec<Vec<SqlValue>>,\n",
                    "    results: Option<Vec<SqlValue>>,\n",
                    "    cursor: usize,\n",
                    "}\n\n",
                    "fn push_window_row(context_id: u64, args: Vec<SqlValue>) {\n",
                    "    WINDOW_STATE.with(|m| {\n",
                    "        m.borrow_mut()\n",
                    "            .entry(context_id)\n",
                    "            .or_default()\n",
                    "            .rows\n",
                    "            .push(args);\n",
                    "    });\n",
                    "}\n\n",
                    "/// Pull the buffered partition rows from the per-context\n",
                    "/// state without removing the entry (`value()` keeps reading\n",
                    "/// `results`; `finalize()` drops the whole entry).\n",
                    "fn drain_window_rows(context_id: u64) -> Vec<Vec<SqlValue>> {\n",
                    "    WINDOW_STATE.with(|m| {\n",
                    "        let mut g = m.borrow_mut();\n",
                    "        g.get_mut(&context_id)\n",
                    "            .map(|c| core::mem::take(&mut c.rows))\n",
                    "            .unwrap_or_default()\n",
                    "    })\n",
                    "}\n\n",
                    "/// Cache the computed per-row results on the per-context\n",
                    "/// state (first value() call writes; subsequent reads).\n",
                    "fn set_window_results(context_id: u64, results: Vec<SqlValue>) {\n",
                    "    WINDOW_STATE.with(|m| {\n",
                    "        let mut g = m.borrow_mut();\n",
                    "        let entry = g.entry(context_id).or_default();\n",
                    "        entry.results = Some(results);\n",
                    "    });\n",
                    "}\n\n",
                    "/// Read the cached per-row results without consuming. Returns\n",
                    "/// `None` when the cache hasn't been populated yet (caller\n",
                    "/// recomputes via the per-func helper).\n",
                    "fn window_results_cached(context_id: u64) -> Option<Vec<SqlValue>> {\n",
                    "    WINDOW_STATE.with(|m| {\n",
                    "        m.borrow().get(&context_id)\n",
                    "            .and_then(|c| c.results.clone())\n",
                    "    })\n",
                    "}\n\n",
                    "/// Advance the per-context cursor and return the previous\n",
                    "/// value (SQLite walks `value()` left-to-right per the\n",
                    "/// window-function ABI; the cursor counts rows already\n",
                    "/// emitted from the partition).\n",
                    "fn bump_window_cursor(context_id: u64) -> usize {\n",
                    "    WINDOW_STATE.with(|m| {\n",
                    "        let mut g = m.borrow_mut();\n",
                    "        let entry = g.entry(context_id).or_default();\n",
                    "        let i = entry.cursor;\n",
                    "        entry.cursor += 1;\n",
                    "        i\n",
                    "    })\n",
                    "}\n\n",
                    "fn drop_window_state(context_id: u64) {\n",
                    "    WINDOW_STATE.with(|m| {\n",
                    "        m.borrow_mut().remove(&context_id);\n",
                    "    });\n",
                    "}\n"
                )
                .to_string(),
                emit_window_compute_helpers(&window_entries),
            )
        } else {
            (String::new(), String::new(), String::new())
        };

    let (witvalue_state_decl, witvalue_state_helpers) = if has_record_agg {
        (
            "\n    /// #607 Phase 1: parallel accumulator for record-typed\n\
             \x20   /// aggregates (mobilitydb temporal-type aggregates).  Holds the\n\
             \x20   /// per-row `WitValuePayload` (canon-CBOR bytes + symbolic +\n\
             \x20   /// type-id); finalize iterates and decodes each via the per-\n\
             \x20   /// record codec helper before invoking the upstream aggregator.\n\
             \x20   static AGG_WITVALUE_STATE: RefCell<HashMap<u64, Vec<bindings::sqlite::extension::types::WitValuePayload>>> =\n\
             \x20       RefCell::new(HashMap::new());"
                .to_string(),
            "\n/// #607 Phase 1: record-typed aggregate accumulator push.\n\
             /// Mirrors `push_geom_state` / `push_raster_state` but stores\n\
             /// the per-row `WitValuePayload` (CBOR bytes + type id) rather\n\
             /// than a raw blob, since record-typed aggregates need the\n\
             /// per-record codec at finalize time rather than `from_wkb` /\n\
             /// `from_raster_binary`.\n\
             fn push_witvalue_state(context_id: u64, payload: bindings::sqlite::extension::types::WitValuePayload) {\n\
             \x20   AGG_WITVALUE_STATE.with(|m| {\n\
             \x20       m.borrow_mut().entry(context_id).or_default().push(payload);\n\
             \x20   });\n\
             }\n\n\
             fn take_witvalue_state(context_id: u64) -> Vec<bindings::sqlite::extension::types::WitValuePayload> {\n\
             \x20   AGG_WITVALUE_STATE.with(|m| {\n\
             \x20       m.borrow_mut().remove(&context_id).unwrap_or_default()\n\
             \x20   })\n\
             }\n"
                .to_string(),
        )
    } else {
        (String::new(), String::new())
    };

    // #557fix W4a composition fix: emit a force-link block that
    // references every upstream function in every primary-shim
    // imported interface as a `*const ()`. Without this,
    // wit-component (run as part of wasm32-wasip2 lower) prunes
    // imported functions the bridge never calls — producing a
    // TRIMMED instance import shape. `wac plug` 0.10's structural
    // match then fails to satisfy that trimmed import from the
    // upstream's full export shape: the subtype check at the
    // graph level passes (plug exports ⊇ socket imports) but the
    // encoded composition leaves the socket's import slot open.
    // Force-linking every upstream function makes the bridge's
    // import shape equal to the upstream's export shape, and the
    // composition closes cleanly.
    let force_link_block = render_force_link_upstream_imports(
        primary,
        &shim_packages,
        &shim_wit_dir,
    )?;

    s.push_str(&format!(
        r##"const BRIDGE_NAME: &str = "{crate_name}";

/// Marker for functions not yet covered by Phase 3's dispatch
/// registry. The host's caller surfaces this back to the user
/// verbatim, so the work item ("widen registry") is
/// self-documenting from the error string.
fn stubbed(kind: &str, func_id: u64) -> String {{
    format!(
        "{{BRIDGE_NAME}}: {{kind}} func_id={{func_id}} is stubbed (no WIT signature classification)",
        BRIDGE_NAME = BRIDGE_NAME,
        kind = kind,
        func_id = func_id,
    )
}}

// ── Argument-unpack helpers ──
//
// Mirror the hand-written `extensions/postgis-bridge/src/lib.rs`
// versions so dispatch arms emitted here look identical to the
// oracle. Each returns `Err(String)` shaped like
// "{{name}}: arg {{idx}} must be TYPE" so the SQL-side error
// surface is consistent.

fn arg_text<'a>(args: &'a [SqlValue], idx: usize, name: &str) -> Result<&'a str, String> {{
    match args.get(idx) {{
        Some(SqlValue::Text(s)) => Ok(s.as_str()),
        _ => Err(format!("{{name}}: arg {{idx}} must be TEXT")),
    }}
}}

fn arg_blob<'a>(args: &'a [SqlValue], idx: usize, name: &str) -> Result<&'a [u8], String> {{
    match args.get(idx) {{
        Some(SqlValue::Blob(b)) => Ok(b.as_slice()),
        // Geometry that arrived as text is accepted as raw bytes
        // for parity with the hand-written bridge.
        Some(SqlValue::Text(s)) => Ok(s.as_bytes()),
        _ => Err(format!("{{name}}: arg {{idx}} must be BLOB")),
    }}
}}

fn arg_f64(args: &[SqlValue], idx: usize, name: &str) -> Result<f64, String> {{
    match args.get(idx) {{
        Some(SqlValue::Real(r)) => Ok(*r),
        Some(SqlValue::Integer(i)) => Ok(*i as f64),
        _ => Err(format!("{{name}}: arg {{idx}} must be REAL")),
    }}
}}

fn arg_i64(args: &[SqlValue], idx: usize, name: &str) -> Result<i64, String> {{
    match args.get(idx) {{
        Some(SqlValue::Integer(i)) => Ok(*i),
        Some(SqlValue::Real(r)) => Ok(*r as i64),
        _ => Err(format!("{{name}}: arg {{idx}} must be INTEGER")),
    }}
}}

/// Phase E generic error-formatter for fallible WIT-side calls.
/// The dispatch arm's `.map_err(|e| format!(\"{{name}}: {{}}\", shim_err_string(e)))?`
/// chain uses this for every shim regardless of error-type shape
/// (postgis-error, temporal-error, ...). Per-shim pretty-printers
/// (`postgis_err_string` for postgis) can still be called by
/// hand-written helpers; the dispatch arms uniformly use the
/// Debug-based formatter so codegen stays shim-agnostic.
#[allow(dead_code)]
fn shim_err_string<E: core::fmt::Debug>(e: E) -> String {{
    format!("{{:?}}", e)
}}

// ── W2 (#542): primitive `list<X>` param helpers ──
//
// Each `ParamShape::ListPrim(elem)` dispatch arm calls one of
// these `parse_json_list_<T>` helpers. The SQL caller passes a
// JSON-array literal in the TEXT arg (e.g.
// `tfloat_at_values(seq, '[1.0, 2.0, 3.0]')`); the helper
// decodes via serde_json into a `Vec<T>` which the arm then
// passes to the WIT function as `&[T]`.
//
// Pragmatic choice over the wit-value-payload path: SQL users
// already know JSON; no per-shape codec registry is required for
// primitives. Complex-element lists (records, spans, geometry
// resource) still need the wit-value codec path — see plan
// doc W2.6 for the deferral rationale.

#[allow(dead_code)]
fn parse_json_list_f64(args: &[SqlValue], idx: usize, name: &str) -> Result<Vec<f64>, String> {{
    let text = arg_text(args, idx, name)?;
    serde_json::from_str::<Vec<f64>>(text)
        .map_err(|e| format!("{{name}}: arg {{idx}} must be JSON array of f64 ({{e}})"))
}}

#[allow(dead_code)]
fn parse_json_list_i32(args: &[SqlValue], idx: usize, name: &str) -> Result<Vec<i32>, String> {{
    let text = arg_text(args, idx, name)?;
    serde_json::from_str::<Vec<i32>>(text)
        .map_err(|e| format!("{{name}}: arg {{idx}} must be JSON array of s32 ({{e}})"))
}}

#[allow(dead_code)]
fn parse_json_list_i64(args: &[SqlValue], idx: usize, name: &str) -> Result<Vec<i64>, String> {{
    let text = arg_text(args, idx, name)?;
    serde_json::from_str::<Vec<i64>>(text)
        .map_err(|e| format!("{{name}}: arg {{idx}} must be JSON array of s64 ({{e}})"))
}}

#[allow(dead_code)]
fn parse_json_list_u32(args: &[SqlValue], idx: usize, name: &str) -> Result<Vec<u32>, String> {{
    let text = arg_text(args, idx, name)?;
    serde_json::from_str::<Vec<u32>>(text)
        .map_err(|e| format!("{{name}}: arg {{idx}} must be JSON array of u32 ({{e}})"))
}}

#[allow(dead_code)]
fn parse_json_list_u64(args: &[SqlValue], idx: usize, name: &str) -> Result<Vec<u64>, String> {{
    let text = arg_text(args, idx, name)?;
    serde_json::from_str::<Vec<u64>>(text)
        .map_err(|e| format!("{{name}}: arg {{idx}} must be JSON array of u64 ({{e}})"))
}}

#[allow(dead_code)]
fn parse_json_list_u8(args: &[SqlValue], idx: usize, name: &str) -> Result<Vec<u8>, String> {{
    let text = arg_text(args, idx, name)?;
    serde_json::from_str::<Vec<u8>>(text)
        .map_err(|e| format!("{{name}}: arg {{idx}} must be JSON array of u8 ({{e}})"))
}}

#[allow(dead_code)]
fn parse_json_list_bool(args: &[SqlValue], idx: usize, name: &str) -> Result<Vec<bool>, String> {{
    let text = arg_text(args, idx, name)?;
    serde_json::from_str::<Vec<bool>>(text)
        .map_err(|e| format!("{{name}}: arg {{idx}} must be JSON array of bool ({{e}})"))
}}

#[allow(dead_code)]
fn parse_json_list_string(args: &[SqlValue], idx: usize, name: &str) -> Result<Vec<String>, String> {{
    let text = arg_text(args, idx, name)?;
    serde_json::from_str::<Vec<String>>(text)
        .map_err(|e| format!("{{name}}: arg {{idx}} must be JSON array of string ({{e}})"))
}}
{TUPLE_LIST_HELPERS}{POSTGIS_HELPERS}{FORCE_LINK_BLOCK}

// ── Aggregate state ──
//
// Per-context map of accumulated WKB blobs. Phase 3 aggregates
// (st_union, st_collect, st_extent, ...) collect input
// geometries in the `step` call and replay them at `finalize`
// time as `list<borrow<geometry>>` to the WIT-side aggregate
// function. The wasm component is single-threaded so a
// thread_local!{{}} RefCell is the simplest correct shape.

thread_local! {{
    static AGG_STATE: RefCell<HashMap<u64, Vec<Vec<u8>>>> =
        RefCell::new(HashMap::new());
    /// #548 (W3.2): parallel accumulator for raster aggregates
    /// (`st_rast_union_aggregate`). The two maps are independent so
    /// concurrent geom + raster aggregates on the same connection
    /// don't share state.
    static AGG_RASTER_STATE: RefCell<HashMap<u64, Vec<Vec<u8>>>> =
        RefCell::new(HashMap::new());{WITVALUE_AGG_STATE_DECL}{WINDOW_STATE_DECL}
    /// Round 2: per-context constant args for aggregates that
    /// take extra params beyond the streaming geometry
    /// (`st_clusterwithin(geom, distance)` and friends).
    /// First-step writer; subsequent steps validate equality.
    static AGG_EXTRAS: RefCell<HashMap<u64, Vec<SqlValue>>> =
        RefCell::new(HashMap::new());
}}

fn push_geom_state(context_id: u64, blob: Vec<u8>) {{
    AGG_STATE.with(|m| {{
        m.borrow_mut().entry(context_id).or_default().push(blob);
    }});
}}

fn take_geom_state(context_id: u64) -> Vec<Vec<u8>> {{
    AGG_STATE.with(|m| {{
        m.borrow_mut().remove(&context_id).unwrap_or_default()
    }})
}}

fn push_raster_state(context_id: u64, blob: Vec<u8>) {{
    AGG_RASTER_STATE.with(|m| {{
        m.borrow_mut().entry(context_id).or_default().push(blob);
    }});
}}

fn take_raster_state(context_id: u64) -> Vec<Vec<u8>> {{
    AGG_RASTER_STATE.with(|m| {{
        m.borrow_mut().remove(&context_id).unwrap_or_default()
    }})
}}
{WITVALUE_AGG_STATE_HELPERS}{WINDOW_STATE_HELPERS}{WINDOW_COMPUTE_HELPERS}
/// Round 2: store-or-validate the constant extras for an
/// aggregate. On the first step we just record them; on
/// subsequent steps we validate that the constants haven't
/// changed (PostgreSQL semantics: constant args MUST be uniform
/// across rows of a single aggregate invocation).
fn set_or_validate_extras(
    context_id: u64,
    extras: Vec<SqlValue>,
    name: &str,
) -> Result<(), String> {{
    AGG_EXTRAS.with(|m| {{
        let mut m = m.borrow_mut();
        if let Some(existing) = m.get(&context_id) {{
            if !sqlvalues_eq(existing, &extras) {{
                return Err(format!(
                    "{{name}}: aggregate constant args drifted between rows",
                    name = name,
                ));
            }}
            Ok(())
        }} else {{
            m.insert(context_id, extras);
            Ok(())
        }}
    }})
}}

fn take_extras_state(context_id: u64) -> Vec<SqlValue> {{
    AGG_EXTRAS.with(|m| m.borrow_mut().remove(&context_id).unwrap_or_default())
}}

fn sqlvalues_eq(a: &[SqlValue], b: &[SqlValue]) -> bool {{
    if a.len() != b.len() {{
        return false;
    }}
    a.iter().zip(b.iter()).all(|(x, y)| match (x, y) {{
        (SqlValue::Null, SqlValue::Null) => true,
        (SqlValue::Integer(p), SqlValue::Integer(q)) => p == q,
        (SqlValue::Real(p), SqlValue::Real(q)) => p == q,
        (SqlValue::Text(p), SqlValue::Text(q)) => p == q,
        (SqlValue::Blob(p), SqlValue::Blob(q)) => p == q,
        _ => false,
    }})
}}

// ── UDTF state ──
//
// Per-cursor materialised rowset. UDTFs materialise the full row
// list at `filter` time and stream it out via `next` / `column`.
// Task #532: each row is a per-column `Vec<SqlValue>` so record-row
// UDTFs (mobilitydb temporal-join-float etc.) can return one
// SqlValue per visible column. Single-column UDTFs (postgis
// st_dump and friends) emit a 1-element Vec wrapping the row's
// single SqlValue::Blob, so the SingleGeom path stays uniform.

thread_local! {{
    static UDTF_STATE: RefCell<HashMap<u64, UdtfCursor>> =
        RefCell::new(HashMap::new());
}}

struct UdtfCursor {{
    rows: Vec<Vec<SqlValue>>,
    idx: usize,
}}

struct {bridge_struct};

"##,
        crate_name = crate_name,
        bridge_struct = bridge_struct,
        POSTGIS_HELPERS = helpers_block.as_str(),
        TUPLE_LIST_HELPERS = tuple_list_helpers.as_str(),
        FORCE_LINK_BLOCK = force_link_block.as_str(),
        WITVALUE_AGG_STATE_DECL = witvalue_state_decl.as_str(),
        WITVALUE_AGG_STATE_HELPERS = witvalue_state_helpers.as_str(),
        WINDOW_STATE_DECL = window_state_decl.as_str(),
        WINDOW_STATE_HELPERS = window_state_helpers.as_str(),
        WINDOW_COMPUTE_HELPERS = window_compute_helpers.as_str(),
    ));

    // MetadataGuest impl — reflect every scalar / aggregate / UDTF
    // from the interface DB into the manifest. Phase C: also reflect
    // every record-type discovered in the primary shim's WIT into
    // `typed_values` so the host's per-extension registry indexes
    // them at load time. #616: window functions ride the
    // `aggregate_functions` list with `is_window: true`.
    s.push_str(&emit_metadata_impl(plan, &bridge_struct, &records, &window_entries));

    // Phase E: per-record wit-value marshaling helpers. Each
    // record gets an `arg_witvalue_<snake>` decoder + a
    // `ret_to_witvalue_<snake>` encoder; the dispatch arms emitted
    // in `emit_scalar_impl` reference these by name.
    //
    // Restrict emission to the records ACTUALLY referenced by a
    // dispatch arm. wit-bindgen elides unused imported types from
    // its generated bindings, so emitting a helper that references
    // an unused upstream record would fail to compile (e.g.,
    // postgis's `CoordZ` — declared in WIT, never referenced by a
    // function, so wit-bindgen drops it from
    // `bindings::postgis::wasm::postgis_types`).
    let referenced_records: std::collections::BTreeSet<String> =
        collect_referenced_records(&scalar_entries, &agg_entries, &udtf_entries);
    let helper_records: Vec<RecordType> = records
        .iter()
        .filter(|r| referenced_records.contains(&r.kebab_name))
        .cloned()
        .collect();
    if !helper_records.is_empty() {
        s.push_str(&emit_wit_value_helpers(primary, &bridge_struct, &helper_records));
    }

    // ScalarFunctionGuest impl — dispatches every wired scalar by
    // func-id; unwired scalars fall through to the stub error.
    s.push_str(&emit_scalar_impl(
        &bridge_struct,
        &scalar_entries,
        &scalar_id_for,
    ));

    // AggregateGuest impl — wires step/finalize for every wired
    // aggregate; unwired aggregates fall through to stub errors.
    // #616: window functions ride the same AggregateGuest interface
    // (step buffers rows, value computes-then-cursor, inverse no-ops,
    // finalize drops state) — `is_window: true` on the manifest tells
    // the host to register via `sqlite3_create_window_function`.
    s.push_str(&emit_aggregate_impl(
        &bridge_struct,
        &agg_entries,
        &agg_id_for,
        &window_entries,
        &window_id_for,
    ));

    // VtabGuest impl — every wired UDTF gets a per-vtab arm in
    // the relevant lifecycle methods; everything else is stubbed.
    s.push_str(&emit_vtab_impl(
        &bridge_struct,
        &udtf_entries,
        &udtf_id_for,
    ));

    // Phase E: serde-ops impl — per-record canon-cbor codec bodies.
    // wac 0.10.1 (PR #205) lifted the 0.10.0 limitation that
    // prevented exporting an interface that re-uses types from
    // satisfied imports, so the bridge can now ship a real
    // SerdeOpsGuest. Bodies are ciborium-based: the
    // wit-bindgen-generated record types carry
    // `serde::Serialize + Deserialize` derives via the
    // `additional_derives` arg above, so encode is
    // `ciborium::ser::into_writer(&value, &mut buf)` and decode is
    // `ciborium::de::from_reader(bytes.as_slice())`.
    if !records.is_empty() {
        s.push_str(&emit_serde_ops_impl(primary, &bridge_struct, &records));
    }

    s.push_str(&format!(
        "bindings::export!({bridge_struct} with_types_in bindings);\n"
    ));

    Ok(s)
}

/// Emit `impl ScalarFunctionGuest for $Bridge`.
fn emit_scalar_impl(
    bridge_struct: &str,
    scalar_entries: &[(DispatchEntry, bool)],
    scalar_id_for: &HashMap<&str, u64>,
) -> String {
    let mut arms = String::new();
    let mut seen_ids = std::collections::HashSet::new();
    for (entry, fallible) in scalar_entries {
        let Some(&id) = scalar_id_for.get(entry.sql_name.as_str()) else {
            continue;
        };
        if !seen_ids.insert(id) {
            // Same func-id can show up twice if canonical+alias
            // share a Result wrap; first writer wins.
            continue;
        }
        let body = dispatch::emit_arm_body(
            &entry.shape,
            *fallible,
            &entry.sql_name,
            "                ",
        );
        arms.push_str(&format!(
            "            {id} => {{\n{body}\n            }}\n",
        ));
    }

    format!(
        r##"
impl ScalarFunctionGuest for {bridge_struct} {{
    fn call(func_id: u64, args: Vec<SqlValue>) -> Result<SqlValue, String> {{
        // SQL-style null propagation. Functions whose NULL inputs
        // are MEANINGFUL would need to opt out; PostGIS scalars
        // are uniformly null-propagating in practice.
        if args.iter().any(|v| matches!(v, SqlValue::Null)) {{
            return Ok(SqlValue::Null);
        }}
        match func_id {{
{arms}            _ => Err(stubbed("scalar-function", func_id)),
        }}
    }}
}}

"##,
    )
}

/// Emit `impl AggregateGuest for $Bridge`. #616: extended to
/// dispatch window-function arms on the same interface — step
/// buffers each row's args into `WINDOW_STATE`, value lazily
/// computes the upstream cluster function on first call (caching
/// `Vec<SqlValue>` results) then advances a per-context cursor to
/// emit the next label, inverse no-ops (whole-partition compute
/// is frame-invariant), finalize drops the per-context entry.
fn emit_aggregate_impl(
    bridge_struct: &str,
    agg_entries: &[AggregateEntry],
    agg_id_for: &HashMap<&str, u64>,
    window_entries: &[WindowEntry],
    window_id_for: &HashMap<&str, u64>,
) -> String {
    let mut step_arms = String::new();
    let mut final_arms = String::new();
    let mut value_arms = String::new();
    let mut inverse_arms = String::new();
    let mut seen_step = std::collections::HashSet::new();
    let mut seen_final = std::collections::HashSet::new();
    let mut seen_value = std::collections::HashSet::new();
    let mut seen_inverse = std::collections::HashSet::new();

    for entry in agg_entries {
        // Phase 1A: AggregateEntry now carries one canonical sql_name
        // and an inline `aliases` Vec (rather than each alias being a
        // distinct entry). Iterate canonical + each alias here so each
        // SQL name still gets its own dispatch arm keyed by func_id —
        // byte-identical to the pre-Phase-1A per-entry walk.
        for name in std::iter::once(entry.sql_name.as_str())
            .chain(entry.aliases.iter().map(|s| s.as_str()))
        {
            let Some(&id) = agg_id_for.get(name) else {
                continue;
            };
            if seen_step.insert(id) {
                let step_body = dispatch::emit_aggregate_step_body(
                    &entry.shape,
                    name,
                    "                ",
                );
                step_arms.push_str(&format!(
                    "            {id} => {{\n{step_body}\n            }}\n",
                ));
            }
            if seen_final.insert(id) {
                let final_body = dispatch::emit_aggregate_finalize_body(
                    &entry.shape,
                    name,
                    "                ",
                );
                final_arms.push_str(&format!(
                    "            {id} => {{\n{final_body}\n            }}\n",
                ));
            }
        }
    }

    // #616 Phase 1: window-function arms across all 4 of step / value
    // / inverse / finalize. The step buffers args verbatim into
    // WINDOW_STATE; value() routes per func_id to the right per-window
    // compute helper (emitted in the prelude); inverse() no-ops;
    // finalize() drops the per-context state.
    for entry in window_entries {
        let Some(&id) = window_id_for.get(entry.sql_name.as_str()) else {
            continue;
        };
        if seen_step.insert(id) {
            step_arms.push_str(&format!(
                "            {id} => {{\n                push_window_row(context_id, args);\n                Ok(())\n            }}\n",
            ));
        }
        if seen_value.insert(id) {
            let compute_fn = window_compute_fn_name(id);
            value_arms.push_str(&format!(
                "            {id} => {{\n\
                 \x20               // First value() call materialises the cached results;\n\
                 \x20               // subsequent calls read from cache and bump cursor.\n\
                 \x20               if window_results_cached(context_id).is_none() {{\n\
                 \x20                   let rows = drain_window_rows(context_id);\n\
                 \x20                   let results = {compute_fn}(&rows)?;\n\
                 \x20                   set_window_results(context_id, results);\n\
                 \x20               }}\n\
                 \x20               let i = bump_window_cursor(context_id);\n\
                 \x20               let cached = window_results_cached(context_id).unwrap_or_default();\n\
                 \x20               Ok(cached.get(i).cloned().unwrap_or(SqlValue::Null))\n\
                 \x20           }}\n",
            ));
        }
        if seen_inverse.insert(id) {
            inverse_arms.push_str(&format!(
                "            {id} => {{ Ok(()) }} // whole-partition compute: inverse is a no-op\n",
            ));
        }
        if seen_final.insert(id) {
            final_arms.push_str(&format!(
                "            {id} => {{\n                drop_window_state(context_id);\n                Ok(SqlValue::Null)\n            }}\n",
            ));
        }
    }

    format!(
        r##"impl AggregateGuest for {bridge_struct} {{
    fn step(func_id: u64, context_id: u64, args: Vec<SqlValue>) -> Result<(), String> {{
        // Null-row contributions are skipped (SQL aggregate semantics).
        if args.iter().any(|v| matches!(v, SqlValue::Null)) {{
            return Ok(());
        }}
        match func_id {{
{step_arms}            _ => Err(stubbed("aggregate-function step", func_id)),
        }}
    }}
    fn finalize(func_id: u64, context_id: u64) -> Result<SqlValue, String> {{
        match func_id {{
{final_arms}            _ => Err(stubbed("aggregate-function finalize", func_id)),
        }}
    }}
    fn value(func_id: u64, context_id: u64) -> Result<SqlValue, String> {{
        // Bind to a `_` shadow so this compiles warning-clean when
        // no window arms reference context_id (mobilitydb has 0
        // window functions; the match arm list is empty).
        let _ = context_id;
        match func_id {{
{value_arms}            _ => Err(stubbed("aggregate-function value (window mode not wired)", func_id)),
        }}
    }}
    fn inverse(func_id: u64, context_id: u64, _args: Vec<SqlValue>) -> Result<(), String> {{
        // Whole-partition window functions (DD2 in
        // PLAN-window-substrate.md) are frame-invariant; inverse is
        // a no-op. Non-window aggregates never reach this path
        // (sqlite only calls inverse on window-mode aggregates).
        let _ = context_id;
        match func_id {{
{inverse_arms}            _ => Err(stubbed("aggregate-function inverse (window mode not wired)", func_id)),
        }}
    }}
}}

"##
    )
}

/// #616: per-window-id compute function name.  One Rust function
/// per window-function dispatch id; the value() arm calls it on
/// first invocation to materialise the cached `Vec<SqlValue>`
/// results.
fn window_compute_fn_name(id: u64) -> String {
    format!("compute_window_{id}")
}

/// #616 Phase 1: emit one Rust helper `compute_window_<id>(rows)`
/// per wired window function. Each helper:
///   1. Walks rows, decodes the first column (geometry blob -> WKB
///      -> Geometry resource) — postgis-clustering shape.
///   2. Picks extras from `rows[0]` (SQL window constants are
///      uniform across the partition).
///   3. Builds the `Vec<&Geometry>` borrow slice.
///   4. Calls the upstream cluster function.
///   5. Marshals each per-row Y back to a `SqlValue` per the
///      classified `WindowReturn` shape.
fn emit_window_compute_helpers(entries: &[WindowEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut emitted: std::collections::HashSet<u64> =
        std::collections::HashSet::new();
    let mut window_id_for: HashMap<&str, u64> = HashMap::new();
    {
        // Reconstruct id-for map locally (mirrors build_window_id_index).
        let mut id: u64 = 3_000_000;
        for e in entries {
            window_id_for.entry(e.sql_name.as_str()).or_insert_with(|| {
                let i = id;
                id += 1;
                i
            });
        }
    }
    let mut out = String::new();
    out.push('\n');
    for entry in entries {
        let Some(&id) = window_id_for.get(entry.sql_name.as_str()) else {
            continue;
        };
        if !emitted.insert(id) {
            continue;
        }
        let name = &entry.sql_name;
        let shape = &entry.shape;
        let module = &shape.wit_module;
        let func = &shape.wit_func;

        // Build extras decode + the call's extra-arg slot.
        let mut extras_decode = String::new();
        let mut call_extras: Vec<String> = Vec::new();
        for (j, p) in shape.extra_args.iter().enumerate() {
            let arg_index = j + 1; // arg 0 is the geometry
            let (decode_line, var_expr) = match p {
                ParamShape::F64 => (
                    format!(
                        "    let extra{j} = arg_f64(&rows[0], {arg_index}, \"{name}\")?;\n"
                    ),
                    format!("extra{j}"),
                ),
                ParamShape::S32 => (
                    format!(
                        "    let extra{j} = arg_i64(&rows[0], {arg_index}, \"{name}\")? as i32;\n"
                    ),
                    format!("extra{j}"),
                ),
                ParamShape::S64 => (
                    format!(
                        "    let extra{j} = arg_i64(&rows[0], {arg_index}, \"{name}\")?;\n"
                    ),
                    format!("extra{j}"),
                ),
                ParamShape::U32 => (
                    format!(
                        "    let extra{j} = arg_i64(&rows[0], {arg_index}, \"{name}\")? as u32;\n"
                    ),
                    format!("extra{j}"),
                ),
                ParamShape::U64 => (
                    format!(
                        "    let extra{j} = arg_i64(&rows[0], {arg_index}, \"{name}\")? as u64;\n"
                    ),
                    format!("extra{j}"),
                ),
                ParamShape::Bool => (
                    format!(
                        "    let extra{j} = arg_i64(&rows[0], {arg_index}, \"{name}\")? != 0;\n"
                    ),
                    format!("extra{j}"),
                ),
                ParamShape::Text => (
                    format!(
                        "    let extra{j} = arg_text(&rows[0], {arg_index}, \"{name}\")?.to_string();\n"
                    ),
                    format!("&extra{j}"),
                ),
                ParamShape::Blob => (
                    format!(
                        "    let extra{j} = arg_blob(&rows[0], {arg_index}, \"{name}\")?.to_vec();\n"
                    ),
                    format!("&extra{j}"),
                ),
                other => {
                    // Defensive: classifier rejects fancier extras
                    // up front, but if a new shape sneaks through
                    // we fail the codegen with a visible error
                    // rather than silently misemitting.
                    return format!(
                        "// ERROR: window {name} has unsupported extra arg shape {other:?}\n"
                    );
                }
            };
            extras_decode.push_str(&decode_line);
            call_extras.push(var_expr);
        }

        let _ = &call_extras;
        let call_extras_lit = if call_extras.is_empty() {
            String::new()
        } else {
            format!(", {}", call_extras.join(", "))
        };

        // Upstream fallibility -> map_err wrap.
        let map_err = if shape.fallible {
            format!(
                ".map_err(|e| format!(\"{name}: {{}}\", postgis_err_string(e)))?"
            )
        } else {
            String::new()
        };

        // Per-row return -> SqlValue marshaling.
        let row_to_sqlvalue = match &shape.returns {
            WindowReturn::OptionU32 => concat!(
                "    Ok(labels\n",
                "        .into_iter()\n",
                "        .map(|opt| match opt {\n",
                "            Some(id) => SqlValue::Integer(id as i64),\n",
                "            None => SqlValue::Null,\n",
                "        })\n",
                "        .collect())\n",
            )
            .to_string(),
            WindowReturn::U32 => concat!(
                "    Ok(labels\n",
                "        .into_iter()\n",
                "        .map(|id| SqlValue::Integer(id as i64))\n",
                "        .collect())\n",
            )
            .to_string(),
            WindowReturn::GeomBlob => concat!(
                "    Ok(labels\n",
                "        .into_iter()\n",
                "        .map(|g| SqlValue::Blob(g.as_wkb()))\n",
                "        .collect())\n",
            )
            .to_string(),
        };

        let returns_dbg = format!("{:?}", shape.returns);
        out.push_str(&format!(
            "/// #616 Phase 1: per-partition compute for `{name}`\n\
             /// (func_id {id}). The upstream returns the full per-row\n\
             /// result list in input order; we marshal each into the\n\
             /// classified SqlValue shape ({returns_dbg}). Called once\n\
             /// per partition (first value() invocation); subsequent\n\
             /// value() calls read from cache.\n\
             fn {compute_fn}(rows: &[Vec<SqlValue>]) -> Result<Vec<SqlValue>, String> {{\n\
             \x20   if rows.is_empty() {{\n\
             \x20       return Ok(Vec::new());\n\
             \x20   }}\n\
             \x20   let mut geoms: Vec<Geometry> = Vec::with_capacity(rows.len());\n\
             \x20   for row in rows {{\n\
             \x20       let bytes = arg_blob(row, 0, \"{name}\")?;\n\
             \x20       geoms.push(\n\
             \x20           Geometry::from_wkb(bytes)\n\
             \x20               .map_err(|e| format!(\"{name}: row decode: {{}}\", postgis_err_string(e)))?,\n\
             \x20       );\n\
             \x20   }}\n\
             {extras}\
             \x20   let geom_refs: Vec<&Geometry> = geoms.iter().collect();\n\
             \x20   let labels = {module}::{func}(&geom_refs{call_extras_lit}){map_err};\n\
             {row_to_sqlvalue}\
             }}\n\n",
            compute_fn = window_compute_fn_name(id),
            extras = extras_decode,
        ));
    }
    out
}


/// Escape a string for emission as a Rust `"..."` literal.
/// We only emit ASCII identifiers + spaces / parens / commas
/// here so escaping is restricted to `\\` and `\"`.
fn rust_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Emit `impl VtabGuest for $Bridge`.
fn emit_vtab_impl(
    bridge_struct: &str,
    udtf_entries: &[UdtfEntry],
    udtf_id_for: &HashMap<&str, u64>,
) -> String {
    // Each wired UDTF maps to a vtab id. We materialise the row
    // list in `filter` and stream it row-by-row via the cursor.
    //
    // The connect path returns a CREATE TABLE schema string. For
    // PostGIS dump-style UDTFs the schema is one BLOB column
    // (the geometry). The wired set is small enough that we
    // emit one match arm per UDTF describing its schema.

    let mut connect_arms = String::new();
    let mut filter_arms = String::new();
    let mut column_arms = String::new();
    let mut seen_ids = std::collections::HashSet::new();
    for entry in udtf_entries {
        let Some(&id) = udtf_id_for.get(entry.sql_name.as_str()) else {
            continue;
        };
        if !seen_ids.insert(id) {
            continue;
        }
        // Per-vtab CREATE TABLE — output columns first (one per
        // visible row field), then HIDDEN columns (one per WIT
        // param). The HIDDEN columns let SQLite route the
        // function-call argv into xFilter; the visible columns
        // satisfy `WHERE point > ...` style references at SQL
        // parse time. Task #531.
        let schema = build_vtab_schema(&entry.sql_name, &entry.shape);
        let visible_count = visible_column_count(&entry.shape.output_row);
        connect_arms.push_str(&format!(
            "            {id} => Ok({schema_lit}.to_string()),\n",
            id = id,
            schema_lit = rust_string_literal(&schema),
        ));
        // Filter body materialises rows. For simple
        // `f(geom) -> list<geometry>` shape we marshal the
        // single param, call the function, and store WKBs.
        let filter_body = emit_udtf_filter_body(entry, "                ");
        filter_arms.push_str(&format!(
            "            {id} => {{\n{filter_body}\n            }}\n",
        ));
        // Column body. Each cursor row is a `Vec<SqlValue>` (one
        // entry per visible column, populated by filter); xColumn
        // returns the i-th entry. HIDDEN columns
        // (idx >= visible_count) surface as Null — sqlite reads
        // them via xFilter argv, not xColumn. Task #532.
        column_arms.push_str(&format!(
            "            {id} => {{\n\
             \x20               if (_col as usize) < {visible_count} {{\n\
             \x20                   Ok(row_cols.get(_col as usize).cloned().unwrap_or(SqlValue::Null))\n\
             \x20               }} else {{ Ok(SqlValue::Null) }}\n\
             \x20           }}\n",
            id = id,
            visible_count = visible_count,
        ));
    }

    format!(
        r##"impl VtabGuest for {bridge_struct} {{
    fn create(
        vtab_id: u64,
        instance_id: u64,
        db_name: String,
        table_name: String,
        args: Vec<String>,
    ) -> Result<String, String> {{
        // Eponymous UDTFs see only connect(); fall through.
        Self::connect(vtab_id, instance_id, db_name, table_name, args)
    }}
    fn connect(
        vtab_id: u64,
        _instance_id: u64,
        _db_name: String,
        _table_name: String,
        _args: Vec<String>,
    ) -> Result<String, String> {{
        match vtab_id {{
{connect_arms}            _ => Err(stubbed("vtab connect", vtab_id)),
        }}
    }}
    fn destroy(_vtab_id: u64, _instance_id: u64) -> Result<(), String> {{
        Ok(())
    }}
    fn disconnect(_vtab_id: u64, _instance_id: u64) -> Result<(), String> {{
        Ok(())
    }}
    fn best_index(
        _vtab_id: u64,
        _instance_id: u64,
        info: IndexInfo,
    ) -> Result<IndexPlan, String> {{
        // Task #531: HIDDEN-column constraints back the
        // table-valued-function arg form `f(g1, g2, ...)`. SQLite
        // assembles the SQL-side argv into xFilter's `args` array
        // in the order each usable constraint's `argv_index`
        // names; an `argv_index` of 0 means "ignore this
        // constraint" — leaving the function form unable to pass
        // its arguments through. Assign each usable EQ constraint
        // a unique 1-based slot and mark `omit: true` so SQLite
        // doesn't re-check the constraint after filter sees it.
        let mut next_argv_idx: i32 = 1;
        let constraint_usage = info.constraints.iter()
            .map(|c| {{
                if c.usable {{
                    let idx = next_argv_idx;
                    next_argv_idx += 1;
                    ConstraintUsage {{ argv_index: idx, omit: true }}
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
            estimated_rows: 1,
            orderby_consumed: false,
        }})
    }}
    fn open(_vtab_id: u64, _instance_id: u64, cursor_id: u64) -> Result<(), String> {{
        UDTF_STATE.with(|m| {{
            m.borrow_mut().insert(cursor_id, UdtfCursor {{ rows: Vec::new(), idx: 0 }});
        }});
        Ok(())
    }}
    fn close(_vtab_id: u64, cursor_id: u64) -> Result<(), String> {{
        UDTF_STATE.with(|m| {{
            m.borrow_mut().remove(&cursor_id);
        }});
        Ok(())
    }}
    fn filter(
        vtab_id: u64,
        cursor_id: u64,
        _idx_num: i32,
        _idx_str: Option<String>,
        args: Vec<SqlValue>,
    ) -> Result<(), String> {{
        match vtab_id {{
{filter_arms}            _ => Err(stubbed("vtab filter", vtab_id)),
        }}
    }}
    fn next(_vtab_id: u64, cursor_id: u64) -> Result<(), String> {{
        UDTF_STATE.with(|m| {{
            if let Some(c) = m.borrow_mut().get_mut(&cursor_id) {{
                c.idx += 1;
            }}
        }});
        Ok(())
    }}
    fn eof(_vtab_id: u64, cursor_id: u64) -> bool {{
        UDTF_STATE.with(|m| {{
            m.borrow().get(&cursor_id)
                .map(|c| c.idx >= c.rows.len())
                .unwrap_or(true)
        }})
    }}
    fn column(vtab_id: u64, cursor_id: u64, _col: i32) -> Result<SqlValue, String> {{
        let row_cols: Vec<SqlValue> = UDTF_STATE.with(|m| {{
            m.borrow().get(&cursor_id)
                .and_then(|c| c.rows.get(c.idx).cloned())
                .ok_or_else(|| stubbed("vtab column out of range", vtab_id))
        }})?;
        match vtab_id {{
{column_arms}            _ => Err(stubbed("vtab column", vtab_id)),
        }}
    }}
    fn rowid(_vtab_id: u64, cursor_id: u64) -> Result<i64, String> {{
        Ok(UDTF_STATE.with(|m| {{
            m.borrow().get(&cursor_id).map(|c| c.idx as i64).unwrap_or(0)
        }}))
    }}
    fn fetch_batch(
        vtab_id: u64,
        _cursor_id: u64,
        _max_rows: u32,
    ) -> Result<Vec<VtabRow>, String> {{
        Err(stubbed("vtab fetch-batch (use per-row path)", vtab_id))
    }}
}}

"##
    )
}

fn emit_udtf_filter_body(entry: &UdtfEntry, arm_indent: &str) -> String {
    // Marshal each arg the WIT signature names, call the WIT
    // function, and store the result list as
    // `Vec<Vec<SqlValue>>` — one inner Vec per row, one SqlValue
    // per visible column. Phase 3 + #532 + #558 cover:
    //
    //   - Single-geom params (postgis dump-style UDTFs).
    //   - F64 / S32 / S64 / U32 / U64 / Bool primitives.
    //   - WitValueRecord params via the per-record
    //     `arg_witvalue_<snake>` helper (mobilitydb temporal-join
    //     etc.).
    //   - ListRecord params via the per-record
    //     `parse_json_list_record_<snake>` helper (#558 W4b:
    //     mobilitydb interval-tree, kdtree, octree, quadtree,
    //     stindex UDTFs).
    //
    // Row decomposition is driven by `entry.shape.output_row`:
    //   - SingleGeom → one Blob column per row.
    //   - SinglePrimitive → one Int/Real/Text/Blob column per row.
    //   - Record { fields } → one SqlValue per field, computed
    //     from the field's UdtfFieldShape.
    //   - Unwired → defer to a stub error.
    let i = arm_indent;
    let mut s = String::new();
    let mut call_args = Vec::new();
    for (idx, p) in entry.shape.params.iter().enumerate() {
        match p {
            ParamShape::Geom => {
                s.push_str(&format!(
                    "{i}let arg{idx} = from_wkb(arg_blob(&args, {idx}, \"{name}\")?, \"{name}\")?;\n",
                    name = entry.sql_name,
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::F64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_f64(&args, {idx}, \"{name}\")?;\n",
                    name = entry.sql_name,
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::S32 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_i64(&args, {idx}, \"{name}\")? as i32;\n",
                    name = entry.sql_name,
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::S64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_i64(&args, {idx}, \"{name}\")?;\n",
                    name = entry.sql_name,
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::U32 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_i64(&args, {idx}, \"{name}\")? as u32;\n",
                    name = entry.sql_name,
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::U64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_i64(&args, {idx}, \"{name}\")? as u64;\n",
                    name = entry.sql_name,
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::Bool => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_i64(&args, {idx}, \"{name}\")? != 0;\n",
                    name = entry.sql_name,
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::Blob => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_blob(&args, {idx}, \"{name}\")?;\n",
                    name = entry.sql_name,
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::OptionNone => {
                call_args.push("None".to_string());
            }
            ParamShape::WitValueRecord {
                kebab_name,
                upstream_by_value,
                ..
            } => {
                // Decode the WitValue payload via the per-record
                // helper emitted at lib.rs top scope. Same path as
                // scalar dispatch — see emit_arm_body's
                // ParamShape::WitValueRecord arm. Task #532.4.
                let snake = kebab_name.replace('-', "_");
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_witvalue_{snake}(&args, {idx}, \"{name}\")?;\n",
                    name = entry.sql_name,
                ));
                if *upstream_by_value {
                    call_args.push(format!("arg{idx}"));
                } else {
                    call_args.push(format!("&arg{idx}"));
                }
            }
            ParamShape::ListRecord { kebab_name, .. } => {
                // W4b (#558): record-element `list<X>` UDTF param via
                // JSON-as-TEXT marshaling. Mirrors the scalar arm in
                // `dispatch::emit_arm_body` (see W2 Phase 2 #553).
                //
                // SQL passes a JSON-array of record-shaped objects
                // (e.g. `'[{"start":0,"end":100,"id":1}, ...]'`); the
                // codegen-emitted `parse_json_list_record_<snake>`
                // helper in the bridge prelude calls
                // `serde_json::from_str::<Vec<UPSTREAM>>` (wit-bindgen
                // additional_derives makes UPSTREAM deserialisable)
                // and the dispatch arm passes `&arg{idx}` to the WIT
                // call.
                //
                // Unlocks 12 mobilitydb UDTFs: interval-tree-query_*,
                // kdtree_xy_*, kdtree_xyz_*, octree_query_*,
                // quadtree_query_box, stindex_find_in_*.
                let snake = kebab_name.replace('-', "_");
                s.push_str(&format!(
                    "{i}let arg{idx} = parse_json_list_record_{snake}(&args, {idx}, \"{name}\")?;\n",
                    name = entry.sql_name,
                ));
                call_args.push(format!("&arg{idx}"));
            }
            _ => {
                // Other shapes (Text, Geog, ListGeom, ListPrim)
                // aren't yet wired through the UDTF filter body.
                // Emit an explicit stub so the bridge still
                // compiles and the unsupported function fails loud
                // at call time.
                //
                // W2 Phase 2 (#553): the shape's Debug representation
                // contains string literals + record-style braces
                // (e.g. `ListRecord { kebab_name: "stindex-entry",
                // ... }`) whose embedded `"`, `{`, `}` all break
                // `format!()`'s template parse if we splice them
                // in directly. Escape the quotes (Rust string
                // literal) and double the braces (format!()
                // template).
                let shape_dbg = format!("{:?}", p)
                    .replace('"', "\\\"")
                    .replace('{', "{{")
                    .replace('}', "}}");
                return format!(
                    "{i}Err(format!(\"{name}: UDTF param shape not yet wired ({shape_dbg})\"))",
                    name = entry.sql_name,
                );
            }
        }
    }
    let module = &entry.shape.wit_module;
    let func = &entry.shape.wit_func;
    let call_args_str = call_args.join(", ");
    let name = &entry.sql_name;
    let call_line = if entry.shape.fallible {
        format!(
            "{i}let __upstream = {module}::{func}({call_args_str})\n\
             {i}    .map_err(|e| format!(\"{name}: {{}}\", shim_err_string(e)))?;\n",
        )
    } else {
        format!("{i}let __upstream = {module}::{func}({call_args_str});\n")
    };
    s.push_str(&call_line);

    // Per-row decomposer: one match arm per output_row shape.
    // Each arm produces a `Vec<Vec<SqlValue>>` named `rows`.
    let row_materialiser = emit_row_materialiser(&entry.shape.output_row, i, name);
    s.push_str(&row_materialiser);
    s.push_str(&format!(
        "{i}UDTF_STATE.with(|m| {{\n\
         {i}    if let Some(c) = m.borrow_mut().get_mut(&cursor_id) {{\n\
         {i}        c.rows = rows;\n\
         {i}        c.idx = 0;\n\
         {i}    }}\n\
         {i}}});\n\
         {i}Ok(())",
    ));
    s
}

/// Emit the per-row decomposer body. The upstream call binds its
/// result list to `__upstream`; this fn produces a let-binding
/// `let rows: Vec<Vec<SqlValue>> = <expr>` that maps each upstream
/// row to its column SqlValue list. Task #532.
fn emit_row_materialiser(
    row: &dispatch::UdtfOutputRow,
    i: &str,
    name: &str,
) -> String {
    match row {
        dispatch::UdtfOutputRow::SingleGeom => format!(
            "{i}let rows: Vec<Vec<SqlValue>> = __upstream\n\
             {i}    .iter()\n\
             {i}    .map(|g| alloc::vec![SqlValue::Blob(g.as_wkb())])\n\
             {i}    .collect();\n",
        ),
        dispatch::UdtfOutputRow::SinglePrimitive { affinity } => {
            // `.iter()` yields `&T` so Integer/Real need an explicit
            // deref before the `as` cast (raw `v as i64` on `&i64` is
            // invalid). Text/Blob already use `.clone()` which is fine
            // on the reference. W4b (#558): first UDTFs returning a
            // primitive list landed via the ListRecord param arm and
            // exposed this pre-existing typo.
            let cell = match affinity {
                dispatch::ColumnAffinity::Integer => "SqlValue::Integer(*v as i64)",
                dispatch::ColumnAffinity::Real => "SqlValue::Real(*v as f64)",
                dispatch::ColumnAffinity::Text => "SqlValue::Text(v.clone())",
                dispatch::ColumnAffinity::Blob => "SqlValue::Blob(v.clone())",
            };
            format!(
                "{i}let rows: Vec<Vec<SqlValue>> = __upstream\n\
                 {i}    .iter()\n\
                 {i}    .map(|v| alloc::vec![{cell}])\n\
                 {i}    .collect();\n",
            )
        }
        dispatch::UdtfOutputRow::Record { fields } => {
            // Build one SqlValue per field. Field names are the
            // WIT-side ident (snake_case after the standard
            // kebab→snake conversion wit-bindgen applies). Each
            // arm dispatches on UdtfFieldShape to produce the
            // right SqlValue::* constructor. Unsupported fields
            // surface as Null so the row is at least loadable;
            // future task: encode nested records as wit-value.
            let mut field_exprs = String::new();
            for f in fields {
                let fsnake = f.name.replace('-', "_");
                let expr = match f.field_shape {
                    dispatch::UdtfFieldShape::Int =>
                        format!("SqlValue::Integer(__row.{fsnake} as i64)"),
                    dispatch::UdtfFieldShape::Real =>
                        format!("SqlValue::Real(__row.{fsnake} as f64)"),
                    dispatch::UdtfFieldShape::Text =>
                        format!("SqlValue::Text(__row.{fsnake}.clone())"),
                    dispatch::UdtfFieldShape::Blob =>
                        format!("SqlValue::Blob(__row.{fsnake}.clone())"),
                    dispatch::UdtfFieldShape::GeomBlob =>
                        format!("SqlValue::Blob(__row.{fsnake}.as_wkb())"),
                    dispatch::UdtfFieldShape::OptionInt => format!(
                        "match &__row.{fsnake} {{ Some(v) => SqlValue::Integer(*v as i64), None => SqlValue::Null }}"
                    ),
                    dispatch::UdtfFieldShape::OptionReal => format!(
                        "match &__row.{fsnake} {{ Some(v) => SqlValue::Real(*v as f64), None => SqlValue::Null }}"
                    ),
                    dispatch::UdtfFieldShape::OptionText => format!(
                        "match &__row.{fsnake} {{ Some(v) => SqlValue::Text(v.clone()), None => SqlValue::Null }}"
                    ),
                    dispatch::UdtfFieldShape::OptionBlob => format!(
                        "match &__row.{fsnake} {{ Some(v) => SqlValue::Blob(v.clone()), None => SqlValue::Null }}"
                    ),
                    dispatch::UdtfFieldShape::OptionGeomBlob => format!(
                        "match &__row.{fsnake} {{ Some(v) => SqlValue::Blob(v.as_wkb()), None => SqlValue::Null }}"
                    ),
                    dispatch::UdtfFieldShape::Unsupported =>
                        "SqlValue::Null".to_string(),
                };
                field_exprs.push_str(&format!("{i}            {expr},\n"));
            }
            format!(
                "{i}let rows: Vec<Vec<SqlValue>> = __upstream\n\
                 {i}    .into_iter()\n\
                 {i}    .map(|__row| alloc::vec![\n\
{field_exprs}{i}        ])\n\
                 {i}    .collect();\n",
            )
        }
        dispatch::UdtfOutputRow::Unwired { reason } => format!(
            "{i}let _ = __upstream;\n\
             {i}return Err(format!(\"{name}: UDTF output row not classifiable: {reason}\"));\n",
            reason = reason.replace('"', "\\\""),
        ),
    }
}

/// Build the (scalar canonical-name + alias) → func-id map.
/// Mirrors the assignment loop inside `emit_metadata_impl`
/// exactly so the dispatcher and the manifest stay in agreement.
fn build_scalar_id_index(plan: &BridgePlan) -> HashMap<&str, u64> {
    let mut idx = HashMap::new();
    let mut id: u64 = 1;
    for ext in &plan.extensions {
        for sc in &ext.scalars {
            idx.insert(sc.canonical_name.as_str(), id);
            id += 1;
            for alias in &sc.aliases {
                idx.insert(alias.as_str(), id);
                id += 1;
            }
        }
    }
    idx
}

fn build_aggregate_id_index(plan: &BridgePlan) -> HashMap<&str, u64> {
    let mut idx = HashMap::new();
    let mut id: u64 = 1_000_000;
    for ext in &plan.extensions {
        for ag in &ext.aggregates {
            idx.insert(ag.canonical_name.as_str(), id);
            id += 1;
            for alias in &ag.aliases {
                idx.insert(alias.as_str(), id);
                id += 1;
            }
        }
    }
    idx
}

/// #616: window-function id index. Mirrors `build_aggregate_id_index`
/// over `plan.extensions[*].window_functions`. Window ids start at
/// 3_000_000 so they don't collide with the scalar / aggregate /
/// UDTF ranges; the `is_window: true` flag on the manifest entry
/// makes the host route through `sqlite3_create_window_function`.
fn build_window_id_index(plan: &BridgePlan) -> HashMap<&str, u64> {
    let mut idx = HashMap::new();
    let mut id: u64 = 3_000_000;
    for ext in &plan.extensions {
        for w in &ext.window_functions {
            idx.insert(w.canonical_name.as_str(), id);
            id += 1;
            for alias in &w.aliases {
                idx.insert(alias.as_str(), id);
                id += 1;
            }
        }
    }
    idx
}

fn build_udtf_id_index(plan: &BridgePlan) -> HashMap<&str, u64> {
    let mut idx = HashMap::new();
    let mut id: u64 = 2_000_000;
    for ext in &plan.extensions {
        for tf in &ext.table_functions {
            idx.insert(tf.canonical_name.as_str(), id);
            id += 1;
            for alias in &tf.aliases {
                idx.insert(alias.as_str(), id);
                id += 1;
            }
        }
    }
    idx
}

/// Emit `impl MetadataGuest for $Bridge`. The describe() result
/// enumerates every scalar / aggregate / UDTF the interface DB
/// lists so the host registers the full surface; bodies for
/// unmapped entries return the stub error. Phase C: also enumerates
/// each `RecordType` in the primary shim's WIT into a
/// `TypedValueBinding` entry on `typed_values`.
fn emit_metadata_impl(
    plan: &BridgePlan,
    bridge_struct: &str,
    records: &[RecordType],
    window_entries: &[WindowEntry],
) -> String {
    let primary = plan
        .extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or("shim");

    let mut scalar_entries = String::new();
    let mut scalar_id: u64 = 1;
    for ext in &plan.extensions {
        for sc in &ext.scalars {
            let num_args = scalar_num_args(sc);
            push_scalar_entry(
                &mut scalar_entries,
                scalar_id,
                &sc.canonical_name,
                num_args,
                sc.is_deterministic,
            );
            scalar_id += 1;
            for alias in &sc.aliases {
                push_scalar_entry(
                    &mut scalar_entries,
                    scalar_id,
                    alias,
                    num_args,
                    sc.is_deterministic,
                );
                scalar_id += 1;
            }
        }
    }

    let mut agg_entries = String::new();
    let mut agg_id: u64 = 1_000_000;
    for ext in &plan.extensions {
        for ag in &ext.aggregates {
            let num_args = aggregate_num_args(ag);
            push_aggregate_entry(
                &mut agg_entries,
                agg_id,
                &ag.canonical_name,
                num_args,
                false,
            );
            agg_id += 1;
            for alias in &ag.aliases {
                push_aggregate_entry(&mut agg_entries, agg_id, alias, num_args, false);
                agg_id += 1;
            }
        }
    }
    // #616: window functions ride the AggregateFunctionSpec list
    // with `is_window: true`. Ids start at 3_000_000 (matches
    // `build_window_id_index`); per-row arg count includes the
    // streaming geometry + any constant extras.
    let mut window_id_iter: u64 = 3_000_000;
    let mut window_arg_count: HashMap<&str, i32> = HashMap::new();
    for w in window_entries {
        let num_args = (1 + w.shape.extra_args.len()) as i32;
        window_arg_count.insert(w.sql_name.as_str(), num_args);
    }
    for ext in &plan.extensions {
        for w in &ext.window_functions {
            let num_args = window_arg_count
                .get(w.canonical_name.as_str())
                .copied()
                .unwrap_or_else(|| {
                    // Fallback if the classifier didn't wire it (shouldn't
                    // happen for postgis pilot but keeps behaviour predictable
                    // when a future extension has interface-DB rows without a
                    // matching upstream).
                    w.param_signatures.first().map(|s| s.len() as i32).unwrap_or(-1)
                });
            push_aggregate_entry(
                &mut agg_entries,
                window_id_iter,
                &w.canonical_name,
                num_args,
                true,
            );
            window_id_iter += 1;
            for alias in &w.aliases {
                push_aggregate_entry(
                    &mut agg_entries,
                    window_id_iter,
                    alias,
                    num_args,
                    true,
                );
                window_id_iter += 1;
            }
        }
    }

    let mut vtab_entries = String::new();
    let mut tf_id: u64 = 2_000_000;
    for ext in &plan.extensions {
        for tf in &ext.table_functions {
            push_vtab_entry(&mut vtab_entries, tf_id, &tf.canonical_name);
            tf_id += 1;
            for alias in &tf.aliases {
                push_vtab_entry(&mut vtab_entries, tf_id, alias);
                tf_id += 1;
            }
        }
    }

    let version = plan
        .extensions
        .first()
        .map(|e| e.version.as_str())
        .unwrap_or("0.1.0");

    // Phase C: typed_values entries — one per record discovered in
    // the primary shim's WIT. The host indexes these into its
    // per-extension typed-value registry at load time.
    let mut typed_value_entries = String::new();
    for r in records {
        push_typed_value_entry(&mut typed_value_entries, r);
    }

    // Use the TypedValueBinding type from the generated bindings.
    let typed_value_use = if !records.is_empty() {
        "        use bindings::exports::sqlite::extension::metadata::TypedValueBinding;\n"
    } else {
        ""
    };

    format!(
        r##"impl MetadataGuest for {bridge_struct} {{
    fn describe() -> Manifest {{
        let det = FunctionFlags::DETERMINISTIC;
        let utf8 = FunctionFlags::empty();

        let _ = utf8;
{typed_value_use}
        let scalar_functions: Vec<ScalarFunctionSpec> = alloc::vec![
{scalar_entries}        ];
        let aggregate_functions: Vec<AggregateFunctionSpec> = alloc::vec![
{agg_entries}        ];
        let vtabs: Vec<VtabSpec> = alloc::vec![
{vtab_entries}        ];
        let typed_values: Vec<TypedValueBinding> = alloc::vec![
{typed_value_entries}        ];

        Manifest {{
            name: "{primary}".into(),
            version: "{version}".into(),
            scalar_functions,
            aggregate_functions,
            collations: alloc::vec![],
            vtabs,
            dot_commands: alloc::vec![],
            has_authorizer: false,
            has_update_hook: false,
            has_commit_hook: false,
            has_wal_hook: false,
            wal_hook_id: 0,
            declared_capabilities: alloc::vec![],
            optional_capabilities: alloc::vec![],
            preferred_prefix: None,
            prefix_expansion: None,
            typed_values,
        }}
    }}
}}

"##,
    )
}

/// Push one `TypedValueBinding { ... }` entry into the manifest
/// emit. Phase C: the encoder/decoder import names point at the
/// deferred `serde-ops` interface; Phase E lands the actual
/// implementations.
fn push_typed_value_entry(out: &mut String, r: &RecordType) {
    // 32-byte sha256 type-id as a byte literal.
    let type_id_lit: Vec<String> =
        r.type_id.iter().map(|b| format!("0x{:02x}", b)).collect();
    let decoder = r.decoder_import();
    let encoder = r.encoder_import();
    let symbolic = &r.symbolic_name;
    out.push_str(&format!(
        "            TypedValueBinding {{\n\
         \x20               type_id: alloc::vec![{type_id}],\n\
         \x20               symbolic_name: \"{symbolic}\".into(),\n\
         \x20               decoder_import: \"{decoder}\".into(),\n\
         \x20               encoder_import: \"{encoder}\".into(),\n\
         \x20           }},\n",
        type_id = type_id_lit.join(", "),
        symbolic = symbolic.replace('"', "\\\""),
        decoder = decoder.replace('"', "\\\""),
        encoder = encoder.replace('"', "\\\""),
    ));
}

fn push_scalar_entry(out: &mut String, id: u64, name: &str, num_args: i32, det: bool) {
    let flags = if det { "det" } else { "FunctionFlags::empty()" };
    out.push_str(&format!(
        "            ScalarFunctionSpec {{ id: {id}, name: \"{name}\".into(), num_args: {num_args}, func_flags: {flags} }},\n",
        id = id,
        name = name.replace('"', "\\\""),
        num_args = num_args,
        flags = flags,
    ));
}

fn push_aggregate_entry(
    out: &mut String,
    id: u64,
    name: &str,
    num_args: i32,
    is_window: bool,
) {
    out.push_str(&format!(
        "            AggregateFunctionSpec {{ id: {id}, name: \"{name}\".into(), num_args: {num_args}, func_flags: FunctionFlags::empty(), is_window: {is_window} }},\n",
        id = id,
        name = name.replace('"', "\\\""),
        num_args = num_args,
        is_window = is_window,
    ));
}

fn push_vtab_entry(out: &mut String, id: u64, name: &str) {
    out.push_str(&format!(
        "            VtabSpec {{ id: {id}, name: \"{name}\".into(), eponymous: true, mutable: false, batched: false }},\n",
        id = id,
        name = name.replace('"', "\\\""),
    ));
}

fn scalar_num_args(sc: &shim_bridge_codegen_core::ScalarFn) -> i32 {
    let variants = &sc.param_signatures;
    if variants.is_empty() {
        return -1;
    }
    let first = variants[0].len();
    if variants.iter().all(|v| v.len() == first) {
        first as i32
    } else {
        -1
    }
}

fn aggregate_num_args(ag: &shim_bridge_codegen_core::AggregateFn) -> i32 {
    let variants = &ag.param_signatures;
    if variants.is_empty() {
        return -1;
    }
    let first = variants[0].len();
    if variants.iter().all(|v| v.len() == first) {
        first as i32
    } else {
        -1
    }
}

/// Emit `impl SerdeOpsGuest for $Bridge` with real canonical-CBOR
/// codec bodies for each record's encoder + decoder. Phase E.
///
/// The wit-bindgen-generated record types carry
/// `serde::Serialize + Deserialize` derives via the bindgen
/// invocation's `additional_derives` (see `mod bindings`). That
/// makes the codec bodies tiny: encode is
/// `ciborium::ser::into_writer(&value, &mut buf)` and decode is
/// `ciborium::de::from_reader(bytes.as_slice())`.
///
/// The bridge ships every record discovered in the primary shim's
/// WIT regardless of whether any wired dispatch arm currently
/// references it. The host's per-extension typed-value registry
/// indexes them all at load time; future scalars that take these
/// records become wirable without re-shipping the bridge.
fn emit_serde_ops_impl(
    primary: &str,
    bridge_struct: &str,
    records: &[RecordType],
) -> String {
    let primary_snake = primary.replace('-', "_");
    let mut s = String::new();
    s.push_str("\n// Phase E serde-ops impl. Bodies use ciborium\n");
    s.push_str("// against the wit-bindgen-generated Rust types\n");
    s.push_str("// (which carry serde::Serialize + Deserialize\n");
    s.push_str("// derives via the bindgen invocation's\n");
    s.push_str("// additional_derives).\n");
    s.push_str(&format!(
        "use bindings::exports::sqlink_bridge::{primary_snake}::serde_ops::Guest as SerdeOpsGuest;\n"
    ));
    // The record types live in the exports::...::serde_ops module
    // (wit-bindgen re-exports the `use`'d types under the exporting
    // interface's export path so they're addressable as
    // `bindings::exports::<pkg>::<iface>::<Type>`).
    let record_names_pascal: Vec<String> =
        records.iter().map(|r| pascal_case(&r.kebab_name)).collect();
    if !record_names_pascal.is_empty() {
        s.push_str(&format!(
            "use bindings::exports::sqlink_bridge::{primary_snake}::serde_ops::{{{}}};\n",
            record_names_pascal.join(", "),
        ));
    }
    s.push_str(&format!(
        "impl SerdeOpsGuest for {bridge_struct} {{\n"
    ));
    for r in records {
        let snake = r.snake_name();
        let pascal = pascal_case(&r.kebab_name);
        // Decoder: bytes → record. Errors map ciborium's
        // structured error to a plain String the host surfaces
        // verbatim.
        s.push_str(&format!(
            "    fn {snake}_from_canon_cbor(bytes: Vec<u8>) -> Result<{pascal}, String> {{\n\
             \x20       ciborium::de::from_reader::<{pascal}, _>(bytes.as_slice())\n\
             \x20           .map_err(|e| format!(\"{primary} {kebab}: canon-cbor decode: {{e}}\"))\n\
             \x20   }}\n",
            kebab = r.kebab_name,
        ));
        // Encoder: record → bytes. ciborium::ser::into_writer is
        // infallible against a Vec<u8> writer (no IO can fail),
        // but we surface a serialisation error if one bubbles up
        // by returning an empty payload — Phase E's contract is
        // that the encoder ALWAYS produces a payload the host
        // hands to the matching decoder; a panic here would
        // surface as a wasm trap, which the host translates to a
        // dispatch error. ciborium's only fail modes are IO
        // (impossible against Vec<u8>) and feature gates we
        // don't use.
        s.push_str(&format!(
            "    fn {snake}_to_canon_cbor(value: {pascal}) -> Vec<u8> {{\n\
             \x20       let mut buf: Vec<u8> = Vec::new();\n\
             \x20       ciborium::ser::into_writer(&value, &mut buf)\n\
             \x20           .expect(\"{primary} {kebab}: canon-cbor encode into Vec<u8> cannot IO-fail\");\n\
             \x20       buf\n\
             \x20   }}\n",
            kebab = r.kebab_name,
        ));
    }
    s.push_str("}\n\n");
    s
}

/// Walk the wired dispatch entries (scalars, aggregates, UDTFs)
/// and collect every record name that appears in a
/// `WitValueRecord` param or return shape. Drives the
/// `emit_wit_value_helpers` filter so helpers are emitted only
/// for records that are actually referenced by a wired arm
/// (otherwise wit-bindgen's import-side elision drops the upstream
/// type and the helper fails to compile).
fn collect_referenced_records(
    scalar_entries: &[(dispatch::DispatchEntry, bool)],
    agg_entries: &[dispatch::AggregateEntry],
    udtf_entries: &[dispatch::UdtfEntry],
) -> std::collections::BTreeSet<String> {
    let mut out: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    fn record_name_in_ret(r: &dispatch::RetShape) -> Option<&String> {
        match r {
            dispatch::RetShape::WitValueRecord { kebab_name, .. }
            | dispatch::RetShape::OptionWitValueRecord { kebab_name, .. }
            | dispatch::RetShape::FirstWitValueRecord { kebab_name, .. } => Some(kebab_name),
            _ => None,
        }
    }
    for (entry, _f) in scalar_entries {
        for p in &entry.shape.params {
            if let dispatch::ParamShape::WitValueRecord { kebab_name, .. } = p {
                out.insert(kebab_name.clone());
            }
            // W2 Phase 2 (#553): ListRecord params also need the
            // per-record `parse_json_list_record_<snake>` helper
            // emitted, so the same registry sweep must include them.
            if let dispatch::ParamShape::ListRecord { kebab_name, .. } = p {
                out.insert(kebab_name.clone());
            }
        }
        if let Some(name) = record_name_in_ret(&entry.shape.ret) {
            out.insert(name.clone());
        }
    }
    for entry in agg_entries {
        for p in &entry.shape.extra_args {
            if let dispatch::ParamShape::WitValueRecord { kebab_name, .. } = p {
                out.insert(kebab_name.clone());
            }
            if let dispatch::ParamShape::ListRecord { kebab_name, .. } = p {
                out.insert(kebab_name.clone());
            }
        }
        if let Some(name) = record_name_in_ret(&entry.shape.ret) {
            out.insert(name.clone());
        }
        // #607 Phase 1 + #612 (OQ1): AccKind::Record aggregates
        // reference TWO per-record codec sites — `arg_witvalue_<in>`
        // for the input record's decoder + `ret_to_witvalue_<out>`
        // for the output record's encoder. For same-record aggregates
        // (Phase 1 pilot scope) the two kebabs match, so only one
        // codec block is emitted. For different-record aggregates
        // (#612: `tgeompoint-st-extent`, `t*-temporal-count`) both
        // need to be present.
        //
        // #614 + #640: `RecordToScalar` / `RecordToTuple` only
        // reference the INPUT-side `arg_witvalue_<in>` helper — the
        // output is a primitive scalar wrap (#614) or a JSON-encoded
        // primitive tuple (#640), not a record codec call.
        match &entry.shape.accumulator_kind {
            dispatch::AccKind::Record { input, output } => {
                out.insert(input.kebab_name.clone());
                out.insert(output.kebab_name.clone());
            }
            dispatch::AccKind::RecordToScalar { input, .. }
            | dispatch::AccKind::RecordToTuple { input, .. } => {
                out.insert(input.kebab_name.clone());
            }
            dispatch::AccKind::Geom | dispatch::AccKind::Raster => {}
        }
    }
    for entry in udtf_entries {
        for p in &entry.shape.params {
            if let dispatch::ParamShape::WitValueRecord { kebab_name, .. } = p {
                out.insert(kebab_name.clone());
            }
            if let dispatch::ParamShape::ListRecord { kebab_name, .. } = p {
                out.insert(kebab_name.clone());
            }
        }
    }
    out
}

/// W2 Phase 2 mop-up (#555): walk the wired dispatch entries and
/// collect every unique tuple-element signature that appears in a
/// `ParamShape::ListTuple`. Drives `render_tuple_list_helpers` so
/// each `parse_json_list_tuple_<sig>` is emitted once per bridge.
fn collect_tuple_list_sigs(
    scalar_entries: &[(dispatch::DispatchEntry, bool)],
    agg_entries: &[dispatch::AggregateEntry],
    udtf_entries: &[dispatch::UdtfEntry],
) -> std::collections::BTreeSet<Vec<dispatch::ListPrimElem>> {
    let mut out: std::collections::BTreeSet<Vec<dispatch::ListPrimElem>> =
        std::collections::BTreeSet::new();
    for (entry, _f) in scalar_entries {
        for p in &entry.shape.params {
            if let Some(sig) = p.list_tuple_sig() {
                out.insert(sig.to_vec());
            }
        }
    }
    for entry in agg_entries {
        for p in &entry.shape.extra_args {
            if let Some(sig) = p.list_tuple_sig() {
                out.insert(sig.to_vec());
            }
        }
    }
    for entry in udtf_entries {
        for p in &entry.shape.params {
            if let Some(sig) = p.list_tuple_sig() {
                out.insert(sig.to_vec());
            }
        }
    }
    out
}

/// W2 Phase 2 mop-up (#555): render each tuple-list helper into the
/// bridge prelude. Each helper:
///   - Reads the SQL TEXT arg as JSON.
///   - Parses via `serde_json::from_str::<Vec<(T1, T2, ...)>>` —
///     serde renders Rust tuples as fixed-length JSON arrays, which
///     matches the SQL surface `'[[1, 10], [20, 30]]'`.
///
/// The signature suffix follows the same `helper_suffix()` convention
/// as `parse_json_list_<T>` (so `list<tuple<s32, s32>>` →
/// `parse_json_list_tuple_i32_i32`).
fn render_tuple_list_helpers(
    sigs: &std::collections::BTreeSet<Vec<dispatch::ListPrimElem>>,
) -> String {
    if sigs.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    s.push_str(
        "\n// W2 Phase 2 mop-up (#555): list<tuple<...>> param helpers.\n\
         // One per unique tuple-element signature referenced by a wired\n\
         // dispatch arm. SQL passes a JSON-array of arrays as the TEXT\n\
         // arg (e.g. `'[[1, 10], [20, 30]]'`); serde_json parses tuples\n\
         // as fixed-length JSON arrays so the upstream\n\
         // `Vec<(T1, T2, ...)>` binding round-trips directly.\n",
    );
    for elements in sigs {
        let suffix = dispatch::list_tuple_sig_suffix(elements);
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
                dispatch::ListPrimElem::F64 => "f64",
                dispatch::ListPrimElem::F32 => "f32",
                dispatch::ListPrimElem::S32 => "s32",
                dispatch::ListPrimElem::S64 => "s64",
                dispatch::ListPrimElem::U32 => "u32",
                dispatch::ListPrimElem::U64 => "u64",
                dispatch::ListPrimElem::U8 => "u8",
                dispatch::ListPrimElem::Bool => "bool",
                dispatch::ListPrimElem::String => "string",
            })
            .collect::<Vec<_>>()
            .join(", ");
        s.push_str(&format!(
            "#[allow(dead_code)]\n\
             fn parse_json_list_tuple_{suffix}(args: &[SqlValue], idx: usize, name: &str) -> Result<Vec<{rust_tuple}>, String> {{\n\
             \x20   let text = arg_text(args, idx, name)?;\n\
             \x20   serde_json::from_str::<Vec<{rust_tuple}>>(text)\n\
             \x20       .map_err(|e| format!(\"{{name}}: arg {{idx}} must be JSON array of [{wit_label}] tuples ({{e}})\"))\n\
             }}\n\n",
        ));
    }
    s
}


/// Emit per-record wit-value marshaling helpers (Phase E).
///
/// For each record `R` in the registry, emits:
///
///   - `arg_witvalue_<snake>(args, idx, name) -> Result<UPSTREAM_R, String>`:
///     unwraps a SqlValue::WitValue payload, calls the bridge's
///     LOCAL serde-ops decoder (`<R>_from_canon_cbor`), then
///     ciborium-round-trips LOCAL → UPSTREAM. Same shape on both
///     sides by construction, so the bytes round-trip identically.
///
///   - `ret_to_witvalue_<snake>(upstream: UPSTREAM_R) -> Result<SqlValue, String>`:
///     encodes UPSTREAM via ciborium into canon-CBOR bytes (same as
///     LOCAL encode would produce); wraps as
///     `SqlValue::WitValue { type_id, bytes, symbolic_name }`.
///
/// The helpers are unused by postgis (whose scalars don't take
/// records on the SQL boundary) but emitted unconditionally so the
/// codegen treatment is uniform; `#[allow(dead_code)]` keeps the
/// compiler quiet.
fn emit_wit_value_helpers(
    primary: &str,
    bridge_struct: &str,
    records: &[RecordType],
) -> String {
    let primary_snake = primary.replace('-', "_");
    let mut s = String::new();
    s.push_str("\n// ── Phase E wit-value marshaling helpers ──\n");
    s.push_str("// Per-record `arg_witvalue_<snake>` (param) and\n");
    s.push_str("// `ret_to_witvalue_<snake>` (return). Each helper\n");
    s.push_str("// closes over the bridge's LOCAL serde-ops codec and\n");
    s.push_str("// the UPSTREAM type from the shim package. LOCAL +\n");
    s.push_str("// UPSTREAM share field shapes by construction so the\n");
    s.push_str("// ciborium round-trip is byte-identical.\n");
    s.push_str("//\n");
    s.push_str("// Task #523: records whose LOCAL clone is byte-compatible\n");
    s.push_str("// with the UPSTREAM Rust type (the `direct` flag on the\n");
    s.push_str("// registry) skip the LOCAL→UPSTREAM round-trip and decode\n");
    s.push_str("// payload bytes straight into the upstream type. The\n");
    s.push_str("// criterion is field-level recursive (see\n");
    s.push_str("// `record_registry::RecordType::direct`).\n");
    s.push_str("#[allow(dead_code)]\n");
    s.push_str("use bindings::sqlite::extension::types::WitValuePayload;\n\n");
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
        let local_path = format!(
            "bindings::exports::sqlink_bridge::{primary_snake}::serde_ops::{pascal}",
        );
        let type_id_bytes: Vec<String> =
            r.type_id.iter().map(|b| format!("0x{:02x}", b)).collect();
        let symbolic = r.symbolic_name.replace('"', "\\\"");
        // Decoder helper. Two paths depending on whether the
        // record's LOCAL clone is byte-compatible with UPSTREAM:
        //   - `direct == true`: ciborium-decode straight into UPSTREAM
        //     (skips one alloc + one struct copy per wit-value call).
        //   - `direct == false`: LOCAL serde-ops decode, then
        //     ciborium round-trip into UPSTREAM (preserves the
        //     pre-#523 semantics for records whose LOCAL ≠ UPSTREAM
        //     at the serde layer — alias inlining over non-primitive
        //     types, cross-package references, etc.).
        if r.direct {
            s.push_str(&format!(
                "#[allow(dead_code)]\n\
                 fn arg_witvalue_{snake}(\n\
                 \x20   args: &[SqlValue],\n\
                 \x20   idx: usize,\n\
                 \x20   name: &str,\n\
                 ) -> Result<{upstream_path}, String> {{\n\
                 \x20   let payload = match args.get(idx) {{\n\
                 \x20       Some(SqlValue::WitValue(p)) => p,\n\
                 \x20       _ => return Err(format!(\"{{name}}: arg {{idx}} must be WIT-VALUE\")),\n\
                 \x20   }};\n\
                 \x20   // Task #523 short-circuit: LOCAL serde-ops type\n\
                 \x20   // is byte-compatible with UPSTREAM (verified at\n\
                 \x20   // codegen time via record_registry's `direct`\n\
                 \x20   // fix-point). Decode straight into UPSTREAM.\n\
                 \x20   ciborium::de::from_reader::<{upstream_path}, _>(payload.bytes.as_slice())\n\
                 \x20       .map_err(|e| format!(\"{{name}}: decode arg {{idx}}: {{}}\", e))\n\
                 }}\n\n",
            ));
        } else {
            s.push_str(&format!(
                "#[allow(dead_code)]\n\
                 fn arg_witvalue_{snake}(\n\
                 \x20   args: &[SqlValue],\n\
                 \x20   idx: usize,\n\
                 \x20   name: &str,\n\
                 ) -> Result<{upstream_path}, String> {{\n\
                 \x20   let payload = match args.get(idx) {{\n\
                 \x20       Some(SqlValue::WitValue(p)) => p,\n\
                 \x20       _ => return Err(format!(\"{{name}}: arg {{idx}} must be WIT-VALUE\")),\n\
                 \x20   }};\n\
                 \x20   // LOCAL serde-ops decode — proves the codec\n\
                 \x20   // fires (not the identity-passthrough fallback).\n\
                 \x20   let __loc: {local_path} = <{bridge_struct} as bindings::exports::sqlink_bridge::{primary_snake}::serde_ops::Guest>::{snake}_from_canon_cbor(\n\
                 \x20       payload.bytes.clone(),\n\
                 \x20   )\n\
                 \x20   .map_err(|e| format!(\"{{name}}: decode arg {{idx}}: {{}}\", e))?;\n\
                 \x20   // LOCAL → UPSTREAM via ciborium round-trip.\n\
                 \x20   let mut __buf: Vec<u8> = Vec::new();\n\
                 \x20   ciborium::ser::into_writer(&__loc, &mut __buf)\n\
                 \x20       .map_err(|e| format!(\"{{name}}: re-encode arg {{idx}}: {{}}\", e))?;\n\
                 \x20   ciborium::de::from_reader::<{upstream_path}, _>(__buf.as_slice())\n\
                 \x20       .map_err(|e| format!(\"{{name}}: convert arg {{idx}}: {{}}\", e))\n\
                 }}\n\n",
            ));
        }
        // W2 Phase 2 (#553): `list<R>` param helper. Parses a JSON
        // array of record-shaped objects from the SQL TEXT arg
        // straight into `Vec<UPSTREAM>` via serde_json's derive.
        //
        // Wit-bindgen's `additional_derives: [serde::Deserialize]`
        // makes UPSTREAM deserialisable; no LOCAL→UPSTREAM ciborium
        // round-trip is needed because the dispatch is by func_id,
        // not by type_id (the type identity is fixed at the dispatch
        // arm, not carried in the SQL payload).
        //
        // Emitted for every record in the helper-records set so
        // ListRecord-param scalars find the helper at compile time
        // regardless of whether the bridge also has a non-list
        // WitValueRecord param/return for the same record.
        s.push_str(&format!(
            "#[allow(dead_code)]\n\
             fn parse_json_list_record_{snake}(\n\
             \x20   args: &[SqlValue],\n\
             \x20   idx: usize,\n\
             \x20   name: &str,\n\
             ) -> Result<Vec<{upstream_path}>, String> {{\n\
             \x20   let text = arg_text(args, idx, name)?;\n\
             \x20   serde_json::from_str::<Vec<{upstream_path}>>(text)\n\
             \x20       .map_err(|e| format!(\"{{name}}: arg {{idx}} must be JSON array of {kebab} ({{e}})\"))\n\
             }}\n\n",
            kebab = r.kebab_name,
        ));
        // Encoder helper: UPSTREAM → bytes → WitValue.
        s.push_str(&format!(
            "#[allow(dead_code)]\n\
             fn ret_to_witvalue_{snake}(\n\
             \x20   upstream: {upstream_path},\n\
             ) -> Result<SqlValue, String> {{\n\
             \x20   let mut __buf: Vec<u8> = Vec::new();\n\
             \x20   ciborium::ser::into_writer(&upstream, &mut __buf)\n\
             \x20       .map_err(|e| format!(\"encode {snake} wit-value: {{}}\", e))?;\n\
             \x20   Ok(SqlValue::WitValue(WitValuePayload {{\n\
             \x20       type_id: alloc::vec![{tid}],\n\
             \x20       bytes: __buf,\n\
             \x20       symbolic_name: \"{symbolic}\".into(),\n\
             \x20   }}))\n\
             }}\n\n",
            tid = type_id_bytes.join(", "),
        ));
    }
    s
}

/// Discover which subdir of `wit_deps_root` holds the primary shim's
/// upstream WIT package. The "primary" pick rule, in order:
///   1. The subdir whose parsed package ns_name's tail starts with
///      the primary name (e.g. `postgis:wasm` for primary `postgis`,
///      `mobilitydb:temporal` for `mobilitydb`).
///   2. The subdir whose dirname starts with the primary name.
///   3. The first subdir that isn't `sqlite-extension` or
///      `sfcgal-component` (or any other well-known helper).
fn pick_primary_shim_dir(
    primary: &str,
    wit_deps_root: &std::path::Path,
    shim_packages: &[wit_parse::WitPackage],
) -> Option<std::path::PathBuf> {
    // (1) Match by package ns_name.
    for pkg in shim_packages {
        // ns_name is "namespace:package", e.g. "postgis:wasm".
        let ns = pkg.ns_name.split(':').next().unwrap_or("");
        if ns == primary {
            // Find a subdir whose parsed package matches.
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
    // (2) Match by dirname prefix.
    if let Ok(rd) = std::fs::read_dir(wit_deps_root) {
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let name = e.file_name();
            let s = name.to_string_lossy();
            if s.starts_with(primary) {
                return Some(e.path());
            }
        }
    }
    // (3) First non-helper subdir.
    if let Ok(rd) = std::fs::read_dir(wit_deps_root) {
        let mut paths: Vec<std::path::PathBuf> =
            rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        paths.sort();
        for p in paths {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            match name {
                "sqlite-extension" | "sfcgal-component" => continue,
                _ => return Some(p),
            }
        }
    }
    None
}

/// Split `"ns:name"` into `("ns", "name")`. Falls back to the whole
/// string as namespace + empty name if no colon.
fn split_pkg(pkg: &str) -> (String, String) {
    match pkg.find(':') {
        Some(i) => (pkg[..i].to_string(), pkg[i + 1..].to_string()),
        None => (pkg.to_string(), String::new()),
    }
}

/// Convert a WIT package namespace or name to its Rust module ident
/// as wit-bindgen would generate it (kebab → snake).
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

const HEADER: &str =
    "// === GENERATED by sqlink-shim-codegen (target=wasm-component)  do not edit by hand ===\n\n";

/// Postgis-specific helpers — emitted only when the shim's WIT
/// declares `resource geometry` + `variant postgis-error`. For
/// non-postgis shims the dispatcher never references these so the
/// helpers can be omitted without breaking compilation.
const POSTGIS_HELPERS_BODY: &str = r#"

fn from_wkb(bytes: &[u8], name: &str) -> Result<Geometry, String> {
    Geometry::from_wkb(bytes).map_err(|e| format!("{name}: {}", postgis_err_string(e)))
}

fn geog_from_wkb(bytes: &[u8], name: &str) -> Result<Geography, String> {
    Geography::from_wkb(bytes).map_err(|e| format!("{name}: {}", postgis_err_string(e)))
}

/// Format a `postgis-error` variant back to a string the SQL
/// caller can read. Mirrors the helper in the hand-written
/// bridge.
fn postgis_err_string(e: PostgisError) -> String {
    match e {
        PostgisError::InvalidGeometry(s)
        | PostgisError::ParseError(s)
        | PostgisError::UnsupportedOperation(s)
        | PostgisError::NumericError(s)
        | PostgisError::SridMismatch(s)
        | PostgisError::General(s) => s,
    }
}"#;

/// Round-490: raster prelude helpers. Mirror of POSTGIS_HELPERS_BODY
/// for the raster resource. Emitted only when the shim's WIT
/// declares `resource raster` + `variant raster-error`.
///
/// The decoder calls the free `from-binary` function on the
/// `postgis-raster-types` module (qualified by package path so we
/// don't need an extra `use` alias). The encode-side path uses the
/// resource's own `as-binary` method in the dispatch arm body.
fn render_raster_helpers(pkg_ns: &str, pkg_name: &str) -> String {
    format!(
        r#"

fn from_raster_binary(bytes: &[u8], name: &str) -> Result<Raster, String> {{
    bindings::{pkg_ns}::{pkg_name}::postgis_raster_types::from_binary(bytes)
        .map_err(|e| format!("{{}}: {{}}", name, raster_err_string(e)))
}}

/// Format a `raster-error` variant back to a string the SQL caller
/// can read.
fn raster_err_string(e: RasterError) -> String {{
    match e {{
        RasterError::ParseError(s)
        | RasterError::OutOfBounds(s)
        | RasterError::TypeMismatch(s)
        | RasterError::General(s) => s,
    }}
}}"#,
        pkg_ns = pkg_ns,
        pkg_name = pkg_name,
    )
}

/// Round-490: topology prelude helpers. Mirror of POSTGIS_HELPERS_BODY
/// for the topology resource. Emitted only when the shim's WIT
/// declares `resource topology` + `variant topology-error`.
fn render_topology_helpers(pkg_ns: &str, pkg_name: &str) -> String {
    format!(
        r#"

fn from_topology_bytes(bytes: &[u8], name: &str) -> Result<Topology, String> {{
    bindings::{pkg_ns}::{pkg_name}::postgis_topology_types::from_bytes(bytes)
        .map_err(|e| format!("{{}}: {{}}", name, topology_err_string(e)))
}}

/// Format a `topology-error` variant back to a string the SQL caller
/// can read. The `*-not-found` arms render the numeric id alongside
/// a short label so the caller sees which primitive failed.
fn topology_err_string(e: TopologyError) -> String {{
    match e {{
        TopologyError::InvalidTopology(s) | TopologyError::General(s) => s,
        TopologyError::NodeNotFound(id) => format!("node not found: {{}}", id),
        TopologyError::EdgeNotFound(id) => format!("edge not found: {{}}", id),
        TopologyError::FaceNotFound(id) => format!("face not found: {{}}", id),
    }}
}}"#,
        pkg_ns = pkg_ns,
        pkg_name = pkg_name,
    )
}
