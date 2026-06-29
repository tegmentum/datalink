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

/// Escape a string literal for direct emission inside a Rust
/// double-quoted string. Replaces `\` and `"` only — the inputs
/// here come from the interface DB (SQL identifiers, op tokens,
/// type names) where embedded newlines and other control chars
/// are not expected.
fn rust_str_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Map a BridgePlan `CastRewrite::source_kind` string (as stored in
/// the interface DB) to the matching `setypes::CastSourceKind`
/// enum variant. The DB stores values in `castsourcekind::<variant>`
/// form (the lowercased `Debug` print); newer shims may also send
/// the bare variant name. Unknown values fall back to `Any` so a
/// freshly-extracted DB with an unexpected discriminant still
/// compiles instead of refusing to regen.
fn cast_source_kind_variant(raw: &str) -> &'static str {
    let stripped = raw
        .rsplit("::")
        .next()
        .unwrap_or(raw)
        .to_ascii_lowercase();
    match stripped.as_str() {
        "stringliteral" | "string-literal" | "string_literal" => "StringLiteral",
        "geographycolumn" | "geography-column" | "geography_column" => "GeographyColumn",
        _ => "Any",
    }
}

/// Build the body of `metadata::Guest::list_cast_rewrites()` —
/// emits `Ok(vec![ setypes::CastRewrite { ... }, ... ])` from
/// `plan.extensions[*].cast_rewrites`. Empty plan yields the
/// original `Ok(Vec::new())` so byte-output for extensions with
/// no cast rewrites stays identical to the prior stub.
fn build_cast_rewrites_body(plan: &BridgePlan) -> String {
    let total: usize = plan.extensions.iter().map(|e| e.cast_rewrites.len()).sum();
    if total == 0 {
        return "        Ok(Vec::new())\n".to_string();
    }
    let mut s = String::new();
    s.push_str("        Ok(alloc::vec![\n");
    for ext in &plan.extensions {
        for c in &ext.cast_rewrites {
            let variant = cast_source_kind_variant(&c.source_kind);
            s.push_str(&format!(
                "            setypes::CastRewrite {{\n\
                 \x20               target_type: \"{tt}\".to_string(),\n\
                 \x20               function_name: \"{fn_}\".to_string(),\n\
                 \x20               source_fn_hint: \"{hint}\".to_string(),\n\
                 \x20               source_kind: setypes::CastSourceKind::{variant},\n\
                 \x20           }},\n",
                tt = rust_str_escape(&c.target_type),
                fn_ = rust_str_escape(&c.function_name),
                hint = rust_str_escape(&c.source_fn_hint),
                variant = variant,
            ));
        }
    }
    s.push_str("        ])\n");
    s
}

/// Build the body of `metadata::Guest::list_operator_rewrites()`.
fn build_operator_rewrites_body(plan: &BridgePlan) -> String {
    let total: usize = plan.extensions.iter().map(|e| e.operators.len()).sum();
    if total == 0 {
        return "        Ok(Vec::new())\n".to_string();
    }
    let mut s = String::new();
    s.push_str("        Ok(alloc::vec![\n");
    for ext in &plan.extensions {
        for op in &ext.operators {
            let lhs = match op.lhs_type_id {
                Some(id) => format!("Some({id}u32)"),
                None => "None".to_string(),
            };
            let rhs = match op.rhs_type_id {
                Some(id) => format!("Some({id}u32)"),
                None => "None".to_string(),
            };
            s.push_str(&format!(
                "            setypes::OperatorRewrite {{\n\
                 \x20               symbol: \"{sym}\".to_string(),\n\
                 \x20               lhs_type_id: {lhs},\n\
                 \x20               rhs_type_id: {rhs},\n\
                 \x20               function_name: \"{fn_}\".to_string(),\n\
                 \x20           }},\n",
                sym = rust_str_escape(&op.symbol),
                lhs = lhs,
                rhs = rhs,
                fn_ = rust_str_escape(&op.function_name),
            ));
        }
    }
    s.push_str("        ])\n");
    s
}

/// Build the body of `metadata::Guest::list_preprocessor_patterns()`.
fn build_preprocessor_patterns_body(plan: &BridgePlan) -> String {
    let total: usize = plan
        .extensions
        .iter()
        .map(|e| e.preprocessor_patterns.len())
        .sum();
    if total == 0 {
        return "        Ok(Vec::new())\n".to_string();
    }
    let mut s = String::new();
    s.push_str("        Ok(alloc::vec![\n");
    for ext in &plan.extensions {
        for p in &ext.preprocessor_patterns {
            s.push_str(&format!(
                "            setypes::PreprocessorPattern {{\n\
                 \x20               op_token: \"{tok}\".to_string(),\n\
                 \x20               function_name: \"{fn_}\".to_string(),\n\
                 \x20           }},\n",
                tok = rust_str_escape(&p.op_token),
                fn_ = rust_str_escape(&p.function_name),
            ));
        }
    }
    s.push_str("        ])\n");
    s
}

/// Build an `aliases:` field literal for the meta record. When the
/// aliases slice is empty, emits the original `Vec::new()` literal
/// so byte-output for extensions with no aliases (postgis today)
/// stays identical to the pre-patch stub. When non-empty, emits a
/// `vec!["a".to_string(), ...]` populated list.
fn aliases_literal(aliases: &[String]) -> String {
    if aliases.is_empty() {
        return "Vec::new()".to_string();
    }
    let mut s = String::new();
    s.push_str("alloc::vec![");
    for a in aliases {
        s.push_str(&format!("\"{}\".to_string(), ", rust_str_escape(a)));
    }
    s.push(']');
    s
}

/// Build a sql-name → canonical-name + aliases lookup over a
/// homogeneous slice of BridgePlan function records (each element
/// must expose its canonical name and alias list via the closures
/// — keeps this generic over ScalarFn/AggregateFn/WindowFn/TableFn
/// without dragging a trait through `shim-bridge-codegen-core`).
///
/// Returns:
///   - `alias_set`: every name that is an ALIAS (not a canonical) —
///     so the per-family registry impl can detect and skip the
///     redundant meta emission.
///   - `canonical_aliases`: for each canonical name, the full
///     aliases vec verbatim from BridgePlan.
struct NameIndex {
    alias_set: std::collections::HashSet<String>,
    canonical_aliases: std::collections::HashMap<String, Vec<String>>,
}

fn build_name_index<'a, I>(it: I) -> NameIndex
where
    I: IntoIterator<Item = (&'a str, &'a [String])>,
{
    let mut alias_set = std::collections::HashSet::new();
    let mut canonical_aliases = std::collections::HashMap::new();
    for (canonical, aliases) in it {
        canonical_aliases.insert(canonical.to_string(), aliases.to_vec());
        for a in aliases {
            alias_set.insert(a.clone());
        }
    }
    NameIndex {
        alias_set,
        canonical_aliases,
    }
}

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

    // UDTF entries — wires the row-yielding table functions
    // against datafission's iterator-style
    // `table_function_registry@1.0.0` (begin/next_row/end).
    let (udtf_entries, udtf_unwired) =
        interface_db::build_udtf_registry(plan, &shim_wit_dir, &records)?;

    // #616 Phase 3: window entries — wires postgis cluster window
    // functions against datafission's `window-function-registry@1.0.0`
    // `compute-partition(name, args-rows) -> list<scalar-value>`
    // surface. Whole-partition compute matches the upstream WIT
    // shape 1:1 — the cleanest fit among the three targets.
    let (window_entries, window_unwired) =
        interface_db::build_window_registry(plan, &shim_wit_dir)?;

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
    if !udtf_unwired.is_empty() {
        eprintln!(
            "[datafission-target] {} table function(s) not wired:",
            udtf_unwired.len(),
        );
        for u in &udtf_unwired {
            eprintln!("  - {}: {}", u.sql_name, u.reason);
        }
    }
    if !window_unwired.is_empty() {
        eprintln!(
            "[datafission-target] {} window function(s) not wired:",
            window_unwired.len(),
        );
        for u in &window_unwired {
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
    // UDTF WIT modules
    for entry in &udtf_entries {
        used_aliases
            .entry(entry.shape.wit_module.clone())
            .or_insert_with(|| entry.shape.wit_package.clone());
    }
    // #616: window WIT modules — typically `pg_cluster`
    // (postgis-clustering).
    for entry in &window_entries {
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
    //
    // #607 Phase 2: AccKind::Record aggregates also reference the
    // per-record `arg_witvalue_<snake>` / `ret_to_witvalue_<snake>`
    // helpers at the finalize site, so their accumulator kebab needs
    // to be in the helper-emission set too.
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
    // #607 Phase 2 + #612 (OQ1): record-typed aggregates reference
    // TWO per-record codec sites — decode via `arg_witvalue_<in>`
    // on the input record + encode via `ret_to_witvalue_<out>` on
    // the output record. Same-record aggregates have `input ==
    // output`; different-record aggregates carry distinct kebabs.
    for entry in &aggregate_entries {
        // #614 + #640: RecordToScalar / RecordToTuple use the same
        // per-input-record decoder as Record but no output-record
        // encoder (the output is a primitive scalar wrap (#614) or
        // a JSON-encoded primitive tuple (#640)).
        match &entry.shape.accumulator_kind {
            interface_db::AccKind::Record { input, output } => {
                referenced_records.insert(input.kebab_name.clone());
                referenced_records.insert(output.kebab_name.clone());
            }
            interface_db::AccKind::RecordToScalar { input, .. }
            | interface_db::AccKind::RecordToTuple { input, .. } => {
                referenced_records.insert(input.kebab_name.clone());
            }
            interface_db::AccKind::Geom | interface_db::AccKind::Raster => {}
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

    // ---- sql-extension-plugin/metadata: WIRED (#650 Path C) ----
    // Each `list-*` body materialises the static metadata that the
    // host snapshots at `CREATE EXTENSION` time. Source: the
    // BridgePlan's `cast_rewrites` / `operators` /
    // `preprocessor_patterns` (loaded verbatim from the interface
    // DB). When a plan has zero entries in a given family the body
    // collapses back to `Ok(Vec::new())` so postgis bridges (which
    // currently surface postgis casts/ops/preprocs via the same
    // shape) and any other zero-row extension stay byte-identical
    // to the pre-patch stub.
    let cast_body = build_cast_rewrites_body(plan);
    let op_body = build_operator_rewrites_body(plan);
    let prep_body = build_preprocessor_patterns_body(plan);
    s.push_str(&format!(
        r##"impl metadata::Guest for Component {{
    fn name() -> String {{ "{primary}".into() }}
    fn version() -> String {{ "{version}".into() }}

    fn list_cast_rewrites() -> Result<Vec<setypes::CastRewrite>, setypes::SqlExtError> {{
{cast_body}    }}
    fn list_operator_rewrites() -> Result<Vec<setypes::OperatorRewrite>, setypes::SqlExtError> {{
{op_body}    }}
    fn list_preprocessor_patterns() -> Result<Vec<setypes::PreprocessorPattern>, setypes::SqlExtError> {{
{prep_body}    }}
}}

"##,
        primary = primary,
        version = version,
        cast_body = cast_body,
        op_body = op_body,
        prep_body = prep_body,
    ));

    // ---- scalar-function-registry: WIRED ----
    // Build the per-name dispatch + metadata data.
    let scalar_block = build_scalar_registry_impl(&scalar_entries, plan);
    s.push_str(&scalar_block);

    // Shared handle-table prelude — emit once when either
    // aggregates or UDTFs are wired (both families use the same
    // `Cell` / `RefCell` / `BTreeMap` imports + the same handle
    // counter shape; deduplicate the `use` lines here so both
    // emit blocks below stay self-contained).
    let needs_handle_prelude =
        !aggregate_entries.is_empty() || !udtf_entries.is_empty();
    if needs_handle_prelude {
        s.push_str(HANDLE_TABLE_PRELUDE);
    }

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

    // ---- window-function-registry: WIRED (#616 Phase 3) ----
    // Empty surface (mobilitydb, etc.) falls back to the stub —
    // `WindowFunctionMeta::list_functions()` advertises nothing,
    // per-call methods return `UnknownFunction`. Non-empty surface
    // (postgis: 4 functions) emits the dispatching impl with one
    // `compute_partition` arm per canonical-or-alias name.
    if window_entries.is_empty() {
        s.push_str(WINDOW_STUB);
    } else {
        s.push_str(&build_window_registry_impl(&window_entries, primary, plan));
    }

    // ---- table-function-registry: WIRED ----
    // Same conditional pattern as aggregates: empty surface uses
    // the original stub, non-empty surface emits the UDTF_STATE
    // block + real Guest impl. HANDLE_TABLE_PRELUDE was emitted
    // above if either family is wired.
    if udtf_entries.is_empty() {
        s.push_str(TABLE_STUB);
    } else {
        s.push_str(UDTF_STATE_BLOCK);
        s.push_str(&build_table_registry_impl(&udtf_entries, plan));
    }

    // ---- multi-custom-type: WIRED ----
    // Note: the single-type `type-plugin/custom-type` interface is
    // intentionally NOT exported by the generated world (see
    // `emit_wit::render_world`). Components register types through
    // `multi-custom-type` instead.
    //
    // #618: `list_types()` advertises one `CustomTypeMeta` per
    // record in the primary-shim record_registry — every wit-value
    // record the bridge knows about. The per-call ops
    // (serialize/deserialize/compare/hash/display/parse) stay as
    // stubs in this cut; they'll be wired in follow-ups once the
    // canonical-CBOR round-trip on the multi-type path lands.
    s.push_str(&build_multi_custom_type_impl(&records));

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

    // #650 Path C: alias index — when a sql_name is a BridgePlan
    // alias of some canonical, skip its meta emission (the canonical
    // will list it in its `aliases` field) but still emit return /
    // execute arms so an alias-keyed dispatch still works.
    let name_idx = build_name_index(plan.extensions.iter().flat_map(|e| {
        e.scalars
            .iter()
            .map(|s| (s.canonical_name.as_str(), s.aliases.as_slice()))
    }));

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
        // Skip alias entries: their canonical advertises them via
        // the canonical's `aliases` field. We still emit the per-name
        // arms below so a host that dispatches by alias name keeps
        // working without depending on host-side pre-resolution.
        let is_alias = name_idx.alias_set.contains(&entry.sql_name);
        let mut sig_block = String::new();
        sig_block.push_str("vec![");
        for p in &entry.shape.params {
            let lt = dispatch::paramshape_to_logicaltype(p);
            sig_block.push_str(&lt);
            sig_block.push_str(", ");
        }
        sig_block.push(']');
        let escaped = entry.sql_name.replace('"', "\\\"");
        if !is_alias {
            let aliases_lit = aliases_literal(
                name_idx
                    .canonical_aliases
                    .get(&entry.sql_name)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]),
            );
            metas_block.push_str(&format!(
                "        ftypes::ScalarFunctionMeta {{\n\
                 \x20           name: \"{escaped}\".to_string(),\n\
                 \x20           aliases: {aliases_lit},\n\
                 \x20           param_types: vec![{sig_block}],\n\
                 \x20           is_deterministic: {deterministic},\n\
                 \x20           propagates_null: {propagates_null},\n\
                 \x20       }},\n",
                escaped = escaped,
                aliases_lit = aliases_lit,
                sig_block = sig_block,
                deterministic = deterministic,
                propagates_null = propagates_null,
            ));
        }

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
    // Phase 1A: AggregateEntry now carries one canonical sql_name +
    // an inline `aliases: Vec<String>`. The pre-Phase-1A shape (one
    // entry per canonical, one per alias) needed a `NameIndex`
    // (#650 Path C) to dedupe alias rows out of the meta block
    // while still emitting their create/finalize arms; the inline
    // alias list makes that workaround unnecessary — the metadata
    // pass iterates `entry.aliases` directly for the `aliases:`
    // literal, and the create_arms pass iterates canonical + each
    // alias for the per-name handle dispatch.

    // Assign each canonical entry one arm_idx; aliases reuse the
    // canonical's arm_idx so the finalize body is emitted once.
    let mut arm_for: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut next_arm: usize = 0;
    for entry in agg_entries {
        let arm_idx = arm_for
            .entry(entry.sql_name.clone())
            .or_insert_with(|| {
                let i = next_arm;
                next_arm += 1;
                i
            })
            .to_owned();
        for alias in &entry.aliases {
            arm_for.entry(alias.clone()).or_insert(arm_idx);
        }
    }

    let mut metas_block = String::new();
    let mut return_arms = String::new();
    let mut create_arms = String::new();
    let mut finalize_arms = String::new();
    let mut emitted_finalize: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    let mut seen_meta: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut seen_create: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for entry in agg_entries {
        let escaped = entry.sql_name.replace('"', "\\\"");
        let arm_idx = *arm_for.get(&entry.sql_name).unwrap();

        // Safety dedupe: if two extensions ever expose the same
        // canonical aggregate name, only emit the meta once
        // (matches the pre-Phase-1A `seen_meta` behaviour).
        if !seen_meta.insert(entry.sql_name.clone()) {
            continue;
        }

        // ---- metadata entry (canonical only — aliases ride the
        // canonical's `aliases:` literal) ----
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
        let aliases_lit = aliases_literal(&entry.aliases);
        metas_block.push_str(&format!(
            "        ftypes::AggregateFunctionMeta {{\n\
             \x20           name: \"{escaped}\".to_string(),\n\
             \x20           aliases: {aliases_lit},\n\
             \x20           param_types: {sig_block},\n\
             \x20           supports_grouped: true,\n\
             \x20           supports_partial: true,\n\
             \x20           is_order_sensitive: false,\n\
             \x20           accepts_config: {accepts_config},\n\
             \x20           config_arg_indices: vec![{cfg_indices}],\n\
             \x20       }},\n",
        ));

        // ---- return_type + create arms (canonical + each alias)
        // — alias-keyed dispatch must still resolve without
        // host-side pre-resolution. ----
        let ret_logical = dispatch::aggregate_ret_logicaltype(&entry.shape);
        for name in std::iter::once(entry.sql_name.as_str())
            .chain(entry.aliases.iter().map(|s| s.as_str()))
        {
            let name_escaped = name.replace('"', "\\\"");
            return_arms.push_str(&format!(
                "            \"{name_escaped}\" => Ok({ret_logical}),\n",
            ));
            if seen_create.insert(name.to_string()) {
                create_arms.push_str(&format!(
                    "            \"{name_escaped}\" => {arm_idx}usize,\n",
                ));
            }
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

/// Shared handle-table prelude — imports + cells that aggregates
/// and UDTFs both need. Emitted once when EITHER family is wired,
/// so the per-family AGGREGATE_STATE_BLOCK / UDTF_STATE_BLOCK can
/// reference `Cell` / `RefCell` / `BTreeMap` without duplicating
/// the `use` lines (which would error at module scope).
const HANDLE_TABLE_PRELUDE: &str = r##"// ─── Handle-table cell + map imports (shared by aggregates + UDTFs) ───

use core::cell::{Cell, RefCell};
use alloc::collections::BTreeMap;

"##;

/// Per-handle accumulator state + thread-local map. Emitted into
/// the bridge's lib.rs prelude once when any aggregate is wired.
/// Assumes `HANDLE_TABLE_PRELUDE` has been emitted first.
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

/// #616 Phase 3: build the `window_function_registry::Guest` impl
/// block. Datafission's window contract is whole-partition compute
/// (DD2 in PLAN-window-substrate.md) which matches the postgis
/// `func(list<borrow<geometry>>, ...) -> list<Y>` upstream 1:1 —
/// the cleanest fit among the three targets.
///
/// One arm per canonical-or-alias name:
///   1. Walk `args_rows`, decode each row's column 0 (binary WKB)
///      via `Geometry::from_wkb`.
///   2. Read extras from `args_rows[0]` (SQL window constants are
///      uniform across the partition).
///   3. Build a `Vec<&Geometry>` borrow slice.
///   4. Call the upstream cluster function.
///   5. Marshal each per-row Y back to a `ftypes::ScalarValue` per
///      the classified `WindowReturn` shape (Null for option<u32>
///      noise points, etc.).
fn build_window_registry_impl(
    window_entries: &[interface_db::WindowEntry],
    primary: &str,
    plan: &BridgePlan,
) -> String {
    // #650 Path C: alias index (see scalar registry for rationale).
    let name_idx = build_name_index(plan.extensions.iter().flat_map(|e| {
        e.window_functions
            .iter()
            .map(|w| (w.canonical_name.as_str(), w.aliases.as_slice()))
    }));

    let mut metas_block = String::new();
    let mut return_arms = String::new();
    let mut compute_arms = String::new();
    let mut emitted_compute: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut seen_meta: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for entry in window_entries {
        let escaped = entry.sql_name.replace('"', "\\\"");
        let is_alias = name_idx.alias_set.contains(&entry.sql_name);

        if seen_meta.insert(entry.sql_name.clone()) {
            let ret_logical = match &entry.shape.returns {
                interface_db::WindowReturn::OptionU32
                | interface_db::WindowReturn::U32 => "types::LogicalType::Int64",
                interface_db::WindowReturn::GeomBlob => "types::LogicalType::Binary",
            };
            // ---- metadata entry (skipped for alias sql_names) ----
            if !is_alias {
                let mut sig_block = String::new();
                sig_block.push_str("vec![vec![");
                // Streaming partition arg: Binary (WKB).
                sig_block.push_str("ftypes::LogicalType::Binary, ");
                for p in &entry.shape.extra_args {
                    let lt = dispatch::paramshape_to_logicaltype(p);
                    sig_block.push_str(&lt);
                    sig_block.push_str(", ");
                }
                sig_block.push_str("]]");
                let aliases_lit = aliases_literal(
                    name_idx
                        .canonical_aliases
                        .get(&entry.sql_name)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]),
                );
                metas_block.push_str(&format!(
                    "        ftypes::WindowFunctionMeta {{\n\
                     \x20           name: \"{escaped}\".to_string(),\n\
                     \x20           aliases: {aliases_lit},\n\
                     \x20           param_types: {sig_block},\n\
                     \x20       }},\n",
                ));
            }

            // ---- return_type arm (emitted for canonical AND alias
            // sql_names so alias-keyed dispatch still resolves) ----
            return_arms.push_str(&format!(
                "            \"{escaped}\" => Ok({ret_logical}),\n",
            ));
        }

        if emitted_compute.insert(entry.sql_name.clone()) {
            let body = emit_window_compute_arm_body(&entry.sql_name, &entry.shape);
            compute_arms.push_str(&format!(
                "            \"{escaped}\" => {{\n{body}\n            }}\n",
            ));
        }
    }

    format!(
        r##"impl window_function_registry::Guest for Component {{
    fn list_functions() -> Vec<ftypes::WindowFunctionMeta> {{
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

    fn compute_partition(
        name: String,
        args_rows: Vec<Vec<ftypes::ScalarValue>>,
    ) -> Result<Vec<ftypes::ScalarValue>, ftypes::FunctionError> {{
        if args_rows.is_empty() {{
            return Ok(Vec::new());
        }}
        match name.as_str() {{
{compute_arms}            other => Err(ftypes::FunctionError::UnknownFunction(other.into())),
        }}
    }}
}}

// {primary}: window-function-registry wired (#616 Phase 3) —
// whole-partition compute matches the postgis-clustering upstream
// 1:1.

"##,
    )
}

/// #616: emit the body of one window-function `compute_partition`
/// match arm. Takes the SQL name + classified `WindowShape`; produces
/// Rust code that decodes the partition rows, calls the upstream
/// cluster function, and marshals the per-row results back to
/// `ftypes::ScalarValue`.
fn emit_window_compute_arm_body(
    sql_name: &str,
    shape: &interface_db::WindowShape,
) -> String {
    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let i = "                ";

    // Decode the streaming geometry column.
    let mut s = String::new();
    s.push_str(&format!(
        "{i}let mut geoms: Vec<Geometry> = Vec::with_capacity(args_rows.len());\n\
         {i}for row in &args_rows {{\n\
         {i}    let bytes = dfv_blob(row, 0, \"{sql_name}\")?;\n\
         {i}    geoms.push(\n\
         {i}        Geometry::from_wkb(bytes)\n\
         {i}            .map_err(|e| ftypes::FunctionError::Internal(format!(\"{sql_name}: row decode: {{:?}}\", e)))?,\n\
         {i}    );\n\
         {i}}}\n",
    ));

    // Decode extras from row 0 (SQL window constants are uniform).
    let mut call_extras: Vec<String> = Vec::new();
    for (j, p) in shape.extra_args.iter().enumerate() {
        let arg_index = j + 1;
        let (decode_line, var_expr) = match p {
            interface_db::ParamShape::F64 => (
                format!(
                    "{i}let extra{j} = match args_rows[0].get({arg_index}) {{\n\
                     {i}    Some(ftypes::ScalarValue::Float64(v)) => *v,\n\
                     {i}    Some(ftypes::ScalarValue::Int64(v)) => *v as f64,\n\
                     {i}    other => return Err(ftypes::FunctionError::Internal(\n\
                     {i}        format!(\"{sql_name}: arg {arg_index} expects Float64; got {{:?}}\", other),\n\
                     {i}    )),\n\
                     {i}}};\n",
                ),
                format!("extra{j}"),
            ),
            interface_db::ParamShape::S32
            | interface_db::ParamShape::S64
            | interface_db::ParamShape::U32
            | interface_db::ParamShape::U64
            | interface_db::ParamShape::Bool => {
                let cast = match p {
                    interface_db::ParamShape::S32 => "as i32",
                    interface_db::ParamShape::S64 => "as i64",
                    interface_db::ParamShape::U32 => "as u32",
                    interface_db::ParamShape::U64 => "as u64",
                    interface_db::ParamShape::Bool => "!= 0",
                    _ => unreachable!(),
                };
                (
                    format!(
                        "{i}let raw{j}: i64 = match args_rows[0].get({arg_index}) {{\n\
                         {i}    Some(ftypes::ScalarValue::Int64(v)) => *v,\n\
                         {i}    Some(ftypes::ScalarValue::Uint64(v)) => *v as i64,\n\
                         {i}    Some(ftypes::ScalarValue::Int32(v)) => *v as i64,\n\
                         {i}    other => return Err(ftypes::FunctionError::Internal(\n\
                         {i}        format!(\"{sql_name}: arg {arg_index} expects integer; got {{:?}}\", other),\n\
                         {i}    )),\n\
                         {i}}};\n\
                         {i}let extra{j} = raw{j} {cast};\n",
                    ),
                    format!("extra{j}"),
                )
            }
            other => {
                return format!(
                    "{i}// ERROR: window {sql_name} has unsupported extra arg shape {other:?}\n\
                     {i}return Err(ftypes::FunctionError::Internal(\"{sql_name}: extra arg unsupported\".into()));\n",
                );
            }
        };
        s.push_str(&decode_line);
        call_extras.push(var_expr);
    }

    let call_extras_lit = if call_extras.is_empty() {
        String::new()
    } else {
        format!(", {}", call_extras.join(", "))
    };

    // Upstream call + error map.
    let map_err = if shape.fallible {
        format!(".map_err(|e| ftypes::FunctionError::Internal(format!(\"{sql_name}: {{:?}}\", e)))?")
    } else {
        String::new()
    };
    s.push_str(&format!(
        "{i}let geom_refs: Vec<&Geometry> = geoms.iter().collect();\n\
         {i}let labels = {module}::{func}(&geom_refs{call_extras_lit}){map_err};\n",
    ));

    // Per-row return -> ftypes::ScalarValue marshaling.
    match &shape.returns {
        interface_db::WindowReturn::OptionU32 => {
            s.push_str(&format!(
                "{i}Ok(labels\n\
                 {i}    .into_iter()\n\
                 {i}    .map(|opt| match opt {{\n\
                 {i}        Some(id) => ftypes::ScalarValue::Uint32(id),\n\
                 {i}        None => ftypes::ScalarValue::Null,\n\
                 {i}    }})\n\
                 {i}    .collect())\n",
            ));
        }
        interface_db::WindowReturn::U32 => {
            s.push_str(&format!(
                "{i}Ok(labels\n\
                 {i}    .into_iter()\n\
                 {i}    .map(ftypes::ScalarValue::Uint32)\n\
                 {i}    .collect())\n",
            ));
        }
        interface_db::WindowReturn::GeomBlob => {
            s.push_str(&format!(
                "{i}Ok(labels\n\
                 {i}    .into_iter()\n\
                 {i}    .map(|g| ftypes::ScalarValue::Binary(g.as_wkb().into()))\n\
                 {i}    .collect())\n",
            ));
        }
    }
    s
}

/// Build the `table_function_registry::Guest` impl block. Emits
/// per-name metadata, output_schema arm, begin arm that calls the
/// per-UDTF emit body (decoding args, calling upstream, materialising
/// rows), plus next_row + end against the UDTF_STATE handle table.
fn build_table_registry_impl(
    udtf_entries: &[interface_db::UdtfEntry],
    plan: &BridgePlan,
) -> String {
    // #650 Path C: alias index (see scalar registry for rationale).
    let name_idx = build_name_index(plan.extensions.iter().flat_map(|e| {
        e.table_functions
            .iter()
            .map(|t| (t.canonical_name.as_str(), t.aliases.as_slice()))
    }));

    let mut metas_block = String::new();
    let mut output_schema_arms = String::new();
    let mut begin_arms = String::new();
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for entry in udtf_entries {
        if !seen.insert(entry.sql_name.clone()) {
            continue;
        }
        let escaped = entry.sql_name.replace('"', "\\\"");
        let is_alias = name_idx.alias_set.contains(&entry.sql_name);

        // ---- metadata entry (skipped for alias sql_names) ----
        if !is_alias {
            let mut sig_block = String::new();
            sig_block.push_str("vec![vec![");
            for p in &entry.shape.params {
                let lt = dispatch::paramshape_to_logicaltype(p);
                sig_block.push_str(&lt);
                sig_block.push_str(", ");
            }
            sig_block.push_str("]]");
            let aliases_lit = aliases_literal(
                name_idx
                    .canonical_aliases
                    .get(&entry.sql_name)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]),
            );
            metas_block.push_str(&format!(
                "        ftypes::TableFunctionMeta {{\n\
                 \x20           name: \"{escaped}\".to_string(),\n\
                 \x20           aliases: {aliases_lit},\n\
                 \x20           param_types: {sig_block},\n\
                 \x20       }},\n",
            ));
        }

        // ---- output_schema arm ----
        let cols = dispatch::emit_udtf_column_info(&entry.shape);
        output_schema_arms.push_str(&format!(
            "            \"{escaped}\" => Ok(alloc::vec![{cols}]),\n",
        ));

        // ---- begin arm ----
        let body = dispatch::emit_udtf_begin_body(
            &entry.shape,
            &entry.sql_name,
            "                ",
        );
        begin_arms.push_str(&format!(
            "            \"{escaped}\" => {{\n{body}\n            }}\n",
        ));
    }

    format!(
        r##"impl table_function_registry::Guest for Component {{
    fn list_functions() -> Vec<ftypes::TableFunctionMeta> {{
        vec![
{metas_block}        ]
    }}

    fn output_schema(
        name: String,
        _input_types: Vec<ftypes::LogicalType>,
    ) -> Result<Vec<ftypes::ColumnInfo>, ftypes::FunctionError> {{
        match name.as_str() {{
{output_schema_arms}            other => Err(ftypes::FunctionError::UnknownFunction(other.into())),
        }}
    }}

    fn begin(
        name: String,
        args: Vec<ftypes::ScalarValue>,
    ) -> Result<u64, ftypes::FunctionError> {{
        match name.as_str() {{
{begin_arms}            other => Err(ftypes::FunctionError::UnknownFunction(other.into())),
        }}
    }}

    fn next_row(handle: u64) -> Option<Result<Vec<ftypes::ScalarValue>, ftypes::FunctionError>> {{
        UDTF_STATE.with(|m| {{
            let mut g = m.borrow_mut();
            let st = g.get_mut(&handle)?;
            if st.idx >= st.rows.len() {{
                return None;
            }}
            let row = core::mem::take(&mut st.rows[st.idx]);
            st.idx += 1;
            Some(Ok(row))
        }})
    }}

    fn end(handle: u64) {{
        UDTF_STATE.with(|m| {{
            m.borrow_mut().remove(&handle);
        }});
    }}
}}

"##,
    )
}

/// Per-handle UDTF state + thread-local map. Emitted into the
/// bridge's lib.rs prelude once when any table function is wired.
/// Assumes `HANDLE_TABLE_PRELUDE` has been emitted first (for the
/// `Cell` / `RefCell` / `BTreeMap` imports). The aggregate and
/// UDTF families share that prelude when both are wired.
const UDTF_STATE_BLOCK: &str = r##"// ─── UDTF iterator state ───
//
// `table_function_registry@1.0.0` uses an iterator model:
// `begin(name, args)` returns a u64 handle after eagerly
// materialising the rowset; `next_row(handle)` peels one row at
// a time; `end(handle)` drops the state. The bridge keeps a
// thread-local map: handle -> UdtfState { rows, idx }.
//
// ACCUMULATORS / NEXT_ACC_HANDLE (from the aggregate block) and
// UDTF_STATE / NEXT_UDTF_HANDLE are independent — the handle
// namespaces are per-family, so collisions are impossible.

#[derive(Default)]
struct UdtfState {
    rows: Vec<Vec<ftypes::ScalarValue>>,
    idx: usize,
}

thread_local! {
    static UDTF_STATE: RefCell<BTreeMap<u64, UdtfState>> =
        RefCell::new(BTreeMap::new());
    static NEXT_UDTF_HANDLE: Cell<u64> = const { Cell::new(1) };
}

fn alloc_udtf_handle(rows: Vec<Vec<ftypes::ScalarValue>>) -> u64 {
    let h = NEXT_UDTF_HANDLE.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    UDTF_STATE.with(|m| {
        m.borrow_mut().insert(h, UdtfState { rows, idx: 0 });
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

/// Map a record's 32-byte canonical type_id (sha256) into the u32
/// space the datafission `multi-custom-type` contract uses, with
/// the host-side guard that type-id >= 1000 (built-in DuckDB-style
/// types occupy [0, 1000)). The mapping is deterministic across
/// regens so the same record gets the same advertised id every
/// time.
fn record_type_id_u32(type_id: &[u8; 32]) -> u32 {
    let raw = u32::from_be_bytes([type_id[0], type_id[1], type_id[2], type_id[3]]);
    1000u32.saturating_add(raw % (u32::MAX - 1000))
}

/// #618: build the `multi_custom_type::Guest for Component` impl
/// block with REAL advertisements from the record registry.
///
/// `list_types()` returns one `CustomTypeMeta` per record the
/// bridge knows about (the same set already used to emit per-record
/// `arg_witvalue_<snake>` / `ret_to_witvalue_<snake>` codecs). Each
/// entry carries:
///
///   - `type_name`: the canonical symbolic name
///     (`<package>@<version>/<interface>/<record>`) — fully
///     qualified so two shims advertising similarly-named records
///     can coexist in the same database without aliasing.
///   - `type_id`: derived from the first 4 bytes of the record's
///     canonical-WIT 32-byte sha256 type_id, mapped into
///     [1000, u32::MAX] (the host requires type_id >= 1000).
///   - `storage_size`: -1 (all wit-value records are
///     variable-length under canonical-CBOR framing).
///   - `cast_from` / `cast_to`: empty — custom types ride the WTV
///     magic-prefix Binary envelope, not implicit built-in casts.
///
/// The per-call ops (serialize / deserialize / compare /
/// hash_value / display / parse) stay as stubs — they no-op or
/// return an Internal error tagged with the type_id. Wiring those
/// against the bridge's serde-ops codecs is a follow-up task.
fn build_multi_custom_type_impl(records: &[RecordType]) -> String {
    let mut metas_block = String::new();
    for r in records {
        let type_id_u32 = record_type_id_u32(&r.type_id);
        let escaped_name = r.symbolic_name.replace('"', "\\\"");
        metas_block.push_str(&format!(
            "        multi_custom_type::CustomTypeMeta {{\n\
             \x20           type_name: \"{escaped_name}\".to_string(),\n\
             \x20           type_id: {type_id_u32}u32,\n\
             \x20           storage_size: -1,\n\
             \x20           cast_from: Vec::new(),\n\
             \x20           cast_to: Vec::new(),\n\
             \x20       }},\n",
        ));
    }
    format!(
        r##"impl multi_custom_type::Guest for Component {{
    fn list_types() -> Vec<multi_custom_type::CustomTypeMeta> {{
        vec![
{metas_block}        ]
    }}
    fn serialize(_type_id: u32, value: Vec<u8>) -> Vec<u8> {{ value }}
    fn deserialize(type_id: u32, _bytes: Vec<u8>) -> Result<Vec<u8>, ttypes::TypeError> {{
        Err(ttypes::TypeError::Internal(format!(
            "no per-call codec wired for custom type id {{type_id}} (advertisement-only cut)"
        )))
    }}
    fn compare(_type_id: u32, _a: Vec<u8>, _b: Vec<u8>) -> ttypes::Ordering {{
        ttypes::Ordering::Equal
    }}
    fn hash_value(_type_id: u32, _value: Vec<u8>) -> u64 {{ 0 }}
    fn display(type_id: u32, _value: Vec<u8>) -> String {{
        format!("<type {{type_id}} (stub)>")
    }}
    fn parse(type_id: u32, _input: String) -> Result<Vec<u8>, ttypes::TypeError> {{
        Err(ttypes::TypeError::Internal(format!(
            "no per-call codec wired for custom type id {{type_id}} (advertisement-only cut)"
        )))
    }}
}}

"##,
    )
}

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
