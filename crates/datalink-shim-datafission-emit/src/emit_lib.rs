//! Emit `src/lib.rs` for the wasm-component bridge (datafission
//! target).
//!
//! Scalar-first cut. The generated file:
//!   1. `wit_bindgen::generate!` against the datafission-extension
//!      world (path: "wit", world: "bridge", generate_all).
//!   2. `use` declarations binding each upstream-shim module that
//!      the scalar dispatch arms reference, plus the datafission
//!      function-plugin type aliases (`types::ScalarValue` /
//!      `types::FunctionError` / `types::LogicalType`).
//!   3. `struct Component;` — the single Guest implementer for
//!      every export interface of the composite world.
//!   4. `impl identity::Guest for Component` — returns the primary
//!      name + version.
//!   5. `impl sql_extension_plugin::metadata::Guest for Component`
//!      — empty cast / operator / preprocessor lists.
//!   6. `impl scalar_function_registry::Guest for Component` —
//!      WIRED. `list_functions()` returns the per-scalar metadata
//!      collected from the interface DB; `return_type` dispatches
//!      by name; `execute` runs the per-scalar arm body via the
//!      shape emit; `execute_batch` loops.
//!   7. Stub impls for aggregate / window / table-function
//!      registries — empty `list_functions`, per-call methods
//!      return `UnknownFunction`.
//!   8. Stub impl for multi-custom-type — empty advertisements +
//!      `Internal` errors. (The single-type `custom-type`
//!      interface is intentionally NOT exported.)
//!   9. Stub impls for spatial-index / system-catalog / index-plugin
//!      — empty advertisements + `Internal` / `UnsupportedOperation`
//!      errors.
//!  10. `export!(Component);` at file scope.
//!
//! Unlike the SQLite and DuckDB targets, datafission's contract
//! has NO `register_*` call: the host snapshots each registry's
//! `list-functions()` ONCE at `CREATE EXTENSION` time, then
//! dispatches per-call by `name` parameter. No handle table is
//! needed for the scalar path. (Aggregate accumulators DO use
//! u64 handles, but the scalar-first cut doesn't exercise them.)

use anyhow::Result;

use shim_bridge_codegen_core::BridgePlan;
use datalink_shim_codegen_core::interface_db;
use datalink_shim_codegen_core::record_registry::{self, RecordType};

use crate::dispatch;
use crate::emit_wit;

/// Snake → PascalCase converter mirroring wit-bindgen's ident
/// conversion. Used for resource-type idents (Geometry, Raster,
/// Topology) and for per-record Rust type names.
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

/// Generate `src/lib.rs`.
pub fn lib_rs(plan: &BridgePlan, crate_name: &str) -> Result<String> {
    let primary = plan
        .extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or("shim");
    let version = plan
        .extensions
        .first()
        .map(|e| e.version.as_str())
        .unwrap_or("0.1.0");

    let wit_deps_root = emit_wit::source_shim_deps_dir(primary)?;
    let shim_packages = emit_wit::discover_shim_packages(&wit_deps_root)?;
    let shim_wit_dir = pick_primary_shim_dir(primary, &wit_deps_root, &shim_packages)
        .unwrap_or_else(|| wit_deps_root.clone());

    // Pull the `datafission:extension` package version from the
    // vendored WIT so identity::api_version() reports whatever
    // contract the bridge was generated against (no hardcoded
    // "1.0.0" — a future bump flows through automatically).
    let extension_pkg = emit_wit::discover_datafission_extension_package(primary)?;
    let api_version = extension_pkg.version.clone();

    // Records are kept only for symmetry with sqlite-emit /
    // duckdb-emit — the dispatch arms in this cut don't reference
    // them yet. A follow-up adds typed-value-binding via the
    // datafission type-plugin/multi-custom-type interface.
    let records: Vec<RecordType> = record_registry::build(&shim_packages, primary)
        .into_iter()
        .filter(|r| emit_wit::package_belongs_to_primary(&r.package, primary))
        .collect();

    let (scalar_entries, scalar_unwired) =
        interface_db::build_full(plan, &shim_wit_dir, &records)?;

    // Aggregate entries — wires the postgis & mobilitydb
    // dissolve-shape aggregates against datafission's
    // `aggregate_function_registry@1.0.0` handle-based ABI.
    let (aggregate_entries, aggregate_unwired) =
        interface_db::build_aggregate_registry(plan, &shim_wit_dir, &records)?;

    // Report on what fell through so the maintainer sees coverage
    // at codegen time.
    let total_unwired = scalar_unwired.len();
    if total_unwired > 0 {
        eprintln!(
            "[datafission-target] {total_unwired} scalar(s) not wired in scalar-first cut:"
        );
        for u in &scalar_unwired {
            eprintln!("  - {}: {}", u.sql_name, u.reason);
        }
    }
    if !aggregate_unwired.is_empty() {
        eprintln!(
            "[datafission-target] {} aggregate(s) not wired:",
            aggregate_unwired.len(),
        );
        for u in &aggregate_unwired {
            eprintln!("  - {}: {}", u.sql_name, u.reason);
        }
    }

    // Track which WIT module aliases are referenced by the
    // emitted arms so the `use` lines align with what's actually
    // needed. Each dispatch arm references `<module>::<func>`.
    let mut used_aliases: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for (entry, _fallible) in &scalar_entries {
        used_aliases
            .entry(entry.shape.wit_module.clone())
            .or_insert_with(|| entry.shape.wit_package.clone());
        // Some return shapes reference helper interfaces (BboxBlob
        // uses `pg_ctor::st_make_envelope`, IsValidDetailText uses
        // `pg_out::st_as_text`) and Enum returns reference the
        // declaring interface's alias. Register those aliases so
        // their `use` lines get emitted.
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
    // (postgis-raster-aggregates).
    for entry in &aggregate_entries {
        used_aliases
            .entry(entry.shape.wit_module.clone())
            .or_insert_with(|| entry.shape.wit_package.clone());
    }

    // Collect referenced records: any scalar with a WitValueRecord
    // (param or ret) / OptionWitValueRecord / FirstWitValueRecord /
    // ListRecord param contributes its record kebab-name to the set.
    // Only those records get per-record helpers emitted (otherwise
    // wit-bindgen's elision of unreferenced upstream types would
    // make the helpers fail to compile).
    let mut referenced_records: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for (entry, _f) in &scalar_entries {
        for p in &entry.shape.params {
            match p {
                interface_db::ParamShape::WitValueRecord { kebab_name, .. }
                | interface_db::ParamShape::ListRecord { kebab_name, .. } => {
                    referenced_records.insert(kebab_name.clone());
                }
                _ => {}
            }
        }
        match &entry.shape.ret {
            interface_db::RetShape::WitValueRecord { kebab_name, .. }
            | interface_db::RetShape::OptionWitValueRecord { kebab_name, .. }
            | interface_db::RetShape::FirstWitValueRecord { kebab_name, .. } => {
                referenced_records.insert(kebab_name.clone());
            }
            _ => {}
        }
    }
    let helper_records: Vec<RecordType> = records
        .iter()
        .filter(|r| referenced_records.contains(&r.kebab_name))
        .cloned()
        .collect();

    // Per-signature tuple-list helpers (one `parse_json_list_tuple_<sig>`
    // helper per unique element-list signature referenced by a
    // wired arm).
    let mut tuple_sigs: std::collections::BTreeSet<Vec<interface_db::ListPrimElem>> =
        std::collections::BTreeSet::new();
    for (entry, _f) in &scalar_entries {
        for p in &entry.shape.params {
            if let Some(sig) = p.list_tuple_sig() {
                tuple_sigs.insert(sig.to_vec());
            }
        }
    }

    // Discover whether the primary shim's WIT declares
    // geometry/raster/topology resource + matching error variant.
    // The per-resource helper bodies (`from_wkb`, `from_raster_binary`,
    // `from_topology_bytes` and their err formatters) are emitted only
    // when both halves are present so non-postgis shims stay slim.
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

    // wit-bindgen `additional_derives_ignore` list. Datafission
    // contract packages and helper-shim packages don't ship serde
    // impls (their variants/flags use macros that don't auto-derive),
    // so we explicitly exclude their types from the
    // Serialize/Deserialize derives. Primary-shim records — the ones
    // we WANT to round-trip — stay off the ignore list.
    let mut derives_ignore: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let datafission_pkgs = emit_wit::discover_datafission_packages()
        .unwrap_or_default();
    for pkg in &datafission_pkgs {
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
    for pkg in &shim_packages {
        if emit_wit::package_belongs_to_primary(&pkg.ns_name, primary) {
            // Primary-shim variants + flags can't derive serde out
            // of the box. Records stay off the ignore list — they're
            // exactly the types we WANT serde for.
            for v in &pkg.variants {
                derives_ignore.insert(v.kebab_name.clone());
            }
            for f in &pkg.flags {
                derives_ignore.insert(f.kebab_name.clone());
            }
        } else {
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
    }
    let derives_ignore_lits: String = derives_ignore
        .iter()
        .map(|n| format!("            \"{n}\",\n"))
        .collect::<Vec<_>>()
        .join("");

    let mut s = String::new();
    s.push_str(HEADER);
    s.push_str(&format!(
        r##"//! Generated by sqlink-shim-codegen (--target datafission).
//!
//! Scalar-first cut: scalars are wired against the canonical
//! datafission per-capability contract (version pins
//! auto-detected from the vendored WIT at codegen time);
//! aggregates, window functions, table functions,
//! multi-custom-type, spatial indexes, system catalog, and 1D
//! index plugin export stubs that advertise nothing and return
//! `Unknown*` / `Internal` errors on per-call paths. The
//! single-type `type-plugin/custom-type` interface is
//! intentionally NOT exported — components register types
//! through `multi-custom-type` instead. See AGENTS.md (in
//! datalink/crates/datalink-shim-datafission-emit) for the
//! migration plan that landed this target.

#![allow(unused_imports, dead_code)]
// wit-bindgen 0.41's generated bindings call unsafe fns outside
// explicit unsafe blocks — fine pre-2024 but warned in edition 2024.
// Datafission's hand-written postgis extension takes the same
// posture; mute it here so the generated bridge stays warning-free.
#![allow(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::format;
use alloc::string::{{String, ToString}};
use alloc::vec::Vec;

mod bindings {{
    wit_bindgen::generate!({{
        path: "wit",
        world: "bridge",
        generate_all,
        // Per-shape arms: derive serde::Serialize + Deserialize on
        // every wit-bindgen-generated record so ciborium can ferry
        // upstream records through the WTV magic-prefix Binary
        // envelope. The contract / helper-shim types listed in
        // `additional_derives_ignore` don't ship serde impls (their
        // variants/flags use macros that don't auto-derive); only
        // primary-shim records get the derives.
        additional_derives: [serde::Serialize, serde::Deserialize],
        additional_derives_ignore: [
{derives_ignore_lits}        ],
    }});
}}

// Datafission's composite world routes every per-registry type
// through `datafission:function-plugin/types` (ScalarValue /
// LogicalType / FunctionError). The other plugin packages each
// declare their own `types` interface; the scalar-first cut
// names them explicitly when stubbing the corresponding Guest
// impl.
use bindings::datafission::function_plugin::types as ftypes;
use bindings::datafission::sql_extension_plugin::types as setypes;
use bindings::datafission::type_plugin::types as ttypes;
use bindings::datafission::spatial_index_plugin::types as sitypes;
use bindings::datafission::system_catalog_plugin::types as sctypes;
use bindings::datafission::index_plugin::types as ixtypes;

use bindings::exports::datafission::extension::identity;
use bindings::exports::datafission::sql_extension_plugin::metadata;
use bindings::exports::datafission::function_plugin::scalar_function_registry;
use bindings::exports::datafission::function_plugin::aggregate_function_registry;
use bindings::exports::datafission::function_plugin::window_function_registry;
use bindings::exports::datafission::function_plugin::table_function_registry;
use bindings::exports::datafission::type_plugin::multi_custom_type;
use bindings::exports::datafission::spatial_index_plugin::spatial_index;
use bindings::exports::datafission::system_catalog_plugin::system_catalog;
use bindings::exports::datafission::index_plugin::index;

// The dispatch arms render `types::FunctionError::ExecutionError`
// against the function-plugin types alias. Local `types` re-exports
// the function-plugin set so the per-arm bodies stay short.
use bindings::datafission::function_plugin::types as types;

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
    // Per-resource `use` lines for the primary shim's
    // Geometry/Raster/Topology + matching error variants. Each is
    // emitted only when the WIT declares both the resource and the
    // error type so non-postgis shims stay slim.
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

    s.push_str(SCALARVALUE_HELPERS);

    // Compose the helper-prelude body from the per-resource flags
    // computed above. Each is independent: a postgis bridge gets
    // all three (geometry + raster + topology); a raster-only shim
    // would get just the raster helpers; etc.
    if shim_has_geometry_resource && shim_has_postgis_error {
        s.push_str(POSTGIS_HELPERS_BODY);
    }
    if shim_has_raster_resource && shim_has_raster_error {
        if let Some(p) = shim_pkg.as_ref() {
            let (pkg_ns, pkg_name) = split_pkg(&p.ns_name);
            s.push_str(&render_raster_helpers(
                &sanitize_module(&pkg_ns),
                &sanitize_module(&pkg_name),
            ));
        }
    }
    if shim_has_topology_resource && shim_has_topology_error {
        if let Some(p) = shim_pkg.as_ref() {
            let (pkg_ns, pkg_name) = split_pkg(&p.ns_name);
            s.push_str(&render_topology_helpers(
                &sanitize_module(&pkg_ns),
                &sanitize_module(&pkg_name),
            ));
        }
    }
    s.push_str(JSON_LIST_PRIM_HELPERS);
    s.push_str(&render_tuple_list_helpers(&tuple_sigs));
    if !helper_records.is_empty() {
        s.push_str(&emit_wit_value_helpers(&helper_records));
    }

    s.push_str("struct Component;\n\n");

    // ---- identity ----
    s.push_str(&format!(
        r##"impl identity::Guest for Component {{
    fn name() -> String {{ "{primary}".into() }}
    fn version() -> String {{ "{version}".into() }}
    fn api_version() -> String {{ "{api_version}".into() }}
}}

"##,
        primary = primary,
        version = version,
        api_version = api_version,
    ));

    // ---- sql-extension-plugin/metadata: empty lists ----
    s.push_str(&format!(
        r##"impl metadata::Guest for Component {{
    fn name() -> String {{ "{primary}".into() }}
    fn version() -> String {{ "{version}".into() }}

    fn list_cast_rewrites() -> Result<Vec<setypes::CastRewrite>, setypes::SqlExtError> {{
        Ok(Vec::new())
    }}
    fn list_operator_rewrites() -> Result<Vec<setypes::OperatorRewrite>, setypes::SqlExtError> {{
        Ok(Vec::new())
    }}
    fn list_preprocessor_patterns() -> Result<Vec<setypes::PreprocessorPattern>, setypes::SqlExtError> {{
        Ok(Vec::new())
    }}
}}

"##,
        primary = primary,
        version = version,
    ));

    // ---- scalar-function-registry: WIRED ----
    // Build the per-name dispatch + metadata data.
    let scalar_block = build_scalar_registry_impl(&scalar_entries, plan);
    s.push_str(&scalar_block);

    // ---- aggregate-function-registry: WIRED ----
    // When any aggregate is classified, emit the AccState prelude
    // and the dispatching impl. Empty surface falls back to the
    // original stub (advertises nothing, every per-call returns
    // UnknownFunction) so an unwired bridge still compiles.
    if aggregate_entries.is_empty() {
        s.push_str(AGGREGATE_STUB);
    } else {
        s.push_str(AGGREGATE_STATE_BLOCK);
        s.push_str(&build_aggregate_registry_impl(&aggregate_entries, plan));
    }

    // ---- window-function-registry: stub ----
    s.push_str(WINDOW_STUB);

    // ---- table-function-registry: stub ----
    s.push_str(TABLE_STUB);

    // ---- multi-custom-type: stub ----
    // Note: the single-type `type-plugin/custom-type` interface is
    // intentionally NOT exported by the generated world (see
    // `emit_wit::render_world`). Components register types through
    // `multi-custom-type` instead.
    s.push_str(MULTI_CUSTOM_TYPE_STUB);

    // ---- spatial-index: stub ----
    s.push_str(&format!(
        r##"impl spatial_index::Guest for Component {{
    fn name() -> String {{ "{primary}-stub-spatial".into() }}
    fn aliases() -> Vec<String> {{ Vec::new() }}
    fn capabilities() -> sitypes::IndexCapabilities {{
        sitypes::IndexCapabilities {{
            knn: false,
            within_distance: false,
            within_distance_wkb: false,
            update_after_build: false,
        }}
    }}
    fn build(_items: Vec<sitypes::BuildItem>) -> Result<u64, sitypes::SpatialError> {{
        Err(sitypes::SpatialError::UnsupportedOperation(
            "spatial-index not wired in scalar-first cut".into(),
        ))
    }}
    fn entry_count(_handle: u64) -> u64 {{ 0 }}
    fn query_envelope(_handle: u64, _env: sitypes::Envelope) -> Result<Vec<u64>, sitypes::SpatialError> {{
        Err(sitypes::SpatialError::UnsupportedOperation(
            "spatial-index not wired in scalar-first cut".into(),
        ))
    }}
    fn query_knn(_handle: u64, _query_bytes: Vec<u8>, _k: u32) -> Result<Vec<u64>, sitypes::SpatialError> {{
        Err(sitypes::SpatialError::UnsupportedOperation(
            "spatial-index not wired in scalar-first cut".into(),
        ))
    }}
    fn query_within_distance(_handle: u64, _query_env: sitypes::Envelope, _distance: f64) -> Result<Vec<u64>, sitypes::SpatialError> {{
        Err(sitypes::SpatialError::UnsupportedOperation(
            "spatial-index not wired in scalar-first cut".into(),
        ))
    }}
    fn query_within_distance_wkb(_handle: u64, _query_wkb: Vec<u8>, _distance: f64) -> Result<Vec<u64>, sitypes::SpatialError> {{
        Err(sitypes::SpatialError::UnsupportedOperation(
            "spatial-index not wired in scalar-first cut".into(),
        ))
    }}
    fn destroy(_handle: u64) {{}}
}}

"##,
        primary = primary,
    ));

    // ---- system-catalog: stub ----
    s.push_str(&format!(
        r##"impl system_catalog::Guest for Component {{
    fn catalog_name() -> String {{ "{primary}".into() }}
    fn list_tables() -> Result<Vec<sctypes::SystemTable>, sctypes::CatalogError> {{
        Ok(Vec::new())
    }}
    fn read_table(table_name: String) -> Result<
        Vec<Vec<sctypes::ScalarValue>>, sctypes::CatalogError,
    > {{
        Err(sctypes::CatalogError::UnknownTable(table_name))
    }}
    fn notify_extension_column_added(
        _session_id: u64,
        _catalog: String,
        _schema: String,
        _table_name: String,
        _column_name: String,
        _type_id: u32,
        _srid: Option<i32>,
        _coord_dim: Option<i32>,
    ) {{}}
    fn notify_extension_column_removed(
        _session_id: u64,
        _catalog: String,
        _schema: String,
        _table_name: String,
        _column_name: String,
    ) {{}}
    fn read_table_for_session(
        _session_id: u64,
        table_name: String,
    ) -> Result<Vec<Vec<sctypes::ScalarValue>>, sctypes::CatalogError> {{
        Err(sctypes::CatalogError::UnknownTable(table_name))
    }}
    fn notify_extension_column_raster_metadata(
        _session_id: u64,
        _catalog: String,
        _schema: String,
        _table_name: String,
        _column_name: String,
        _metadata: sctypes::RasterColumnMetadata,
    ) {{}}
}}

"##,
        primary = primary,
    ));

    // ---- index-plugin: stub ----
    s.push_str(&format!(
        r##"impl index::Guest for Component {{
    fn name() -> String {{ "{primary}-stub-index".into() }}
    fn type_id() -> u32 {{ 0 }}
    fn supported_types() -> Vec<ixtypes::LogicalType> {{ Vec::new() }}
    fn capabilities() -> ixtypes::IndexCapabilities {{
        ixtypes::IndexCapabilities {{
            point_lookup: false,
            range_scan: false,
            prefix_scan: false,
            ordering: false,
            spatial_search: false,
            fulltext_search: false,
            approximate_membership: false,
        }}
    }}
    fn create(_options: Vec<(String, String)>) -> Result<u64, ixtypes::IndexError> {{
        Err(unimpl_index_err())
    }}
    fn destroy(_handle: u64) {{}}
    fn insert(_handle: u64, _key: Vec<ixtypes::ScalarValue>, _row_id: u64) -> Result<(), ixtypes::IndexError> {{
        Err(unimpl_index_err())
    }}
    fn delete(_handle: u64, _key: Vec<ixtypes::ScalarValue>) -> Result<bool, ixtypes::IndexError> {{
        Err(unimpl_index_err())
    }}
    fn contains(_handle: u64, _key: Vec<ixtypes::ScalarValue>) -> Result<bool, ixtypes::IndexError> {{
        Err(unimpl_index_err())
    }}
    fn get(_handle: u64, _key: Vec<ixtypes::ScalarValue>) -> Result<Option<u64>, ixtypes::IndexError> {{
        Err(unimpl_index_err())
    }}
    fn begin_scan(
        _handle: u64,
        _start: ixtypes::ScanBound,
        _end: ixtypes::ScanBound,
        _direction: ixtypes::ScanDirection,
        _limit: Option<u64>,
    ) -> Result<u64, ixtypes::IndexError> {{
        Err(unimpl_index_err())
    }}
    fn next(_cursor: u64) -> Option<Result<ixtypes::IndexEntry, ixtypes::IndexError>> {{
        None
    }}
    fn close_scan(_cursor: u64) {{}}
    fn stats(_handle: u64) -> Result<ixtypes::IndexStats, ixtypes::IndexError> {{
        Err(unimpl_index_err())
    }}
    fn bulk_load(_handle: u64, _entries: Vec<ixtypes::IndexEntry>) -> Result<(), ixtypes::IndexError> {{
        Err(unimpl_index_err())
    }}
    fn serialize(_handle: u64) -> Result<Vec<u8>, ixtypes::IndexError> {{
        Err(unimpl_index_err())
    }}
    fn deserialize(_data: Vec<u8>) -> Result<u64, ixtypes::IndexError> {{
        Err(unimpl_index_err())
    }}
}}

fn unimpl_index_err() -> ixtypes::IndexError {{
    ixtypes::IndexError::Internal(
        "1D index not implemented in this {primary} bridge (scalar-first cut)".into(),
    )
}}

"##,
        primary = primary,
    ));

    // Export the Component as the world's entry point.
    s.push_str("bindings::export!(Component with_types_in bindings);\n");

    let _ = crate_name; // surface in README/CARGO; not used in lib.rs body
    Ok(s)
}

/// Build the scalar_function_registry Guest impl block. Walks the
/// post-classifier `scalar_entries` to materialize:
///   * per-name `list_functions()` metadata entries (with the
///     ParamShape-derived `param_types` widths the planner sees)
///   * per-name `return_type` dispatch arms (RetShape → LogicalType)
///   * per-name `execute` dispatch arms (the marshalling body)
fn build_scalar_registry_impl(
    scalar_entries: &[(interface_db::DispatchEntry, bool)],
    plan: &BridgePlan,
) -> String {
    // Collect determinism + propagates-null from the BridgePlan
    // side (the post-classifier DispatchEntry doesn't carry those).
    let mut det: std::collections::HashMap<String, (bool, bool)> =
        std::collections::HashMap::new();
    for ext in &plan.extensions {
        for sc in &ext.scalars {
            det.insert(
                sc.canonical_name.clone(),
                (sc.is_deterministic, sc.propagates_null),
            );
            for alias in &sc.aliases {
                det.insert(alias.clone(), (sc.is_deterministic, sc.propagates_null));
            }
        }
    }

    // First pass: build deduped sql_name list (mirrors the per-arm
    // walk pattern from duckdb-emit's build_scalar_arms — a SQL
    // name might surface multiple times if the entry classifier
    // produces an alias+canonical pair; first writer wins).
    let mut metas_block = String::new();
    let mut return_arms = String::new();
    let mut execute_arms = String::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (entry, fallible) in scalar_entries {
        if !seen.insert(entry.sql_name.clone()) {
            continue;
        }
        let (deterministic, propagates_null) =
            det.get(&entry.sql_name).copied().unwrap_or((true, true));

        // ---- metadata entry ----
        let mut sig_block = String::new();
        sig_block.push_str("vec![");
        for p in &entry.shape.params {
            let lt = dispatch::paramshape_to_logicaltype(p);
            sig_block.push_str(&lt);
            sig_block.push_str(", ");
        }
        sig_block.push(']');
        let escaped = entry.sql_name.replace('"', "\\\"");
        metas_block.push_str(&format!(
            "        ftypes::ScalarFunctionMeta {{\n\
             \x20           name: \"{escaped}\".to_string(),\n\
             \x20           aliases: Vec::new(),\n\
             \x20           param_types: vec![{sig_block}],\n\
             \x20           is_deterministic: {deterministic},\n\
             \x20           propagates_null: {propagates_null},\n\
             \x20       }},\n",
            escaped = escaped,
            sig_block = sig_block,
            deterministic = deterministic,
            propagates_null = propagates_null,
        ));

        // ---- return_type arm ----
        let ret_logical = dispatch::retshape_to_logicaltype(&entry.shape.ret);
        return_arms.push_str(&format!(
            "            \"{escaped}\" => Ok({ret_logical}),\n",
        ));

        // ---- execute arm ----
        let body = dispatch::emit_scalar_arm_body(
            &entry.shape,
            *fallible,
            &entry.sql_name,
            "                ",
        );
        execute_arms.push_str(&format!(
            "            \"{escaped}\" => {{\n{body}\n            }}\n",
        ));
    }

    format!(
        r##"impl scalar_function_registry::Guest for Component {{
    fn list_functions() -> Vec<ftypes::ScalarFunctionMeta> {{
        vec![
{metas_block}        ]
    }}

    fn return_type(
        name: String,
        _input_types: Vec<ftypes::LogicalType>,
    ) -> Result<ftypes::LogicalType, ftypes::FunctionError> {{
        match name.as_str() {{
{return_arms}            other => Err(ftypes::FunctionError::UnknownFunction(other.into())),
        }}
    }}

    fn execute(
        name: String,
        args: Vec<ftypes::ScalarValue>,
    ) -> Result<ftypes::ScalarValue, ftypes::FunctionError> {{
        // Datafission's host can either short-circuit NULL inputs
        // engine-side or push them through to the guest. PostGIS
        // scalars uniformly propagate; for safety we short-circuit
        // here too so the marshalling helpers never see a Null arm.
        if args.iter().any(|v| matches!(v, ftypes::ScalarValue::Null)) {{
            return Ok(ftypes::ScalarValue::Null);
        }}
        match name.as_str() {{
{execute_arms}            other => Err(ftypes::FunctionError::UnknownFunction(other.into())),
        }}
    }}

    fn execute_batch(
        name: String,
        args_batch: Vec<Vec<ftypes::ScalarValue>>,
    ) -> Result<Vec<ftypes::ScalarValue>, ftypes::FunctionError> {{
        let mut out = Vec::with_capacity(args_batch.len());
        for row in args_batch {{
            out.push(<Self as scalar_function_registry::Guest>::execute(name.clone(), row)?);
        }}
        Ok(out)
    }}
}}

"##,
        metas_block = metas_block,
        return_arms = return_arms,
        execute_arms = execute_arms,
    )
}

/// Build the `aggregate_function_registry::Guest` impl block.
/// Emits per-name metadata, return_type arm, the
/// create-accumulator family (passes the name into the per-handle
/// AccState), `accumulate` (pushes the row's blob arg), `merge`
/// (appends source blobs into target), and `finalize` (decodes
/// the accumulated blobs and calls the upstream WIT aggregate).
///
/// The thread-local accumulator state lives in the
/// `AGGREGATE_STATE_BLOCK` prelude emitted alongside this impl.
fn build_aggregate_registry_impl(
    agg_entries: &[interface_db::AggregateEntry],
    _plan: &BridgePlan,
) -> String {
    // Walk aggregates with first-writer-wins dedupe over sql_name.
    // The host calls all of {create_accumulator, accumulate, ...,
    // finalize} by NAME — but our finalize body needs to know
    // which decode/upstream-call recipe to run, so we assign each
    // unique name an arm_idx and store it on the AccState. Aliases
    // resolve to the same arm_idx as their canonical.
    let mut arm_for: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut next_arm: usize = 0;
    for entry in agg_entries {
        arm_for.entry(entry.sql_name.clone()).or_insert_with(|| {
            let i = next_arm;
            next_arm += 1;
            i
        });
    }

    let mut metas_block = String::new();
    let mut return_arms = String::new();
    let mut create_arms = String::new();
    let mut finalize_arms = String::new();
    let mut emitted_finalize: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    let mut seen_meta: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for entry in agg_entries {
        let escaped = entry.sql_name.replace('"', "\\\"");
        let arm_idx = *arm_for.get(&entry.sql_name).unwrap();

        if seen_meta.insert(entry.sql_name.clone()) {
            // ---- metadata entry ----
            let mut sig_block = String::new();
            sig_block.push_str("vec![vec![");
            // Streaming accumulator arg signature: Blob (WKB or
            // raster binary).
            sig_block.push_str("ftypes::LogicalType::Binary, ");
            for p in &entry.shape.extra_args {
                let lt = dispatch::paramshape_to_logicaltype(p);
                sig_block.push_str(&lt);
                sig_block.push_str(", ");
            }
            sig_block.push_str("]]");
            // config-arg-indices: positions 1..=extras_len map to
            // the constant config args (after the streaming arg).
            let mut cfg_indices = String::new();
            for j in 0..entry.shape.extra_args.len() {
                let i1 = j + 1;
                cfg_indices.push_str(&format!("{i1}u32, "));
            }
            let accepts_config = !entry.shape.extra_args.is_empty();
            metas_block.push_str(&format!(
                "        ftypes::AggregateFunctionMeta {{\n\
                 \x20           name: \"{escaped}\".to_string(),\n\
                 \x20           aliases: Vec::new(),\n\
                 \x20           param_types: {sig_block},\n\
                 \x20           supports_grouped: true,\n\
                 \x20           supports_partial: true,\n\
                 \x20           is_order_sensitive: false,\n\
                 \x20           accepts_config: {accepts_config},\n\
                 \x20           config_arg_indices: vec![{cfg_indices}],\n\
                 \x20       }},\n",
            ));

            let ret_logical =
                dispatch::aggregate_ret_logicaltype(&entry.shape);
            return_arms.push_str(&format!(
                "            \"{escaped}\" => Ok({ret_logical}),\n",
            ));

            create_arms.push_str(&format!(
                "            \"{escaped}\" => {arm_idx}usize,\n",
            ));
        }

        if emitted_finalize.insert(arm_idx) {
            let body = dispatch::emit_aggregate_finalize_body(
                &entry.shape,
                &entry.sql_name,
                "                ",
            );
            finalize_arms.push_str(&format!(
                "            {arm_idx}usize => {{\n{body}\n            }}\n",
            ));
        }
    }

    format!(
        r##"impl aggregate_function_registry::Guest for Component {{
    fn list_functions() -> Vec<ftypes::AggregateFunctionMeta> {{
        vec![
{metas_block}        ]
    }}

    fn return_type(
        name: String,
        _input_types: Vec<ftypes::LogicalType>,
    ) -> Result<ftypes::LogicalType, ftypes::FunctionError> {{
        match name.as_str() {{
{return_arms}            other => Err(ftypes::FunctionError::UnknownFunction(other.into())),
        }}
    }}

    fn create_accumulator(name: String) -> Result<u64, ftypes::FunctionError> {{
        let arm = match name.as_str() {{
{create_arms}            other => return Err(ftypes::FunctionError::UnknownFunction(other.into())),
        }};
        Ok(alloc_accumulator(arm, Vec::new()))
    }}

    fn create_accumulator_with_config(
        name: String,
        config: String,
    ) -> Result<u64, ftypes::FunctionError> {{
        let arm = match name.as_str() {{
{create_arms}            other => return Err(ftypes::FunctionError::UnknownFunction(other.into())),
        }};
        Ok(alloc_accumulator(arm, alloc::vec![config]))
    }}

    fn create_accumulator_with_configs(
        name: String,
        configs: Vec<String>,
    ) -> Result<u64, ftypes::FunctionError> {{
        let arm = match name.as_str() {{
{create_arms}            other => return Err(ftypes::FunctionError::UnknownFunction(other.into())),
        }};
        Ok(alloc_accumulator(arm, configs))
    }}

    fn accumulate(
        handle: u64,
        value: ftypes::ScalarValue,
    ) -> Result<(), ftypes::FunctionError> {{
        ACCUMULATORS.with(|m| {{
            let mut g = m.borrow_mut();
            let st = g.get_mut(&handle).ok_or_else(|| {{
                ftypes::FunctionError::ExecutionError(format!(
                    "no accumulator at handle {{}}", handle
                ))
            }})?;
            // Null streaming inputs are skipped (SQL aggregate
            // semantics: NULL contributions don't change the result).
            if matches!(value, ftypes::ScalarValue::Null) {{
                return Ok(());
            }}
            let bytes = match value {{
                ftypes::ScalarValue::Binary(b) => b,
                ftypes::ScalarValue::Utf8(s) => s.into_bytes(),
                _ => return Err(ftypes::FunctionError::TypeError(
                    "aggregate streaming arg must be BINARY".into()
                )),
            }};
            st.blobs.push(bytes);
            Ok(())
        }})
    }}

    fn accumulate_batch(
        handle: u64,
        values: Vec<ftypes::ScalarValue>,
    ) -> Result<(), ftypes::FunctionError> {{
        for v in values {{
            <Self as aggregate_function_registry::Guest>::accumulate(handle, v)?;
        }}
        Ok(())
    }}

    fn merge(
        target: u64,
        source: u64,
    ) -> Result<(), ftypes::FunctionError> {{
        ACCUMULATORS.with(|m| {{
            let mut g = m.borrow_mut();
            let src = g.remove(&source).ok_or_else(|| {{
                ftypes::FunctionError::ExecutionError(format!(
                    "merge: source handle {{}} not found", source
                ))
            }})?;
            let tgt = g.get_mut(&target).ok_or_else(|| {{
                ftypes::FunctionError::ExecutionError(format!(
                    "merge: target handle {{}} not found", target
                ))
            }})?;
            if tgt.arm_idx != src.arm_idx {{
                return Err(ftypes::FunctionError::ExecutionError(
                    "merge: target and source must come from the same aggregate".into()
                ));
            }}
            tgt.blobs.extend(src.blobs);
            Ok(())
        }})
    }}

    fn finalize(handle: u64) -> Result<ftypes::ScalarValue, ftypes::FunctionError> {{
        // Take the accumulator out — finalize consumes it
        // (subsequent calls on the same handle return UnknownFunction).
        let st = ACCUMULATORS.with(|m| m.borrow_mut().remove(&handle)).ok_or_else(|| {{
            ftypes::FunctionError::ExecutionError(format!(
                "finalize: no accumulator at handle {{}}", handle
            ))
        }})?;
        match st.arm_idx {{
{finalize_arms}            other => Err(ftypes::FunctionError::ExecutionError(format!(
                "finalize: unknown aggregate arm {{}}", other
            ))),
        }}
    }}

    fn reset(handle: u64) {{
        ACCUMULATORS.with(|m| {{
            if let Some(st) = m.borrow_mut().get_mut(&handle) {{
                st.blobs.clear();
            }}
        }});
    }}

    fn destroy_accumulator(handle: u64) {{
        ACCUMULATORS.with(|m| {{
            m.borrow_mut().remove(&handle);
        }});
    }}
}}

"##,
    )
}

/// Per-handle accumulator state + thread-local map. Emitted into
/// the bridge's lib.rs prelude once when any aggregate is wired.
const AGGREGATE_STATE_BLOCK: &str = r##"// ─── Aggregate accumulator state ───
//
// Datafission's aggregate_function_registry@1.0.0 contract is
// handle-based: create-accumulator returns u64, subsequent
// accumulate/merge/finalize calls reference that handle. The
// bridge keeps a thread-local `BTreeMap<u64, AccState>` keyed by
// handle, with a monotonic u64 counter for fresh handles. Each
// AccState carries: which aggregate (arm_idx) it represents, the
// raw blobs accumulated via `accumulate`, and any constant
// configs passed at create-accumulator-with-configs time.
//
// thread_local because wit-bindgen's guest impl is per-instance
// and each component instance handles its own host calls in its
// own thread.

use core::cell::{Cell, RefCell};
use alloc::collections::BTreeMap;

#[derive(Clone)]
struct AccState {
    arm_idx: usize,
    blobs: Vec<Vec<u8>>,
    extras: Vec<String>,
}

thread_local! {
    static ACCUMULATORS: RefCell<BTreeMap<u64, AccState>> =
        RefCell::new(BTreeMap::new());
    static NEXT_ACC_HANDLE: Cell<u64> = const { Cell::new(1) };
}

fn alloc_accumulator(arm_idx: usize, extras: Vec<String>) -> u64 {
    let h = NEXT_ACC_HANDLE.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    ACCUMULATORS.with(|m| {
        m.borrow_mut().insert(h, AccState {
            arm_idx,
            blobs: Vec::new(),
            extras,
        });
    });
    h
}

"##;

const HEADER: &str =
    "// === GENERATED by sqlink-shim-codegen (target=datafission)  do not edit by hand ===\n\n";

/// ScalarValue ↔ Rust helpers ferried into the bridge's lib.rs
/// prelude. Mirror of the `arg_text` / `arg_blob` / `arg_f64` /
/// `arg_i64` set in sqlite-emit but unpacking from
/// `ftypes::ScalarValue` arms. Datafission's value variant is the
/// widest of the three (separate signed/unsigned int arms + date /
/// time / timestamp specialisations), all coerced into the same
/// common Rust primitive via the four helpers below.
const SCALARVALUE_HELPERS: &str = r##"// ─── ScalarValue arg helpers ───
//
// Mirror of sqlite-emit's `arg_text` / `arg_blob` / `arg_i64` set
// and duckdb-emit's `dv_*` set but unpacking from
// `ftypes::ScalarValue` arms. The datafission variant is the
// widest of the three (split signed/unsigned int arms + date /
// time / timestamp specialisations); helpers coerce to a common
// Rust primitive.

fn dfv_text<'a>(args: &'a [ftypes::ScalarValue], idx: usize, name: &str) -> Result<&'a str, ftypes::FunctionError> {
    match args.get(idx) {
        Some(ftypes::ScalarValue::Utf8(s)) => Ok(s.as_str()),
        _ => Err(ftypes::FunctionError::TypeError(format!(
            "{name}: arg {idx} must be UTF8"
        ))),
    }
}

fn dfv_blob<'a>(args: &'a [ftypes::ScalarValue], idx: usize, name: &str) -> Result<&'a [u8], ftypes::FunctionError> {
    match args.get(idx) {
        Some(ftypes::ScalarValue::Binary(b)) => Ok(b.as_slice()),
        Some(ftypes::ScalarValue::Utf8(s)) => Ok(s.as_bytes()),
        _ => Err(ftypes::FunctionError::TypeError(format!(
            "{name}: arg {idx} must be BINARY"
        ))),
    }
}

fn dfv_f64(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<f64, ftypes::FunctionError> {
    match args.get(idx) {
        Some(ftypes::ScalarValue::Float64(v)) => Ok(*v),
        Some(ftypes::ScalarValue::Float32(v)) => Ok(*v as f64),
        Some(ftypes::ScalarValue::Int64(v)) => Ok(*v as f64),
        Some(ftypes::ScalarValue::Int32(v)) => Ok(*v as f64),
        Some(ftypes::ScalarValue::Uint64(v)) => Ok(*v as f64),
        Some(ftypes::ScalarValue::Uint32(v)) => Ok(*v as f64),
        Some(ftypes::ScalarValue::Int16(v)) => Ok(*v as f64),
        Some(ftypes::ScalarValue::Int8(v)) => Ok(*v as f64),
        Some(ftypes::ScalarValue::Uint16(v)) => Ok(*v as f64),
        Some(ftypes::ScalarValue::Uint8(v)) => Ok(*v as f64),
        _ => Err(ftypes::FunctionError::TypeError(format!(
            "{name}: arg {idx} must be FLOAT"
        ))),
    }
}

fn dfv_i64(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<i64, ftypes::FunctionError> {
    match args.get(idx) {
        Some(ftypes::ScalarValue::Int64(v)) => Ok(*v),
        Some(ftypes::ScalarValue::Int32(v)) => Ok(*v as i64),
        Some(ftypes::ScalarValue::Uint64(v)) => Ok(*v as i64),
        Some(ftypes::ScalarValue::Uint32(v)) => Ok(*v as i64),
        Some(ftypes::ScalarValue::Int16(v)) => Ok(*v as i64),
        Some(ftypes::ScalarValue::Int8(v)) => Ok(*v as i64),
        Some(ftypes::ScalarValue::Uint16(v)) => Ok(*v as i64),
        Some(ftypes::ScalarValue::Uint8(v)) => Ok(*v as i64),
        Some(ftypes::ScalarValue::Boolean(b)) => Ok(if *b { 1 } else { 0 }),
        Some(ftypes::ScalarValue::Float64(v)) => Ok(*v as i64),
        Some(ftypes::ScalarValue::Float32(v)) => Ok(*v as i64),
        Some(ftypes::ScalarValue::Date(v)) => Ok(*v as i64),
        Some(ftypes::ScalarValue::Time(v)) => Ok(*v),
        Some(ftypes::ScalarValue::Timestamp(v)) => Ok(*v),
        _ => Err(ftypes::FunctionError::TypeError(format!(
            "{name}: arg {idx} must be INTEGER"
        ))),
    }
}

fn dfv_bool(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<bool, ftypes::FunctionError> {
    match args.get(idx) {
        Some(ftypes::ScalarValue::Boolean(b)) => Ok(*b),
        Some(ftypes::ScalarValue::Int64(v)) => Ok(*v != 0),
        Some(ftypes::ScalarValue::Int32(v)) => Ok(*v != 0),
        _ => Err(ftypes::FunctionError::TypeError(format!(
            "{name}: arg {idx} must be BOOLEAN"
        ))),
    }
}

/// Generic error formatter for fallible upstream WIT calls. The
/// dispatch arm wraps the upstream error via this fn into a
/// String the FunctionError::ExecutionError arm carries.
fn shim_err_string<E: core::fmt::Debug>(e: E) -> String {
    format!("{:?}", e)
}

"##;

/// Stub for aggregate_function_registry. Advertises nothing; every
/// per-call method returns `UnknownFunction`.
const AGGREGATE_STUB: &str = r##"impl aggregate_function_registry::Guest for Component {
    fn list_functions() -> Vec<ftypes::AggregateFunctionMeta> {
        Vec::new()
    }
    fn return_type(name: String, _input_types: Vec<ftypes::LogicalType>)
        -> Result<ftypes::LogicalType, ftypes::FunctionError>
    {
        Err(ftypes::FunctionError::UnknownFunction(name))
    }
    fn create_accumulator(name: String) -> Result<u64, ftypes::FunctionError> {
        Err(ftypes::FunctionError::UnknownFunction(name))
    }
    fn create_accumulator_with_config(name: String, _config: String) -> Result<u64, ftypes::FunctionError> {
        Err(ftypes::FunctionError::UnknownFunction(name))
    }
    fn create_accumulator_with_configs(name: String, _configs: Vec<String>) -> Result<u64, ftypes::FunctionError> {
        Err(ftypes::FunctionError::UnknownFunction(name))
    }
    fn accumulate(handle: u64, _value: ftypes::ScalarValue) -> Result<(), ftypes::FunctionError> {
        Err(ftypes::FunctionError::Internal(format!(
            "no accumulator at handle {handle} (aggregates not wired in scalar-first cut)"
        )))
    }
    fn accumulate_batch(handle: u64, _values: Vec<ftypes::ScalarValue>) -> Result<(), ftypes::FunctionError> {
        Err(ftypes::FunctionError::Internal(format!(
            "no accumulator at handle {handle} (aggregates not wired in scalar-first cut)"
        )))
    }
    fn merge(target: u64, _source: u64) -> Result<(), ftypes::FunctionError> {
        Err(ftypes::FunctionError::Internal(format!(
            "no accumulator at handle {target} (aggregates not wired in scalar-first cut)"
        )))
    }
    fn finalize(handle: u64) -> Result<ftypes::ScalarValue, ftypes::FunctionError> {
        Err(ftypes::FunctionError::Internal(format!(
            "no accumulator at handle {handle} (aggregates not wired in scalar-first cut)"
        )))
    }
    fn reset(_handle: u64) {}
    fn destroy_accumulator(_handle: u64) {}
}

"##;

const WINDOW_STUB: &str = r##"impl window_function_registry::Guest for Component {
    fn list_functions() -> Vec<ftypes::WindowFunctionMeta> {
        Vec::new()
    }
    fn return_type(name: String, _input_types: Vec<ftypes::LogicalType>)
        -> Result<ftypes::LogicalType, ftypes::FunctionError>
    {
        Err(ftypes::FunctionError::UnknownFunction(name))
    }
    fn compute_partition(
        name: String,
        _args_rows: Vec<Vec<ftypes::ScalarValue>>,
    ) -> Result<Vec<ftypes::ScalarValue>, ftypes::FunctionError> {
        Err(ftypes::FunctionError::UnknownFunction(name))
    }
}

"##;

const TABLE_STUB: &str = r##"impl table_function_registry::Guest for Component {
    fn list_functions() -> Vec<ftypes::TableFunctionMeta> {
        Vec::new()
    }
    fn output_schema(
        name: String,
        _input_types: Vec<ftypes::LogicalType>,
    ) -> Result<Vec<ftypes::ColumnInfo>, ftypes::FunctionError> {
        Err(ftypes::FunctionError::UnknownFunction(name))
    }
    fn begin(name: String, _args: Vec<ftypes::ScalarValue>) -> Result<u64, ftypes::FunctionError> {
        Err(ftypes::FunctionError::UnknownFunction(name))
    }
    fn next_row(_handle: u64) -> Option<Result<Vec<ftypes::ScalarValue>, ftypes::FunctionError>> {
        None
    }
    fn end(_handle: u64) {}
}

"##;

const MULTI_CUSTOM_TYPE_STUB: &str = r##"impl multi_custom_type::Guest for Component {
    fn list_types() -> Vec<multi_custom_type::CustomTypeMeta> {
        Vec::new()
    }
    fn serialize(_type_id: u32, value: Vec<u8>) -> Vec<u8> { value }
    fn deserialize(type_id: u32, _bytes: Vec<u8>) -> Result<Vec<u8>, ttypes::TypeError> {
        Err(ttypes::TypeError::Internal(format!(
            "no custom type at id {type_id} (multi-custom-type not wired in scalar-first cut)"
        )))
    }
    fn compare(_type_id: u32, _a: Vec<u8>, _b: Vec<u8>) -> ttypes::Ordering {
        ttypes::Ordering::Equal
    }
    fn hash_value(_type_id: u32, _value: Vec<u8>) -> u64 { 0 }
    fn display(type_id: u32, _value: Vec<u8>) -> String {
        format!("<type {type_id} (stub)>")
    }
    fn parse(type_id: u32, _input: String) -> Result<Vec<u8>, ttypes::TypeError> {
        Err(ttypes::TypeError::Internal(format!(
            "no custom type at id {type_id} (multi-custom-type not wired in scalar-first cut)"
        )))
    }
}

"##;

/// Discover which subdir of `wit_deps_root` holds the primary
/// shim's upstream WIT package. Same heuristic as sqlite-emit /
/// duckdb-emit's `pick_primary_shim_dir`.
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
                "sqlite-extension" | "duckdb-extension" | "sfcgal-component"
                | "extension" | "function-plugin" | "sql-extension-plugin"
                | "type-plugin" | "spatial-index-plugin"
                | "system-catalog-plugin" | "index-plugin" => continue,
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

/// Postgis-specific helpers — emitted only when the shim's WIT
/// declares `resource geometry` + `variant postgis-error`. For
/// non-postgis shims the dispatcher never references these so the
/// helpers can be omitted without breaking compilation.
const POSTGIS_HELPERS_BODY: &str = r#"
// ─── Postgis resource helpers ───

fn from_wkb(bytes: &[u8], name: &str) -> Result<Geometry, types::FunctionError> {
    Geometry::from_wkb(bytes).map_err(|e| {
        types::FunctionError::ExecutionError(format!("{name}: {}", postgis_err_string(e)))
    })
}

fn geog_from_wkb(bytes: &[u8], name: &str) -> Result<Geography, types::FunctionError> {
    Geography::from_wkb(bytes).map_err(|e| {
        types::FunctionError::ExecutionError(format!("{name}: {}", postgis_err_string(e)))
    })
}

/// Format a `postgis-error` variant back to a string the SQL
/// caller can read.
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

/// Raster prelude helpers. Mirror of POSTGIS_HELPERS_BODY for the
/// raster resource. Emitted only when the shim's WIT declares
/// `resource raster` + `variant raster-error`.
fn render_raster_helpers(pkg_ns: &str, pkg_name: &str) -> String {
    format!(
        r#"
// ─── Raster resource helpers ───

fn from_raster_binary(bytes: &[u8], name: &str) -> Result<Raster, types::FunctionError> {{
    bindings::{pkg_ns}::{pkg_name}::postgis_raster_types::from_binary(bytes)
        .map_err(|e| types::FunctionError::ExecutionError(
            format!("{{}}: {{}}", name, raster_err_string(e))))
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

/// Topology prelude helpers. Mirror of POSTGIS_HELPERS_BODY for
/// the topology resource. Emitted only when the shim's WIT
/// declares `resource topology` + `variant topology-error`.
fn render_topology_helpers(pkg_ns: &str, pkg_name: &str) -> String {
    format!(
        r#"
// ─── Topology resource helpers ───

fn from_topology_bytes(bytes: &[u8], name: &str) -> Result<Topology, types::FunctionError> {{
    bindings::{pkg_ns}::{pkg_name}::postgis_topology_types::from_bytes(bytes)
        .map_err(|e| types::FunctionError::ExecutionError(
            format!("{{}}: {{}}", name, topology_err_string(e))))
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

/// Primitive `list<X>` param helpers. Each
/// `ParamShape::ListPrim(elem)` dispatch arm calls one of these
/// `parse_json_list_<T>` helpers. The SQL caller passes a JSON-array
/// literal in the TEXT arg; the helper decodes via serde_json into
/// a `Vec<T>` which the arm then passes to the WIT function as
/// `&[T]`.
const JSON_LIST_PRIM_HELPERS: &str = r##"
// ─── JSON-as-TEXT list<prim> helpers ───

#[allow(dead_code)]
fn parse_json_list_f64(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<Vec<f64>, types::FunctionError> {
    let text = dfv_text(args, idx, name)?;
    serde_json::from_str::<Vec<f64>>(text)
        .map_err(|e| types::FunctionError::ExecutionError(
            format!("{name}: arg {idx} must be JSON array of f64 ({e})")))
}

#[allow(dead_code)]
fn parse_json_list_i32(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<Vec<i32>, types::FunctionError> {
    let text = dfv_text(args, idx, name)?;
    serde_json::from_str::<Vec<i32>>(text)
        .map_err(|e| types::FunctionError::ExecutionError(
            format!("{name}: arg {idx} must be JSON array of s32 ({e})")))
}

#[allow(dead_code)]
fn parse_json_list_i64(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<Vec<i64>, types::FunctionError> {
    let text = dfv_text(args, idx, name)?;
    serde_json::from_str::<Vec<i64>>(text)
        .map_err(|e| types::FunctionError::ExecutionError(
            format!("{name}: arg {idx} must be JSON array of s64 ({e})")))
}

#[allow(dead_code)]
fn parse_json_list_u32(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<Vec<u32>, types::FunctionError> {
    let text = dfv_text(args, idx, name)?;
    serde_json::from_str::<Vec<u32>>(text)
        .map_err(|e| types::FunctionError::ExecutionError(
            format!("{name}: arg {idx} must be JSON array of u32 ({e})")))
}

#[allow(dead_code)]
fn parse_json_list_u64(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<Vec<u64>, types::FunctionError> {
    let text = dfv_text(args, idx, name)?;
    serde_json::from_str::<Vec<u64>>(text)
        .map_err(|e| types::FunctionError::ExecutionError(
            format!("{name}: arg {idx} must be JSON array of u64 ({e})")))
}

#[allow(dead_code)]
fn parse_json_list_u8(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<Vec<u8>, types::FunctionError> {
    let text = dfv_text(args, idx, name)?;
    serde_json::from_str::<Vec<u8>>(text)
        .map_err(|e| types::FunctionError::ExecutionError(
            format!("{name}: arg {idx} must be JSON array of u8 ({e})")))
}

#[allow(dead_code)]
fn parse_json_list_bool(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<Vec<bool>, types::FunctionError> {
    let text = dfv_text(args, idx, name)?;
    serde_json::from_str::<Vec<bool>>(text)
        .map_err(|e| types::FunctionError::ExecutionError(
            format!("{name}: arg {idx} must be JSON array of bool ({e})")))
}

#[allow(dead_code)]
fn parse_json_list_string(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<Vec<String>, types::FunctionError> {
    let text = dfv_text(args, idx, name)?;
    serde_json::from_str::<Vec<String>>(text)
        .map_err(|e| types::FunctionError::ExecutionError(
            format!("{name}: arg {idx} must be JSON array of string ({e})")))
}
"##;

/// Render each tuple-list helper into the bridge prelude. Each
/// helper:
///   - Reads the SQL TEXT arg as JSON.
///   - Parses via `serde_json::from_str::<Vec<(T1, T2, ...)>>` —
///     serde renders Rust tuples as fixed-length JSON arrays.
fn render_tuple_list_helpers(
    sigs: &std::collections::BTreeSet<Vec<interface_db::ListPrimElem>>,
) -> String {
    if sigs.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    s.push_str(
        "\n// ─── list<tuple<...>> param helpers ───\n\
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
                interface_db::ListPrimElem::F64 => "f64",
                interface_db::ListPrimElem::F32 => "f32",
                interface_db::ListPrimElem::S32 => "s32",
                interface_db::ListPrimElem::S64 => "s64",
                interface_db::ListPrimElem::U32 => "u32",
                interface_db::ListPrimElem::U64 => "u64",
                interface_db::ListPrimElem::U8 => "u8",
                interface_db::ListPrimElem::Bool => "bool",
                interface_db::ListPrimElem::String => "string",
            })
            .collect::<Vec<_>>()
            .join(", ");
        s.push_str(&format!(
            "#[allow(dead_code)]\n\
             fn parse_json_list_tuple_{suffix}(args: &[ftypes::ScalarValue], idx: usize, name: &str) -> Result<Vec<{rust_tuple}>, types::FunctionError> {{\n\
             \x20   let text = dfv_text(args, idx, name)?;\n\
             \x20   serde_json::from_str::<Vec<{rust_tuple}>>(text)\n\
             \x20       .map_err(|e| types::FunctionError::ExecutionError(\n\
             \x20           format!(\"{{name}}: arg {{idx}} must be JSON array of [{wit_label}] tuples ({{e}})\")))\n\
             }}\n\n",
        ));
    }
    s
}

/// Per-record WitValue marshaling helpers using the magic-prefix
/// Binary scheme. For each referenced record `R`, emits:
///
///   - `arg_witvalue_<snake>(args, idx, name) -> Result<UPSTREAM, FunctionError>`:
///     reads `args[idx]` as `ScalarValue::Binary`, verifies the
///     `b"WTV\x01"` magic + the baked-in 32-byte type_id, then
///     ciborium-decodes the payload tail directly into the upstream
///     record (records flagged `direct=false` instead surface an
///     ExecutionError noting the deferral).
///
///   - `ret_to_witvalue_<snake>(upstream: UPSTREAM) -> Result<ScalarValue, FunctionError>`:
///     ciborium-encodes the upstream record into a buffer prefixed
///     with the WTV magic + type_id; returns
///     `ScalarValue::Binary(buf)`.
///
///   - `parse_json_list_record_<snake>(args, idx, name) -> Result<Vec<UPSTREAM>, FunctionError>`:
///     JSON-array fallback for `ListRecord` params; the WIT-bindgen
///     `additional_derives: [serde::Deserialize]` makes UPSTREAM
///     deserialisable directly, so no LOCAL→UPSTREAM ciborium
///     round-trip is needed.
fn emit_wit_value_helpers(records: &[RecordType]) -> String {
    let mut s = String::new();
    s.push_str("\n// ─── WIT-value record marshaling helpers ───\n");
    s.push_str("// Per-record `arg_witvalue_<snake>` / `ret_to_witvalue_<snake>`\n");
    s.push_str("// over the WTV magic-prefix Binary envelope:\n");
    s.push_str("//   bytes[0..4]  = b\"WTV\\x01\"\n");
    s.push_str("//   bytes[4..36] = 32-byte sha256 type_id (baked per record)\n");
    s.push_str("//   bytes[36..]  = canonical-CBOR payload (ciborium).\n");
    s.push_str("//\n");
    s.push_str("// Records flagged `direct == false` decode UPSTREAM via a\n");
    s.push_str("// LOCAL → UPSTREAM ciborium round-trip — the LOCAL serde-ops\n");
    s.push_str("// codec the sqlite target ships isn't available on the\n");
    s.push_str("// datafission surface, so we surface those as ExecutionError\n");
    s.push_str("// with a self-describing message instead of restructuring\n");
    s.push_str("// codegen-core.\n\n");
    s.push_str("const WTV_MAGIC: [u8; 4] = *b\"WTV\\x01\";\n\n");

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
        let type_id_bytes: Vec<String> =
            r.type_id.iter().map(|b| format!("0x{:02x}", b)).collect();
        let type_id_lits = type_id_bytes.join(", ");
        let expected_hex: String = r
            .type_id
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join("");

        if r.direct {
            s.push_str(&format!(
                "#[allow(dead_code)]\n\
                 fn arg_witvalue_{snake}(\n\
                 \x20   args: &[ftypes::ScalarValue],\n\
                 \x20   idx: usize,\n\
                 \x20   name: &str,\n\
                 ) -> Result<{upstream_path}, types::FunctionError> {{\n\
                 \x20   let bytes = dfv_blob(args, idx, name)?;\n\
                 \x20   if bytes.len() < 36 || &bytes[..4] != &WTV_MAGIC {{\n\
                 \x20       return Err(types::FunctionError::ExecutionError(\n\
                 \x20           format!(\"{{name}}: arg {{idx}} not a WTV-framed wit-value\")));\n\
                 \x20   }}\n\
                 \x20   const EXPECTED_TYPE_ID: [u8; 32] = [{type_id_lits}];\n\
                 \x20   if &bytes[4..36] != &EXPECTED_TYPE_ID {{\n\
                 \x20       return Err(types::FunctionError::ExecutionError(\n\
                 \x20           format!(\"{{name}}: arg {{idx}} type_id mismatch (expected {expected_hex})\")));\n\
                 \x20   }}\n\
                 \x20   let payload = &bytes[36..];\n\
                 \x20   ciborium::de::from_reader::<{upstream_path}, _>(payload)\n\
                 \x20       .map_err(|e| types::FunctionError::ExecutionError(\n\
                 \x20           format!(\"{{name}}: decode arg {{idx}}: {{}}\", e)))\n\
                 }}\n\n",
            ));
        } else {
            // Non-direct: the LOCAL serde-ops codec isn't available
            // on the datafission target, so we report the deferral
            // explicitly at call time rather than silently producing
            // wrong bytes.
            s.push_str(&format!(
                "#[allow(dead_code)]\n\
                 fn arg_witvalue_{snake}(\n\
                 \x20   _args: &[ftypes::ScalarValue],\n\
                 \x20   _idx: usize,\n\
                 \x20   name: &str,\n\
                 ) -> Result<{upstream_path}, types::FunctionError> {{\n\
                 \x20   Err(types::FunctionError::ExecutionError(format!(\n\
                 \x20       \"{{name}}: wit-value record {snake} is non-direct (LOCAL serde-ops codec required but not available on the datafission surface)\")))\n\
                 }}\n\n",
            ));
        }

        // `parse_json_list_record_<snake>` for ListRecord params.
        s.push_str(&format!(
            "#[allow(dead_code)]\n\
             fn parse_json_list_record_{snake}(\n\
             \x20   args: &[ftypes::ScalarValue],\n\
             \x20   idx: usize,\n\
             \x20   name: &str,\n\
             ) -> Result<Vec<{upstream_path}>, types::FunctionError> {{\n\
             \x20   let text = dfv_text(args, idx, name)?;\n\
             \x20   serde_json::from_str::<Vec<{upstream_path}>>(text)\n\
             \x20       .map_err(|e| types::FunctionError::ExecutionError(\n\
             \x20           format!(\"{{name}}: arg {{idx}} must be JSON array of {kebab} ({{e}})\")))\n\
             }}\n\n",
            kebab = r.kebab_name,
        ));

        // Encoder: UPSTREAM → WTV-framed Binary.
        s.push_str(&format!(
            "#[allow(dead_code)]\n\
             fn ret_to_witvalue_{snake}(\n\
             \x20   upstream: {upstream_path},\n\
             ) -> Result<ftypes::ScalarValue, types::FunctionError> {{\n\
             \x20   let mut buf: Vec<u8> = alloc::vec::Vec::with_capacity(64);\n\
             \x20   buf.extend_from_slice(&WTV_MAGIC);\n\
             \x20   const TYPE_ID: [u8; 32] = [{type_id_lits}];\n\
             \x20   buf.extend_from_slice(&TYPE_ID);\n\
             \x20   ciborium::ser::into_writer(&upstream, &mut buf)\n\
             \x20       .map_err(|e| types::FunctionError::ExecutionError(\n\
             \x20           format!(\"encode {snake} wit-value: {{}}\", e)))?;\n\
             \x20   Ok(ftypes::ScalarValue::Binary(buf))\n\
             }}\n\n",
        ));
    }
    s
}
