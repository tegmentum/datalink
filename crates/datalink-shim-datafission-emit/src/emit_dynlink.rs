//! Dynlink-mode datafission bridge emitter (#823 Phase 3 Commit 6).
//!
//! Emits a bridge crate that routes SQL scalar dispatch through
//! `compose:dynlink/linker` — CBOR envelope in / CBOR envelope out
//! against a resident provider identified by `provider_id` — instead
//! of the wac-plug-linked WIT interfaces the sibling `emit`
//! function produces.
//!
//! Wire discipline mirrors `postgis-wasm/crates/provider/src/envelope.rs`:
//! `Request { version: 1, args: Vec<CborValue> }` /
//! `Response { ok: Option<CborValue>, err: Option<String> }`. The
//! `CborValue` variants are serialised as single-key CBOR maps
//! (the round-10 disambiguation).
//!
//! ## Scope for the first landing
//!
//! Scalar arms with primitive parameter + return shapes only. That
//! covers all 65 postgis_sfcgal arms (which take `list<u8>` WKB and
//! return `f64` / `bool` / `result<list<u8>>`). Non-primitive shapes
//! (resource handles, records, enums, list-of-record, etc.) fall
//! through to unwired stubs — the caller sees them in
//! `scalar_function_registry::list_functions` but any invocation
//! returns `FunctionError::UnknownFunction("<name>: not-yet
//! supported in dynlink mode")`.
//!
//! UDTFs / window functions: emitted as empty registries. Aggregates
//! wire real dispatch through the CBOR envelope for `AccKind::Geom`
//! (postgis WKB blobs — `st_union_aggregate`, `st_polygonize_aggregate`,
//! `st_collect_aggregate`, `st_extent`, `st_extent_threed`, etc.) plus
//! the record-typed `AccKind::Record{,ToScalar,ToTuple,ToListPrim,
//! SetToRecordSet}` families (mobilitydb temporal aggregators —
//! `tint_merge_agg`, `tfloat_temporal_min`, `tint_count_aggregate`,
//! `tint_range_aggregate`, `tjsonb_sequences_agg_*`, etc.). The
//! accumulator is always `Vec<Vec<u8>>` since every SQL aggregate row
//! streams a `binary` payload; the provider owns record decode via
//! the CBOR envelope. `AccKind::Raster` still surfaces in
//! `list_functions` but its `finalize` returns `UnknownFunction` —
//! the raster-blob accumulator ABI shares the same wire shape but no
//! resident provider dispatches it today. See
//! `build_aggregate_registry_impl_dynlink`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use shim_bridge_codegen_core::BridgePlan;
use datalink_shim_codegen_core::interface_db::{
    self, AccKind, AggregateEntry, DispatchEntry, ListPrimElem, ParamShape, RetShape,
};
use datalink_shim_codegen_core::record_registry::{self, RecordType};

use crate::emit_wit;

/// Options for `emit_dynlink`.
pub struct DynlinkOptions {
    /// The provider id the bridge resolves at instantiate time via
    /// `compose:dynlink/linker.resolve-by-id`. Matches what
    /// `datafission-df-plugin-loader::sub_ext_provider_id(<sub_ext>)`
    /// hands the process-global provider registry.
    pub provider_id: String,
    /// SQL-facing sub-extension name. Used for the crate name +
    /// identity + diagnostic prefixes.
    pub sub_ext: String,
}

/// Entry point: emit a Dynlink-mode datafission bridge crate.
///
/// Produced layout:
///
/// ```text
/// Cargo.toml
/// README.md
/// src/lib.rs
/// wit/world.wit
/// wit/deps/compose-dynlink/     (symlinked from datalink-dynlink)
/// wit/deps/sys-compose/         (symlinked from datalink-dynlink)
/// wit/deps/datafission-*/       (copied from datafission WIT)
/// ```
pub fn emit_dynlink(plan: &BridgePlan, out_dir: &Path, opts: &DynlinkOptions) -> Result<()> {
    fs::create_dir_all(out_dir.join("src"))?;
    fs::create_dir_all(out_dir.join("wit/deps"))?;

    let crate_name = crate_name_for(opts);
    let version = plan
        .extensions
        .first()
        .map(|e| e.version.as_str())
        .unwrap_or("0.1.0");

    // Cargo.toml
    fs::write(out_dir.join("Cargo.toml"), cargo_toml(&crate_name, version))?;

    // wit/world.wit
    fs::write(out_dir.join("wit/world.wit"), world_wit(&crate_name))?;

    // wit/deps — copy datafission + compose-dynlink + sys-compose.
    populate_deps(plan, opts, &out_dir.join("wit/deps"))?;

    // src/lib.rs
    let lib_src = lib_rs(plan, opts)?;
    fs::write(out_dir.join("src/lib.rs"), lib_src)?;

    // README.md — one-liner pointing at the design doc.
    fs::write(
        out_dir.join("README.md"),
        readme(&crate_name, &opts.provider_id, &opts.sub_ext),
    )?;

    Ok(())
}

fn crate_name_for(opts: &DynlinkOptions) -> String {
    let sub = opts
        .sub_ext
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    format!("{sub}-datafission-bridge-dynlink")
}

fn cargo_toml(crate_name: &str, version: &str) -> String {
    format!(
        r#"[package]
name = "{crate_name}"
version = "{version}"
edition = "2021"
description = "Phase 3 dynlink-mode datafission bridge — routes SQL dispatch through compose:dynlink/linker (#823)."
license = "Apache-2.0"
publish = false

[workspace]
members = ["."]

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = {{ version = "0.51", features = ["macros"] }}
wit-bindgen-rt = {{ version = "0.44", features = ["bitflags"] }}
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
panic = "abort"
"#,
        crate_name = crate_name,
        version = version,
    )
}

fn world_wit(crate_name: &str) -> String {
    let pkg = crate_name;
    format!(
        r#"package datafission-bridge:{pkg}@0.1.0;

/// Dynlink-mode datafission bridge — Phase 3 Commit 6b (#823).
///
/// Instead of importing the shim's typed WIT interfaces, the bridge
/// imports `compose:dynlink/linker` and dispatches every SQL arm
/// through `resolve-by-id` + `invoke` against a resident provider.
///
/// The world exports the full 10-interface datafission composite
/// surface so `RuntimeWasmExtension::from_file` can bind against
/// `ExtensionBindings::instantiate` without falling over a missing
/// export. Scalar dispatch is the only category actually wired to
/// the resident provider; the other six categories advertise empty
/// registries (aggregate / window / table / multi-custom-type) or
/// honest no-op stubs (spatial-index / system-catalog / index) so
/// the runtime sees a structurally complete component even though
/// only scalars route through `compose:dynlink/linker`.
world bridge {{
    // Compose:dynlink linker — the only shim-side import.
    import compose:dynlink/linker@0.1.0;

    // Host-provided logging — same package as `extension`.
    import datafission:extension/logging@1.0.0;

    // Datafission composite exports — full 10-interface surface so
    // the runtime's composite-world binding is satisfied.
    export datafission:extension/identity@1.0.0;
    export datafission:sql-extension-plugin/metadata@1.2.0;
    export datafission:spatial-index-plugin/spatial-index@1.0.0;
    export datafission:system-catalog-plugin/system-catalog@1.0.0;
    export datafission:function-plugin/scalar-function-registry@1.0.0;
    export datafission:function-plugin/aggregate-function-registry@1.0.0;
    export datafission:function-plugin/table-function-registry@1.0.0;
    export datafission:function-plugin/window-function-registry@1.0.0;
    export datafission:type-plugin/multi-custom-type@1.0.0;
    export datafission:index-plugin/index@1.0.0;
}}
"#,
        pkg = pkg,
    )
}

fn populate_deps(plan: &BridgePlan, opts: &DynlinkOptions, deps_dir: &Path) -> Result<()> {
    // compose-dynlink + sys-compose are sourced from the local
    // datalink-dynlink crate's WIT tree — the definitive copy for
    // this repo. Any downstream host that pins a different revision
    // of these packages regenerates the bridge against its own tree.
    let dynlink_root =
        std::env::var("DATALINK_DYNLINK_WIT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_default();
                PathBuf::from(home).join("git/datalink/crates/datalink-dynlink/wit")
            });
    // datalink-dynlink/wit lays out the tree with compose-dynlink at
    // the root and sys-compose nested under compose-dynlink/deps/.
    let compose_dynlink_from = dynlink_root.join("compose-dynlink");
    let sys_compose_from = dynlink_root.join("compose-dynlink/deps/sys-compose");
    // compose-dynlink/world.wit references sqlite:extension worlds that
    // aren't part of the datafission bridge — skip it. We only need the
    // package + endpoint + linker interfaces the world imports.
    if !compose_dynlink_from.is_dir() {
        return Err(anyhow!(
            "compose:dynlink WIT source missing: {} (set DATALINK_DYNLINK_WIT)",
            compose_dynlink_from.display()
        ));
    }
    let compose_dst = deps_dir.join("compose-dynlink");
    fs::create_dir_all(&compose_dst)?;
    for name in ["package.wit", "linker.wit", "endpoint.wit"] {
        let f = compose_dynlink_from.join(name);
        if f.is_file() {
            fs::copy(&f, compose_dst.join(name))?;
        }
    }
    if !sys_compose_from.is_dir() {
        return Err(anyhow!(
            "sys:compose WIT source missing: {}",
            sys_compose_from.display()
        ));
    }
    copy_tree(&sys_compose_from, &deps_dir.join("sys-compose"))?;

    // Datafission extension packages — same sourcing rules the
    // WacPlug emitter uses.
    let primary = &opts.sub_ext;
    let df_root = emit_wit::source_datafission_wit_root(primary)?;
    for pkg in DATAFISSION_PACKAGE_DIRS {
        let from = df_root.join(pkg);
        if !from.is_dir() {
            return Err(anyhow!(
                "datafission WIT package missing: {}",
                from.display()
            ));
        }
        let to = deps_dir.join(pkg);
        copy_tree(&from, &to)?;
    }
    let _ = plan; // reserved for future per-plan customisation.
    Ok(())
}

const DATAFISSION_PACKAGE_DIRS: &[&str] = &[
    "extension",
    "function-plugin",
    "sql-extension-plugin",
    "type-plugin",
    "spatial-index-plugin",
    "system-catalog-plugin",
    "index-plugin",
];

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&src, &dst)?;
        } else if ty.is_file() {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn readme(crate_name: &str, provider_id: &str, sub_ext: &str) -> String {
    format!(
        "# {crate_name}\n\
         \n\
         Phase 3 dynlink-mode datafission bridge for `{sub_ext}` (#823).\n\
         \n\
         The bridge imports only `compose:dynlink/linker` and dispatches SQL\n\
         scalar arms as CBOR envelopes through the resident provider\n\
         `{provider_id}` — no `postgis:wasm/*` imports, no wac-plug composition\n\
         step. The provider crate + backend live behind the plans authored under\n\
         `postgis-wasm/plans/` (see the sub-extensions design doc).\n"
    )
}

// ============================================================
// src/lib.rs generation
// ============================================================

fn lib_rs(plan: &BridgePlan, opts: &DynlinkOptions) -> Result<String> {
    let primary = plan
        .extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or(&opts.sub_ext);
    let version = plan
        .extensions
        .first()
        .map(|e| e.version.as_str())
        .unwrap_or("0.1.0");

    // Reuse the WacPlug classifier to build DispatchEntry list.
    let wit_deps_root = emit_wit::source_shim_deps_dir(primary)?;
    let shim_packages = emit_wit::discover_shim_packages(&wit_deps_root)?;
    let shim_wit_dir = pick_primary_shim_dir(primary, &wit_deps_root, &shim_packages);

    // Build the record registry so the aggregate classifier can
    // resolve `list<record>` inputs (mobilitydb temporal-type
    // aggregates) into `AccKind::Record{To*}` variants. Without
    // records the classifier can never match these — every
    // mobilitydb aggregate would fall through to `unwired` and the
    // emitted registry would report `0 wired / 0 stubbed`. Mirrors
    // the sibling `emit_lib.rs` shape.
    let records: Vec<RecordType> = record_registry::build(&shim_packages, primary)
        .into_iter()
        .filter(|r| emit_wit::package_belongs_to_primary(&r.package, primary))
        .collect();

    let (scalar_entries, _unwired) =
        interface_db::build_full(plan, &shim_wit_dir, &records)?;

    // Reuse the WacPlug aggregate classifier. Failures degrade to an
    // empty list so a shim without aggregates still emits cleanly.
    let aggregate_entries: Vec<AggregateEntry> =
        match interface_db::build_aggregate_registry(plan, &shim_wit_dir, &records) {
            Ok((entries, _agg_unwired)) => entries,
            Err(e) => {
                eprintln!("[datafission-dynlink] build_aggregate_registry failed: {e}");
                Vec::new()
            }
        };

    // Filter to primitive-only shapes and partition by sub-extension.
    // For Commit 6 we scope to the sub-ext the bridge is being emitted
    // for; other sub-exts get emitted as unwired stubs (list_functions
    // still surfaces them so upstream integration doesn't regress).
    let mut primitive_arms: Vec<(&DispatchEntry, bool)> = Vec::new();
    let mut skipped: Vec<&DispatchEntry> = Vec::new();
    for (entry, fallible) in &scalar_entries {
        if is_primitive_shape(&entry.shape.params, &entry.shape.ret) {
            primitive_arms.push((entry, *fallible));
        } else {
            skipped.push(entry);
        }
    }
    if !skipped.is_empty() {
        eprintln!(
            "[datafission-dynlink] {} scalar(s) skipped (non-primitive shapes not supported in Commit 6 scope):",
            skipped.len()
        );
        // Shape-family histogram — helps triage what's worth admitting next.
        use std::collections::BTreeMap;
        let mut shape_hist: BTreeMap<&'static str, usize> = BTreeMap::new();
        for e in &skipped {
            for p in &e.shape.params {
                let name: &'static str = match p {
                    ParamShape::Blob => "Blob",
                    ParamShape::F64 => "F64",
                    ParamShape::S32 => "S32",
                    ParamShape::S64 => "S64",
                    ParamShape::U32 => "U32",
                    ParamShape::U64 => "U64",
                    ParamShape::Bool => "Bool",
                    ParamShape::Text => "Text",
                    ParamShape::Geom => "Geom",
                    ParamShape::Geog => "Geog",
                    ParamShape::Raster => "Raster",
                    ParamShape::Topology => "Topology",
                    ParamShape::OptionNone => "OptionNone",
                    ParamShape::ListGeom => "ListGeom",
                    ParamShape::WitValueRecord { .. } => "WitValueRecord",
                    ParamShape::Enum { .. } => "Enum",
                    ParamShape::ListPrim(_) => "ListPrim",
                    ParamShape::ListRecord { .. } => "ListRecord",
                    ParamShape::ListListU8 => "ListListU8",
                    ParamShape::ListListPrim(_) => "ListListPrim",
                    ParamShape::ListListRecord { .. } => "ListListRecord",
                    ParamShape::ListTuple { .. } => "ListTuple",
                    ParamShape::ListTupleMixed { .. } => "ListTupleMixed",
                };
                *shape_hist.entry(name).or_insert(0) += 1;
            }
        }
        eprintln!("[datafission-dynlink] skipped-arm param-shape histogram:");
        for (k, v) in shape_hist {
            eprintln!("  - {} × {}", k, v);
        }
        // RetShape histogram — after the last param-shape round the
        // remaining skips are dominated by the return side, so tallying
        // return shapes tells the next iteration which variants to
        // admit.
        let mut ret_hist: BTreeMap<&'static str, usize> = BTreeMap::new();
        for e in &skipped {
            let name: &'static str = match &e.shape.ret {
                RetShape::Text => "Text",
                RetShape::Real => "Real",
                RetShape::Int => "Int",
                RetShape::BoolInt => "BoolInt",
                RetShape::Blob => "Blob",
                RetShape::GeomBlob => "GeomBlob",
                RetShape::RasterBlob => "RasterBlob",
                RetShape::TopologyBlob => "TopologyBlob",
                RetShape::TopoGeometryViaGeom => "TopoGeometryViaGeom",
                RetShape::OptionText => "OptionText",
                RetShape::OptionReal => "OptionReal",
                RetShape::OptionInt => "OptionInt",
                RetShape::OptionBlob => "OptionBlob",
                RetShape::OptionGeomBlob => "OptionGeomBlob",
                RetShape::OptionRasterBlob => "OptionRasterBlob",
                RetShape::OptionTopologyBlob => "OptionTopologyBlob",
                RetShape::FirstGeomBlob => "FirstGeomBlob",
                RetShape::FirstRasterBlob => "FirstRasterBlob",
                RetShape::FirstTopologyBlob => "FirstTopologyBlob",
                RetShape::FirstOptionU32Int => "FirstOptionU32Int",
                RetShape::BboxBlob => "BboxBlob",
                RetShape::IsValidDetailText => "IsValidDetailText",
                RetShape::Bbox3dWkbLineZ => "Bbox3dWkbLineZ",
                RetShape::WitValueRecord { .. } => "WitValueRecord",
                RetShape::OptionBoolInt => "OptionBoolInt",
                RetShape::OptionWitValueRecord { .. } => "OptionWitValueRecord",
                RetShape::FirstWitValueRecord { .. } => "FirstWitValueRecord",
                RetShape::FirstInt => "FirstInt",
                RetShape::FirstReal => "FirstReal",
                RetShape::FirstText => "FirstText",
                RetShape::Enum { .. } => "Enum",
                RetShape::OptionEnum { .. } => "OptionEnum",
                RetShape::JsonText { .. } => "JsonText",
                RetShape::ListBool => "ListBool",
                RetShape::ListListU8 => "ListListU8",
                RetShape::TuplePick { .. } => "TuplePick",
                RetShape::Unit => "Unit",
            };
            *ret_hist.entry(name).or_insert(0) += 1;
        }
        eprintln!("[datafission-dynlink] skipped-arm ret-shape histogram:");
        for (k, v) in ret_hist {
            eprintln!("  - {} × {}", k, v);
        }
        // Per-arm dump gated on env var — the default keeps CI log
        // volume down while `SKIPPED_ARM_DUMP=1` lets an operator
        // enumerate the exact set (with param + return shapes) when
        // triaging which shape to admit next.
        let dump_all = std::env::var("SKIPPED_ARM_DUMP")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let head = if dump_all { skipped.len() } else { skipped.len().min(5) };
        for e in &skipped[..head] {
            if dump_all {
                let ps: Vec<String> = e.shape.params.iter().map(|p| format!("{:?}", p)).collect();
                eprintln!(
                    "  - {} :: params=[{}] ret={:?}",
                    e.sql_name,
                    ps.join(", "),
                    e.shape.ret,
                );
            } else {
                eprintln!("  - {}", e.sql_name);
            }
        }
        if !dump_all && skipped.len() > 5 {
            eprintln!("  - ... ({} more; SKIPPED_ARM_DUMP=1 to enumerate)", skipped.len() - 5);
        }
    }

    // Extension package version for identity::api_version().
    let api_version = "1.0.0".to_string();
    let provider_id = &opts.provider_id;

    // Build the aggregate_function_registry impl. Wired arms are
    // `AccKind::Geom` with `extra_args.is_empty()` and a supported
    // return shape (GeomBlob / BboxBlob / Bbox3dWkbLineZ / Real /
    // Int / Text plus their Option variants); other AccKind
    // variants surface in `list_functions` for SQL discoverability
    // but their `finalize` returns `UnknownFunction`. Empty entry
    // list falls back to the empty-registry stub.
    let (aggregate_impl_block, agg_wired, agg_stubbed) =
        build_aggregate_registry_impl_dynlink(&aggregate_entries);
    eprintln!(
        "[datafission-dynlink] aggregate entries: {} wired, {} stubbed",
        agg_wired, agg_stubbed
    );

    // Stubs for the five non-aggregate non-scalar exports —
    // advertised as empty (window / table / multi-custom-type) or
    // honest no-op (spatial-index / system-catalog / index) so the
    // composite world's `ExtensionBindings::instantiate` binding is
    // satisfied. Mirrors the shape emit_lib.rs produces for wac-plug
    // bridges, adapted for the dynlink bridge's no_std + alloc
    // environment.
    let non_scalar_stubs = build_non_scalar_stubs(primary, &aggregate_impl_block);

    // Build the scalar_function_registry match arms.
    let mut metas_block = String::new();
    let mut return_arms = String::new();
    let mut execute_arms = String::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (entry, fallible) in &primitive_arms {
        if !seen.insert(entry.sql_name.clone()) {
            continue;
        }
        let sql = &entry.sql_name;
        let escaped = sql.replace('"', "\\\"");
        // Signature param types
        let sig = entry
            .shape
            .params
            .iter()
            .map(param_to_logicaltype_lit)
            .collect::<Vec<_>>()
            .join(", ");
        metas_block.push_str(&format!(
            "        ftypes::ScalarFunctionMeta {{ name: \"{escaped}\".to_string(), aliases: alloc::vec![], param_types: alloc::vec![alloc::vec![{sig}]], is_deterministic: true, propagates_null: true }},\n",
        ));
        return_arms.push_str(&format!(
            "            \"{escaped}\" => Ok({}),\n",
            ret_to_logicaltype_lit(&entry.shape.ret),
        ));

        // Execute arm body: build CBOR Request::args, invoke, decode Response.
        // The provider dispatcher matches on the WIT method name in
        // kebab-case (see e.g. `"st-geom-from-text"` in
        // postgis-wasm/crates/provider/src/dispatch/postgis_core.rs).
        // `entry.sql_name` is the postgis SQL alias in snake_case
        // (`st_geomfromtext`) and doesn't match — use the WIT function
        // name (`st_geom_from_text`) converted to kebab.
        //
        // #823: `RetShape::TopoGeometryViaGeom` arms route through the
        // provider's `-via-geom` companion (`create-topo-geom-via-geom`
        // / `to-topogeom-via-geom`), which emits `CborValue::Bytes`
        // for the assembled MULTI* WKB instead of the base arm's tgm
        // wire form. The base arm names stay reserved for callers who
        // need the tgm wire (e.g. `topogeom-add-element` chains).
        let mut invoke_name = entry.shape.wit_func.replace('_', "-");
        if matches!(entry.shape.ret, RetShape::TopoGeometryViaGeom) {
            invoke_name.push_str("-via-geom");
        }
        let body = emit_scalar_arm_body(sql, &invoke_name, &entry.shape.params, &entry.shape.ret, *fallible);
        execute_arms.push_str(&format!(
            "            \"{escaped}\" => {{\n{body}\n            }}\n",
        ));
    }

    // Emit non-primitive skipped arms as list_functions entries with
    // stub execute → UnknownFunction. Preserves SQL discoverability.
    let mut stub_meta = String::new();
    let mut stub_return = String::new();
    let mut stub_execute = String::new();
    let mut stub_seen: BTreeSet<String> = BTreeSet::new();
    for entry in &skipped {
        if !stub_seen.insert(entry.sql_name.clone()) {
            continue;
        }
        let escaped = entry.sql_name.replace('"', "\\\"");
        let sig = entry
            .shape
            .params
            .iter()
            .map(param_to_logicaltype_lit_stub)
            .collect::<Vec<_>>()
            .join(", ");
        stub_meta.push_str(&format!(
            "        ftypes::ScalarFunctionMeta {{ name: \"{escaped}\".to_string(), aliases: alloc::vec![], param_types: alloc::vec![alloc::vec![{sig}]], is_deterministic: true, propagates_null: true }},\n",
        ));
        stub_return.push_str(&format!(
            "            \"{escaped}\" => Ok({}),\n",
            ret_to_logicaltype_lit_stub(&entry.shape.ret),
        ));
        stub_execute.push_str(&format!(
            "            \"{escaped}\" => Err(ftypes::FunctionError::UnknownFunction(\"{escaped}: dynlink-mode bridge does not yet support this shape (Phase 3 Commit 6 scope)\".to_string())),\n",
        ));
    }

    Ok(format!(
        r##"//! Auto-generated by `datalink_shim_datafission_emit::emit_dynlink`
//! (Phase 3 Commit 6b, #823). Do NOT edit by hand — regenerate.
#![no_std]
#![allow(unused_imports, dead_code)]
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
    }});
}}

use bindings::datafission::function_plugin::types as ftypes;
use bindings::datafission::sql_extension_plugin::types as setypes;
use bindings::datafission::type_plugin::types as ttypes;
use bindings::datafission::spatial_index_plugin::types as sitypes;
use bindings::datafission::system_catalog_plugin::types as sctypes;
use bindings::datafission::index_plugin::types as ixtypes;

use bindings::exports::datafission::extension::identity;
use bindings::exports::datafission::sql_extension_plugin::metadata as se_meta;
use bindings::exports::datafission::function_plugin::scalar_function_registry;
use bindings::exports::datafission::function_plugin::aggregate_function_registry;
use bindings::exports::datafission::function_plugin::window_function_registry;
use bindings::exports::datafission::function_plugin::table_function_registry;
use bindings::exports::datafission::type_plugin::multi_custom_type;
use bindings::exports::datafission::spatial_index_plugin::spatial_index;
use bindings::exports::datafission::system_catalog_plugin::system_catalog;
use bindings::exports::datafission::index_plugin::index;

use bindings::compose::dynlink::linker;

const PROVIDER_ID: &str = "{provider_id}";

fn resolve() -> Result<linker::Instance, ftypes::FunctionError> {{
    linker::resolve_by_id(&PROVIDER_ID.to_string())
        .map_err(|e| ftypes::FunctionError::ExecutionError(format!("dynlink resolve('{{}}'): {{:?}}", PROVIDER_ID, e)))
}}

// -----------------------------------------------------------
// CBOR envelope (mirrors provider crate's Request/Response).
// -----------------------------------------------------------

// CborValue pins the wire form at the bare CBOR type of each variant
// via manual `Serialize` — matching the provider-side envelope. The
// derive would emit unit variant `Null` as bare CBOR text `"Null"`
// (variant name) and tuple variant `Int(42)` as map `{{"Int": 42}}`;
// that asymmetry lands `CborValue::Null` on the wire as text and the
// provider decodes it as `Text("Null")`. The custom impl emits every
// variant as its native CBOR type (Null -> null, Bool -> bool, etc.)
// so the round-trip is symmetric on both sides.
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
    // The provider's envelope decoder (postgis-wasm-provider's
    // `envelope::Request`) declares `#[serde(rename = "v")] version`
    // — the wire form of the version tag is the one-char key `v`,
    // not `version`. Missing this rename here surfaces at the
    // FIRST dynlink call as `envelope decode: Semantic(None,
    // "missing field `v`")` because ciborium round-trips struct
    // field names verbatim.
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

// Response `ok` accepts either the bare CBOR wire type (new form,
// matches the provider's manual Serialize) or the legacy single-key
// map form (`{{"Int": 42}}`) for backward compat with any wasm still
// built against the old derive. Every visitor routes to the matching
// ResponseValue variant.
//
// `List` admits `CborValue::List(...)` payloads used by `RetShape::
// BboxBlob`, `ListListU8`, `ListBool`, and `JsonText` (nested list
// of primitives). The variant carries `Vec<ResponseValue>` so nested
// list-of-list shapes decode recursively.
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
                f.write_str("a CBOR value (null, bool, int, uint, float, text, bytes, list) or single-key map")
            }}
            fn visit_unit<E: Error>(self) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Null)
            }}
            fn visit_none<E: Error>(self) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Null)
            }}
            fn visit_bool<E: Error>(self, v: bool) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Bool(v))
            }}
            fn visit_i64<E: Error>(self, v: i64) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Int(v))
            }}
            fn visit_u8<E: Error>(self, v: u8) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Int(v as i64))
            }}
            fn visit_u16<E: Error>(self, v: u16) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Int(v as i64))
            }}
            fn visit_u32<E: Error>(self, v: u32) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Int(v as i64))
            }}
            // Positive CBOR unsigned int is inherently ambiguous — the
            // wire form for `Int(n)` and `Uint(n)` with n >= 0 is
            // identical (major type 0). Canonicalise positive values
            // that fit in i64 as `Int` so timestamps and other s64
            // fields round-trip cleanly. Values above i64::MAX still
            // decode as `Uint` to preserve the full u64 range.
            fn visit_u64<E: Error>(self, v: u64) -> Result<ResponseValue, E> {{
                if v <= i64::MAX as u64 {{
                    Ok(ResponseValue::Int(v as i64))
                }} else {{
                    Ok(ResponseValue::Uint(v))
                }}
            }}
            fn visit_f64<E: Error>(self, v: f64) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Float(v))
            }}
            fn visit_str<E: Error>(self, v: &str) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Text(v.to_string()))
            }}
            fn visit_string<E: Error>(self, v: String) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Text(v))
            }}
            fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Bytes(v.to_vec()))
            }}
            fn visit_byte_buf<E: Error>(self, v: Vec<u8>) -> Result<ResponseValue, E> {{
                Ok(ResponseValue::Bytes(v))
            }}
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<ResponseValue, A::Error> {{
                let mut items = Vec::new();
                while let Some(v) = s.next_element::<ResponseValue>()? {{
                    items.push(v);
                }}
                Ok(ResponseValue::List(items))
            }}
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<ResponseValue, A::Error> {{
                let k: Option<String> = m.next_key()?;
                let k = k.ok_or_else(|| A::Error::custom("empty map for ResponseValue"))?;
                let v = match k.as_str() {{
                    "Null" => {{ let _: serde::de::IgnoredAny = m.next_value()?; ResponseValue::Null }}
                    "Bool" => ResponseValue::Bool(m.next_value()?),
                    "Int" => ResponseValue::Int(m.next_value()?),
                    "Uint" => ResponseValue::Uint(m.next_value()?),
                    "Float" => ResponseValue::Float(m.next_value()?),
                    "Text" => ResponseValue::Text(m.next_value()?),
                    "Bytes" => {{
                        let b: serde_bytes::ByteBuf = m.next_value()?;
                        ResponseValue::Bytes(b.into_vec())
                    }}
                    "List" => ResponseValue::List(m.next_value()?),
                    other => return Err(A::Error::custom(alloc::format!("unknown ResponseValue tag: {{}}", other))),
                }};
                if let Some(extra) = m.next_key::<String>()? {{
                    return Err(A::Error::custom(alloc::format!("extra key {{}}", extra)));
                }}
                Ok(v)
            }}
        }}
        d.deserialize_any(V)
    }}
}}

// Serialize `ResponseValue` as a JSON-friendly tree — used only by
// `response_to_json` below for `JsonText` / `ListListU8` / `ListBool`
// returns. Bytes serialize as a JSON array of u8 numbers (matches
// what WacPlug's `serde_json::to_string(&Vec<u8>)` produces on the
// same shape — serde renders `Vec<u8>` as an array by default).
impl serde::Serialize for ResponseValue {{
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {{
        use serde::ser::SerializeSeq;
        match self {{
            ResponseValue::Null => s.serialize_unit(),
            ResponseValue::Bool(b) => s.serialize_bool(*b),
            ResponseValue::Int(i) => s.serialize_i64(*i),
            ResponseValue::Uint(u) => s.serialize_u64(*u),
            ResponseValue::Float(f) => s.serialize_f64(*f),
            ResponseValue::Text(t) => s.serialize_str(t),
            ResponseValue::Bytes(b) => {{
                let mut seq = s.serialize_seq(Some(b.len()))?;
                for byte in b {{
                    seq.serialize_element(byte)?;
                }}
                seq.end()
            }}
            ResponseValue::List(items) => {{
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {{
                    seq.serialize_element(item)?;
                }}
                seq.end()
            }}
        }}
    }}
}}

/// Render a `ResponseValue` as JSON text. Mirrors what WacPlug emits
/// via `serde_json::to_string(&<upstream Rust value>)` for the same
/// SQL arm. Records are the one non-generic case (WacPlug renders
/// records as JSON objects with named fields, while the provider
/// encodes them positionally as `CborValue::List(...)` on the wire,
/// so record-based `JsonText` arms produce a positional array here
/// rather than an object). All other `JsonText` kinds map
/// byte-identically.
fn response_to_json(v: &ResponseValue, ctx: &str) -> Result<String, ftypes::FunctionError> {{
    serde_json::to_string(v).map_err(|e| {{
        ftypes::FunctionError::ExecutionError(alloc::format!("{{}}: encode JSON: {{}}", ctx, e))
    }})
}}

/// Build a WKB polygon (little-endian, type 3 POLYGON) from a
/// `bbox {{ min_x, min_y, max_x, max_y }}`. Mirrors WacPlug's
/// `pg_ctor::st_make_envelope` — a closed 5-point ring around the
/// bbox. 93 bytes.
fn bbox_to_wkb_polygon(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Vec<u8> {{
    let mut w: Vec<u8> = Vec::with_capacity(93);
    w.push(0x01u8);
    w.extend_from_slice(&3u32.to_le_bytes());
    w.extend_from_slice(&1u32.to_le_bytes());
    w.extend_from_slice(&5u32.to_le_bytes());
    for (x, y) in [
        (min_x, min_y),
        (max_x, min_y),
        (max_x, max_y),
        (min_x, max_y),
        (min_x, min_y),
    ] {{
        w.extend_from_slice(&x.to_le_bytes());
        w.extend_from_slice(&y.to_le_bytes());
    }}
    w
}}

/// Build a WKB linestring-Z (little-endian, type 1002 LINESTRING Z,
/// 2 vertices) from a `bbox3d {{ min_x, min_y, min_z, max_x, max_y,
/// max_z }}`. Mirrors WacPlug's `Bbox3dWkbLineZ` return-shape
/// rendering — the diagonal from min-corner to max-corner preserves
/// all six coordinates and parses as standard WKB. 57 bytes.
fn bbox3d_to_wkb_linestring_z(
    min_x: f64, min_y: f64, min_z: f64,
    max_x: f64, max_y: f64, max_z: f64,
) -> Vec<u8> {{
    let mut w: Vec<u8> = Vec::with_capacity(57);
    w.push(0x01u8);
    w.extend_from_slice(&1002u32.to_le_bytes());
    w.extend_from_slice(&2u32.to_le_bytes());
    w.extend_from_slice(&min_x.to_le_bytes());
    w.extend_from_slice(&min_y.to_le_bytes());
    w.extend_from_slice(&min_z.to_le_bytes());
    w.extend_from_slice(&max_x.to_le_bytes());
    w.extend_from_slice(&max_y.to_le_bytes());
    w.extend_from_slice(&max_z.to_le_bytes());
    w
}}

/// Decode a `ResponseValue::List` of 6 floats (xmin, ymin, zmin,
/// xmax, ymax, zmax) into an owned `[f64; 6]`. Matches the provider's
/// `st-extent-threed` wire discipline.
fn response_bbox3d_floats(v: &ResponseValue, ctx: &str) -> Result<[f64; 6], ftypes::FunctionError> {{
    let items = match v {{
        ResponseValue::List(items) => items,
        other => return Err(ftypes::FunctionError::ExecutionError(
            alloc::format!("{{}}: expected List of 6 floats, got {{:?}}", ctx, other))),
    }};
    if items.len() != 6 {{
        return Err(ftypes::FunctionError::ExecutionError(
            alloc::format!("{{}}: expected 6-element bbox3d list, got {{}}", ctx, items.len())));
    }}
    let mut out = [0.0f64; 6];
    for (i, item) in items.iter().enumerate() {{
        out[i] = match item {{
            ResponseValue::Float(f) => *f,
            ResponseValue::Int(n) => *n as f64,
            ResponseValue::Uint(u) => *u as f64,
            other => return Err(ftypes::FunctionError::ExecutionError(
                alloc::format!("{{}}: bbox3d[{{}}] must be float, got {{:?}}", ctx, i, other))),
        }};
    }}
    Ok(out)
}}

/// Decode a `ResponseValue::List` of 4 floats (xmin, ymin, xmax, ymax)
/// into an owned `[f64; 4]`. Errors if the response isn't a 4-element
/// list of floats — matches the provider's `st-extent` / bbox-returning
/// arms' wire discipline.
fn response_bbox_floats(v: &ResponseValue, ctx: &str) -> Result<[f64; 4], ftypes::FunctionError> {{
    let items = match v {{
        ResponseValue::List(items) => items,
        other => return Err(ftypes::FunctionError::ExecutionError(
            alloc::format!("{{}}: expected List of 4 floats, got {{:?}}", ctx, other))),
    }};
    if items.len() != 4 {{
        return Err(ftypes::FunctionError::ExecutionError(
            alloc::format!("{{}}: expected 4-element bbox list, got {{}}", ctx, items.len())));
    }}
    let mut out = [0.0f64; 4];
    for (i, item) in items.iter().enumerate() {{
        out[i] = match item {{
            ResponseValue::Float(f) => *f,
            ResponseValue::Int(n) => *n as f64,
            ResponseValue::Uint(u) => *u as f64,
            other => return Err(ftypes::FunctionError::ExecutionError(
                alloc::format!("{{}}: bbox[{{}}] must be float, got {{:?}}", ctx, i, other))),
        }};
    }}
    Ok(out)
}}

fn encode_request(args: Vec<CborValue>) -> Result<Vec<u8>, ftypes::FunctionError> {{
    let mut out = Vec::new();
    ciborium::into_writer(&Request {{ version: 1, args }}, &mut out)
        .map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!("cbor encode: {{}}", e)))?;
    Ok(out)
}}

fn decode_response(bytes: &[u8]) -> Result<Response, ftypes::FunctionError> {{
    ciborium::from_reader(bytes)
        .map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!("cbor decode: {{}}", e)))
}}

/// Invoke `method` on the resident provider and unwrap its Ok payload.
fn call(method: &str, args: Vec<CborValue>) -> Result<ResponseValue, ftypes::FunctionError> {{
    let inst = resolve()?;
    let payload = encode_request(args)?;
    let bytes = inst
        .invoke(&method.to_string(), &payload)
        .map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!("{{}}: invoke: {{:?}}", method, e)))?;
    let resp = decode_response(&bytes)?;
    if let Some(err) = resp.err {{
        return Err(ftypes::FunctionError::ExecutionError(alloc::format!("{{}}: {{}}", method, err)));
    }}
    resp.ok.ok_or_else(|| {{
        ftypes::FunctionError::ExecutionError(alloc::format!("{{}}: empty response", method))
    }})
}}

// -----------------------------------------------------------
// SQL arg / return marshalling helpers.
// -----------------------------------------------------------

fn dfv_blob(args: &[ftypes::ScalarValue], i: usize, name: &str) -> Result<Vec<u8>, ftypes::FunctionError> {{
    match args.get(i) {{
        Some(ftypes::ScalarValue::Binary(b)) => Ok(b.clone()),
        Some(ftypes::ScalarValue::Utf8(s)) => Ok(s.as_bytes().to_vec()),
        _ => Err(ftypes::FunctionError::ExecutionError(alloc::format!("{{}}: arg {{}} must be BINARY", name, i))),
    }}
}}

fn dfv_f64(args: &[ftypes::ScalarValue], i: usize, name: &str) -> Result<f64, ftypes::FunctionError> {{
    match args.get(i) {{
        Some(ftypes::ScalarValue::Float64(f)) => Ok(*f),
        Some(ftypes::ScalarValue::Int64(i)) => Ok(*i as f64),
        _ => Err(ftypes::FunctionError::ExecutionError(alloc::format!("{{}}: arg {{}} must be DOUBLE", name, i))),
    }}
}}

fn dfv_i64(args: &[ftypes::ScalarValue], i: usize, name: &str) -> Result<i64, ftypes::FunctionError> {{
    match args.get(i) {{
        Some(ftypes::ScalarValue::Int64(v)) => Ok(*v),
        _ => Err(ftypes::FunctionError::ExecutionError(alloc::format!("{{}}: arg {{}} must be BIGINT", name, i))),
    }}
}}

fn dfv_bool(args: &[ftypes::ScalarValue], i: usize, name: &str) -> Result<bool, ftypes::FunctionError> {{
    match args.get(i) {{
        Some(ftypes::ScalarValue::Boolean(b)) => Ok(*b),
        _ => Err(ftypes::FunctionError::ExecutionError(alloc::format!("{{}}: arg {{}} must be BOOL", name, i))),
    }}
}}

fn dfv_text(args: &[ftypes::ScalarValue], i: usize, name: &str) -> Result<String, ftypes::FunctionError> {{
    match args.get(i) {{
        Some(ftypes::ScalarValue::Utf8(s)) => Ok(s.clone()),
        _ => Err(ftypes::FunctionError::ExecutionError(alloc::format!("{{}}: arg {{}} must be VARCHAR", name, i))),
    }}
}}

// -----------------------------------------------------------
// Guest impls.
// -----------------------------------------------------------

struct Component;

impl identity::Guest for Component {{
    fn name() -> String {{ "{primary}".to_string() }}
    fn version() -> String {{ "{version}".to_string() }}
    fn api_version() -> String {{ "{api_version}".to_string() }}
}}

impl se_meta::Guest for Component {{
    fn name() -> String {{ "{primary}".to_string() }}
    fn version() -> String {{ "{version}".to_string() }}
    fn list_cast_rewrites() -> Result<Vec<setypes::CastRewrite>, setypes::SqlExtError> {{ Ok(Vec::new()) }}
    fn list_operator_rewrites() -> Result<Vec<setypes::OperatorRewrite>, setypes::SqlExtError> {{ Ok(Vec::new()) }}
    fn list_preprocessor_patterns() -> Result<Vec<setypes::PreprocessorPattern>, setypes::SqlExtError> {{ Ok(Vec::new()) }}
}}

impl scalar_function_registry::Guest for Component {{
    fn list_functions() -> Vec<ftypes::ScalarFunctionMeta> {{
        alloc::vec![
{metas_block}{stub_meta}        ]
    }}

    fn return_type(name: String, _input_types: Vec<ftypes::LogicalType>) -> Result<ftypes::LogicalType, ftypes::FunctionError> {{
        match name.as_str() {{
{return_arms}{stub_return}            other => Err(ftypes::FunctionError::UnknownFunction(other.to_string())),
        }}
    }}

    fn execute(name: String, args: Vec<ftypes::ScalarValue>) -> Result<ftypes::ScalarValue, ftypes::FunctionError> {{
        if args.iter().any(|v| matches!(v, ftypes::ScalarValue::Null)) {{
            return Ok(ftypes::ScalarValue::Null);
        }}
        match name.as_str() {{
{execute_arms}{stub_execute}            other => Err(ftypes::FunctionError::UnknownFunction(other.to_string())),
        }}
    }}

    fn execute_batch(name: String, args_batch: Vec<Vec<ftypes::ScalarValue>>) -> Result<Vec<ftypes::ScalarValue>, ftypes::FunctionError> {{
        let mut out = Vec::with_capacity(args_batch.len());
        for row in args_batch {{
            out.push(<Self as scalar_function_registry::Guest>::execute(name.clone(), row)?);
        }}
        Ok(out)
    }}
}}

{non_scalar_stubs}

bindings::export!(Component with_types_in bindings);
"##,
        provider_id = provider_id,
        primary = primary,
        version = version,
        api_version = api_version,
        metas_block = metas_block,
        return_arms = return_arms,
        execute_arms = execute_arms,
        stub_meta = stub_meta,
        stub_return = stub_return,
        stub_execute = stub_execute,
        non_scalar_stubs = non_scalar_stubs,
    ))
}

// ============================================================
// Non-scalar Guest impl stubs.
// ============================================================

/// Concatenate all six non-scalar Guest impl blocks. Emitted after
/// the scalar-function-registry impl so the composite bridge world
/// has an export for every capability instance the runtime binds
/// against. Every block is either advertise-empty
/// (aggregate / window / table / multi-custom-type) or honest no-op
/// (spatial-index / system-catalog / index).
///
/// Mirrors the shape emit_lib.rs produces for wac-plug bridges,
/// adapted for the dynlink bridge's no_std + alloc environment
/// (no `vec![]`, no std `format!` prelude, error types resolved
/// through the interface-specific type alias).
fn build_non_scalar_stubs(primary: &str, aggregate_impl: &str) -> String {
    let mut s = String::new();
    if aggregate_impl.is_empty() {
        s.push_str(AGGREGATE_STUB);
    } else {
        s.push_str(aggregate_impl);
    }
    s.push_str(WINDOW_STUB);
    s.push_str(TABLE_STUB);
    s.push_str(MULTI_CUSTOM_TYPE_STUB);
    s.push_str(SPATIAL_INDEX_STUB);
    s.push_str(&build_system_catalog_stub(primary));
    s.push_str(&build_index_stub(primary));
    s
}

/// Advertises no aggregate functions; every per-call method returns
/// `UnknownFunction`. Handle-scoped methods return
/// `FunctionError::Internal` referencing the (non-existent) handle
/// so a runtime that racks call ordering sees a clean diagnostic.
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
            "no accumulator at handle {handle} (aggregates not wired in dynlink scalar-first cut)"
        )))
    }
    fn accumulate_batch(handle: u64, _values: Vec<ftypes::ScalarValue>) -> Result<(), ftypes::FunctionError> {
        Err(ftypes::FunctionError::Internal(format!(
            "no accumulator at handle {handle} (aggregates not wired in dynlink scalar-first cut)"
        )))
    }
    fn merge(target: u64, _source: u64) -> Result<(), ftypes::FunctionError> {
        Err(ftypes::FunctionError::Internal(format!(
            "no accumulator at handle {target} (aggregates not wired in dynlink scalar-first cut)"
        )))
    }
    fn finalize(handle: u64) -> Result<ftypes::ScalarValue, ftypes::FunctionError> {
        Err(ftypes::FunctionError::Internal(format!(
            "no accumulator at handle {handle} (aggregates not wired in dynlink scalar-first cut)"
        )))
    }
    fn reset(_handle: u64) {}
    fn destroy_accumulator(_handle: u64) {}
}

"##;

/// Advertises no window functions.
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

/// Advertises no table functions. `next_row` returns `None` on any
/// handle since no iterator was ever created.
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

/// Advertises no custom types; per-call ops return
/// `TypeError::Internal`. The scalar-first dynlink bridge doesn't
/// carry the record registry the WacPlug emitter uses to advertise
/// wit-value records, so custom-type registration is a follow-up.
const MULTI_CUSTOM_TYPE_STUB: &str = r##"impl multi_custom_type::Guest for Component {
    fn list_types() -> Vec<multi_custom_type::CustomTypeMeta> {
        Vec::new()
    }
    fn serialize(_type_id: u32, value: Vec<u8>) -> Vec<u8> { value }
    fn deserialize(type_id: u32, _bytes: Vec<u8>) -> Result<Vec<u8>, ttypes::TypeError> {
        Err(ttypes::TypeError::Internal(format!(
            "dynlink bridge advertises no custom types (id {type_id})"
        )))
    }
    fn compare(_type_id: u32, _a: Vec<u8>, _b: Vec<u8>) -> ttypes::Ordering {
        ttypes::Ordering::Equal
    }
    fn hash_value(_type_id: u32, _value: Vec<u8>) -> u64 { 0 }
    fn display(type_id: u32, _value: Vec<u8>) -> String {
        format!("<type {type_id} (dynlink stub)>")
    }
    fn parse(type_id: u32, _input: String) -> Result<Vec<u8>, ttypes::TypeError> {
        Err(ttypes::TypeError::Internal(format!(
            "dynlink bridge advertises no custom types (id {type_id})"
        )))
    }
}

"##;

/// Advertises no aliases, no capabilities; every op returns
/// `SpatialError::UnsupportedOperation`. Downstream wac-plug bridges
/// route the postgis STRtree through here; the dynlink bridge would
/// dispatch through `compose:dynlink/linker` instead, but that path
/// is deferred to a follow-up.
const SPATIAL_INDEX_STUB: &str = r##"impl spatial_index::Guest for Component {
    fn name() -> String { "dynlink-stub-spatial".to_string() }
    fn aliases() -> Vec<String> { Vec::new() }
    fn capabilities() -> sitypes::IndexCapabilities {
        sitypes::IndexCapabilities {
            knn: false,
            within_distance: false,
            within_distance_wkb: false,
            update_after_build: false,
        }
    }
    fn build(_items: Vec<sitypes::BuildItem>) -> Result<u64, sitypes::SpatialError> {
        Err(sitypes::SpatialError::UnsupportedOperation(
            "spatial-index not wired in dynlink scalar-first cut".to_string(),
        ))
    }
    fn entry_count(_handle: u64) -> u64 { 0 }
    fn query_envelope(_handle: u64, _env: sitypes::Envelope) -> Result<Vec<u64>, sitypes::SpatialError> {
        Err(sitypes::SpatialError::UnsupportedOperation(
            "spatial-index not wired in dynlink scalar-first cut".to_string(),
        ))
    }
    fn query_knn(_handle: u64, _query_bytes: Vec<u8>, _k: u32) -> Result<Vec<u64>, sitypes::SpatialError> {
        Err(sitypes::SpatialError::UnsupportedOperation(
            "spatial-index not wired in dynlink scalar-first cut".to_string(),
        ))
    }
    fn query_within_distance(_handle: u64, _query_env: sitypes::Envelope, _distance: f64) -> Result<Vec<u64>, sitypes::SpatialError> {
        Err(sitypes::SpatialError::UnsupportedOperation(
            "spatial-index not wired in dynlink scalar-first cut".to_string(),
        ))
    }
    fn query_within_distance_wkb(_handle: u64, _query_wkb: Vec<u8>, _distance: f64) -> Result<Vec<u64>, sitypes::SpatialError> {
        Err(sitypes::SpatialError::UnsupportedOperation(
            "spatial-index not wired in dynlink scalar-first cut".to_string(),
        ))
    }
    fn destroy(_handle: u64) {}
}

"##;

/// `catalog_name()` reports the primary shim name so a host that
/// enumerates catalogs sees a real provider; `list_tables()` returns
/// the empty vector (honest discovery). Notify entrypoints stay
/// no-ops. Read arms return a clearly labelled `CatalogError::Internal`.
fn build_system_catalog_stub(primary: &str) -> String {
    format!(
        r##"impl system_catalog::Guest for Component {{
    fn catalog_name() -> String {{ "{primary}".to_string() }}
    fn list_tables() -> Result<Vec<sctypes::SystemTable>, sctypes::CatalogError> {{
        Ok(Vec::new())
    }}
    fn read_table(
        _table_name: String,
    ) -> Result<Vec<Vec<sctypes::ScalarValue>>, sctypes::CatalogError> {{
        Err(unimpl_catalog_err("read_table"))
    }}
    fn read_table_for_session(
        _session_id: u64,
        _table_name: String,
    ) -> Result<Vec<Vec<sctypes::ScalarValue>>, sctypes::CatalogError> {{
        Err(unimpl_catalog_err("read_table_for_session"))
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
    fn notify_extension_column_raster_metadata(
        _session_id: u64,
        _catalog: String,
        _schema: String,
        _table_name: String,
        _column_name: String,
        _metadata: sctypes::RasterColumnMetadata,
    ) {{}}
}}

fn unimpl_catalog_err(op: &str) -> sctypes::CatalogError {{
    sctypes::CatalogError::Internal(format!(
        "{primary} system-catalog-plugin: {{op}} not implemented \
         (dynlink bridge advertises no catalog tables)"
    ))
}}

"##,
        primary = primary,
    )
}

/// `name()` reports `<primary>-stub-index` so a host dumping the
/// export by name doesn't mistake the stub for a real plugin;
/// `type_id()` returns 0; `supported_types()` is empty; capabilities
/// all false. Per-op methods return `IndexError::Internal` naming
/// both the primary shim and the WIT method.
fn build_index_stub(primary: &str) -> String {
    let methods: &[(&str, &str, &str)] = &[
        (
            "create",
            "_options: Vec<(String, String)>",
            "Result<u64, ixtypes::IndexError>",
        ),
        (
            "insert",
            "_handle: u64, _key: Vec<ixtypes::ScalarValue>, _row_id: u64",
            "Result<(), ixtypes::IndexError>",
        ),
        (
            "delete",
            "_handle: u64, _key: Vec<ixtypes::ScalarValue>",
            "Result<bool, ixtypes::IndexError>",
        ),
        (
            "contains",
            "_handle: u64, _key: Vec<ixtypes::ScalarValue>",
            "Result<bool, ixtypes::IndexError>",
        ),
        (
            "get",
            "_handle: u64, _key: Vec<ixtypes::ScalarValue>",
            "Result<Option<u64>, ixtypes::IndexError>",
        ),
        (
            "stats",
            "_handle: u64",
            "Result<ixtypes::IndexStats, ixtypes::IndexError>",
        ),
        (
            "bulk_load",
            "_handle: u64, _entries: Vec<ixtypes::IndexEntry>",
            "Result<(), ixtypes::IndexError>",
        ),
        (
            "serialize",
            "_handle: u64",
            "Result<Vec<u8>, ixtypes::IndexError>",
        ),
        (
            "deserialize",
            "_data: Vec<u8>",
            "Result<u64, ixtypes::IndexError>",
        ),
    ];
    let mut per_op = String::new();
    for (method, params, ret) in methods {
        per_op.push_str(&format!(
            "    fn {method}({params}) -> {ret} {{\n        \
             Err(unimpl_index_err(\"{method}\"))\n    }}\n",
        ));
    }
    format!(
        r##"impl index::Guest for Component {{
    fn name() -> String {{ "{primary}-stub-index".to_string() }}
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
    fn destroy(_handle: u64) {{}}
    fn begin_scan(
        _handle: u64,
        _start: ixtypes::ScanBound,
        _end: ixtypes::ScanBound,
        _direction: ixtypes::ScanDirection,
        _limit: Option<u64>,
    ) -> Result<u64, ixtypes::IndexError> {{
        Err(unimpl_index_err("begin_scan"))
    }}
    fn next(_cursor: u64) -> Option<Result<ixtypes::IndexEntry, ixtypes::IndexError>> {{
        None
    }}
    fn close_scan(_cursor: u64) {{}}
{per_op}}}

fn unimpl_index_err(op: &str) -> ixtypes::IndexError {{
    ixtypes::IndexError::Internal(format!(
        "{primary} index-plugin: {{op}} not implemented \
         (dynlink bridge advertises no generic index)"
    ))
}}

"##,
        primary = primary,
        per_op = per_op,
    )
}

// ============================================================
// Shape helpers.
// ============================================================

fn is_primitive_shape(params: &[ParamShape], ret: &RetShape) -> bool {
    // In dynlink mode, any input that lowers to CBOR bytes is fine
    // — we forward WKB verbatim through the envelope. Arg shapes
    // that require materialising a resource handle (Geom, Raster,
    // Topology) do NOT work: the provider's dispatcher expects
    // bytes on the wire, not a wit-value resource lift.
    //
    // Similarly on the return side: shapes whose WacPlug body
    // re-wraps a resource (GeomBlob / RasterBlob / TopologyBlob)
    // reduce to "just bytes" in dynlink mode — the provider emits
    // WKB as CBOR::Bytes and we hand it back as Binary. Real,
    // Text, Int, BoolInt lower to CBOR Float/Text/Int/Bool.
    params.iter().all(|p| matches!(p,
        ParamShape::Blob | ParamShape::F64
            | ParamShape::S32 | ParamShape::S64
            | ParamShape::U32 | ParamShape::U64
            | ParamShape::Bool | ParamShape::Text
            | ParamShape::Geom | ParamShape::Geog
            | ParamShape::Raster | ParamShape::Topology
            | ParamShape::OptionNone
            | ParamShape::Enum { .. }
            | ParamShape::ListPrim(_)
            | ParamShape::ListListU8
            // #823: `list<borrow<geometry>>` / `list<geometry>` — SQL
            // surface is a JSON-array TEXT literal of WKB byte arrays
            // (`'[[<u8>...], [<u8>...], ...]'`). Same wire shape as
            // `ListListU8` — the arm body parses `Vec<Vec<u8>>` and
            // forwards as `CborValue::List(<CborValue::Bytes>)`. Provider
            // dispatchers decode via `decode_geometry_list`. Unlocks
            // `st_as_geobuf`, `st_as_mvt`, `st_collect`, and other
            // variadic-geometry arms.
            | ParamShape::ListGeom
            // WitValueRecord param — SQL surface is a Binary literal
            // carrying the WTV-framed CBOR envelope. Same wire shape
            // as `Blob` / `Geom` — the arm body forwards the SQL
            // Binary bytes verbatim as `CborValue::Bytes`; the
            // provider dispatcher re-decodes the record via
            // `ciborium::from_reader::<UPSTREAM, _>` (upstream WIT
            // record type derives serde::Deserialize under
            // `additional_derives`). Unlocks arms like the mobilitydb
            // temporal-value inputs and the postgis `raster` param
            // that carries record-typed data through the boundary.
            | ParamShape::WitValueRecord { .. }
            // #823: `list<record>` param. SQL passes JSON-array TEXT
            // literal of record objects (e.g.
            // `'[{{"element_id": 1, "element_type": 1}}, ...]'` for
            // `topo-element` list). Emit body parses the JSON to
            // `Vec<serde_json::Value>`, ciborium-encodes each element
            // into its own byte buffer, and wraps the whole thing as
            // `CborValue::List(Vec<CborValue::Bytes>)` — mirrors the
            // `WitValueRecord` param wire shape one list level deep.
            // Provider dispatcher decodes each `Bytes` payload via
            // `ciborium::from_reader::<UPSTREAM, _>` (upstream WIT
            // record type derives serde::Deserialize under
            // `additional_derives`) and reassembles a `Vec<UPSTREAM>`
            // for the WIT call. Unlocks arms like
            // `topo-element-array-append` and the
            // `create-topo-geom` / `topology-create-topo-geom`
            // topo-element-list inputs.
            | ParamShape::ListRecord { .. }
            // #823: `list<list<primitive>>` param over a non-u8
            // element (u8 uses the dedicated `ListListU8` shape).
            // SQL passes JSON-array TEXT literal matching
            // `Vec<Vec<T>>` (e.g. `'[[1.0, 2.0], [3.0, 4.0]]'` for
            // `list<list<f64>>`). Emit body parses locally into
            // `Vec<Vec<T>>` via `serde_json::from_str` and forwards
            // as `CborValue::List(Vec<CborValue::List(<CborValue::<Prim>>...)>)`
            // — nested list of primitives on the wire. Provider
            // dispatcher decodes recursively. Unlocks postgis
            // raster `st-set-values` (`list<list<f64>>` values).
            | ParamShape::ListListPrim(_)
    )) && matches!(ret,
        RetShape::Text | RetShape::Real | RetShape::Int
            | RetShape::Blob | RetShape::BoolInt
            | RetShape::GeomBlob | RetShape::RasterBlob | RetShape::TopologyBlob
            // #823: `topo-geometry` resource return, lifted via the
            // provider's `-via-geom` companion arm to the assembled
            // MULTI* WKB. The provider ships `CborValue::Bytes`
            // (`topo_geometry.geometry().as_wkb()`); bridge decode is
            // the same `Bytes -> Binary` path as `GeomBlob`. Provider
            // side: see `create-topo-geom-via-geom` /
            // `to-topogeom-via-geom` in
            // `postgis-wasm/crates/provider/src/dispatch/postgis_topology.rs`.
            | RetShape::TopoGeometryViaGeom
            // #823: SFCGAL validity report — `tuple<bool,
            // option<string>, option<geometry>>` with the location
            // geometry lifted to WKT via `st_as_text`. Provider ships
            // `CborValue::List([Bool, Text-or-Null, Text-or-Null])`;
            // bridge decode formats as the PostgreSQL composite-type
            // text `(valid,"reason","location")` — same SQL surface
            // WacPlug produces.
            | RetShape::IsValidDetailText
            // Option<T> returns — provider emits `CborValue::<T>` on
            // Some and `CborValue::Null` on None; wrap side turns
            // Null into `SqlValue::Null`.
            | RetShape::OptionText | RetShape::OptionReal | RetShape::OptionInt
            | RetShape::OptionBlob | RetShape::OptionGeomBlob
            | RetShape::OptionRasterBlob | RetShape::OptionTopologyBlob
            // #823: `option<bool>` — provider emits `CborValue::Bool(v)` on
            // Some and `CborValue::Null` on None; wrap turns Null into SQL
            // Null and Bool into `ScalarValue::Boolean`. Same shape family
            // as the other Option primitives above. Unlocks the mobilitydb
            // `tbool-value-at` / `tbool-start-value` / `tbool-end-value` /
            // `tbool-value-n` / `bitemporal-bool-*` / `tgeography-is-simple`
            // family (10 scalars, provider dispatch arms still pending in
            // mobilitydb-wasm/crates/provider).
            | RetShape::OptionBoolInt
            // list<T>-then-first projections — provider does the
            // `.first()` conversion internally and emits Bytes (or
            // Null when the list is empty). Same wire as *Blob for
            // dynlink purposes.
            | RetShape::FirstGeomBlob | RetShape::FirstRasterBlob
            | RetShape::FirstTopologyBlob
            // list<primitive>-then-first projections — provider does
            // the `.first()` conversion and emits `CborValue::<T>`
            // (or Null when the list is empty). Same wire as
            // Option<T> for dynlink purposes.
            | RetShape::FirstInt | RetShape::FirstReal | RetShape::FirstText
            // `result<_, E>` unit-OK — surfaces as SQL NULL. The
            // provider dispatcher may emit `CborValue::Null` for
            // pure mutators or a fallback payload (bytes for
            // topology mutators that return the modified topo)
            // that the decode arm discards. Either way the SQL
            // surface is NULL.
            | RetShape::Unit
            // #823 W4b: `List`-returning shapes decoded via
            // `ResponseValue::List(...)`. `BboxBlob` decodes 4
            // floats and re-encodes as WKB polygon (matches
            // WacPlug's `pg_ctor::st_make_envelope`). `ListListU8`
            // / `ListBool` / `JsonText` render as JSON text via
            // `response_to_json`.
            | RetShape::BboxBlob
            | RetShape::ListListU8
            | RetShape::ListBool
            | RetShape::JsonText { .. }
            // #823: `tuple<X0, X1, ...>` return with a per-arm
            // element-index override that projects a single tuple
            // slot to a SQL scalar. Provider emits `CborValue::List`
            // of primitives (matches the WacPlug path); bridge picks
            // position `index` and wraps per `elem`.
            | RetShape::TuplePick { .. }
            // #823: WIT enum return. Provider serialises the case
            // ordinal as `CborValue::Uint(n)` (see e.g.
            // `pixel_type_to_u64` in
            // `postgis-wasm-provider::dispatch::postgis_raster`);
            // bridge wraps as `ScalarValue::Int64` matching the
            // WacPlug convention. `OptionEnum` adds Null for None —
            // no postgis arm classifies as OptionEnum today but the
            // wire shape is the same modulo Null-passthrough.
            | RetShape::Enum { .. }
            | RetShape::OptionEnum { .. }
            // WitValueRecord returns — the provider re-serialises the
            // WIT-generated record as CBOR bytes and wraps in
            // `CborValue::Bytes`. Bridge forwards as SQL Binary. The
            // magic-prefix WTV envelope adds `WTV\x01` + type_id + CBOR
            // payload on the emit side so downstream consumers can
            // rebuild the typed record. `FirstWitValueRecord` is the
            // `list<R>`-then-first projection; the provider does the
            // `.first()` conversion and either emits `CborValue::Bytes`
            // (single-record CBOR envelope) or `CborValue::Null` on
            // empty. `OptionWitValueRecord` follows the same shape.
            | RetShape::WitValueRecord { .. }
            | RetShape::OptionWitValueRecord { .. }
            | RetShape::FirstWitValueRecord { .. }
    )
}

fn param_to_logicaltype_lit(p: &ParamShape) -> String {
    match p {
        ParamShape::Blob
        | ParamShape::Geom
        | ParamShape::Geog
        | ParamShape::Raster
        | ParamShape::Topology
        // WitValueRecord param — SQL Binary carrying the WTV-framed
        // CBOR envelope. Provider ciborium-decodes into the
        // wit-bindgen record type.
        | ParamShape::WitValueRecord { .. } => "ftypes::LogicalType::Binary".to_string(),
        ParamShape::F64 => "ftypes::LogicalType::Float64".to_string(),
        ParamShape::S32
        | ParamShape::S64
        | ParamShape::U32
        | ParamShape::U64
        | ParamShape::Enum { .. } => "ftypes::LogicalType::Int64".to_string(),
        ParamShape::Bool => "ftypes::LogicalType::Boolean".to_string(),
        // ListPrim + ListListU8 arrive as a JSON-array TEXT literal
        // at the SQL surface. OptionNone consumes a SQL slot (see
        // WacPlug's `param_to_logicaltype_lit`) whose value the arm
        // body ignores; Utf8 matches the WacPlug convention.
        ParamShape::Text
        | ParamShape::ListPrim(_)
        | ParamShape::ListListU8
        // ListGeom lowers to a JSON-array TEXT literal of WKB byte
        // arrays — same SQL surface as ListListU8.
        | ParamShape::ListGeom
        // #823: `list<record>` — SQL passes JSON-array TEXT literal
        // of record objects; emit body ciborium-encodes each record
        // into `CborValue::Bytes`. Wire shape at the SQL layer is
        // Utf8, matching WacPlug's `parse_json_list_record_<snake>`
        // helper convention.
        | ParamShape::ListRecord { .. }
        // #823: `list<list<primitive>>` — SQL passes JSON-array TEXT
        // literal of arrays of primitives. Same SQL surface as
        // `ListListU8` but the element type is arbitrary primitive
        // (non-u8).
        | ParamShape::ListListPrim(_)
        | ParamShape::OptionNone => "ftypes::LogicalType::Utf8".to_string(),
        _ => "ftypes::LogicalType::Binary".to_string(),
    }
}

fn param_to_logicaltype_lit_stub(p: &ParamShape) -> String {
    // For skipped/stub arms just pick Binary so the meta compiles.
    let _ = p;
    "ftypes::LogicalType::Binary".to_string()
}

fn ret_to_logicaltype_lit(r: &RetShape) -> String {
    match r {
        RetShape::Text | RetShape::OptionText | RetShape::FirstText => {
            "ftypes::LogicalType::Utf8".to_string()
        }
        // #823: `IsValidDetailText` renders as PostgreSQL composite-type
        // text `(valid,"reason","location")` — Utf8 on the SQL surface.
        RetShape::IsValidDetailText => "ftypes::LogicalType::Utf8".to_string(),
        RetShape::Real | RetShape::OptionReal | RetShape::FirstReal => {
            "ftypes::LogicalType::Float64".to_string()
        }
        RetShape::Int | RetShape::OptionInt | RetShape::FirstInt => {
            "ftypes::LogicalType::Int64".to_string()
        }
        RetShape::Blob
        | RetShape::GeomBlob
        | RetShape::RasterBlob
        | RetShape::TopologyBlob
        // #823: `TopoGeometryViaGeom` returns WKB Binary via the
        // provider's `-via-geom` companion arm — same SQL surface as
        // `GeomBlob`.
        | RetShape::TopoGeometryViaGeom
        | RetShape::OptionBlob
        | RetShape::OptionGeomBlob
        | RetShape::OptionRasterBlob
        | RetShape::OptionTopologyBlob
        | RetShape::FirstGeomBlob
        | RetShape::FirstRasterBlob
        | RetShape::FirstTopologyBlob
        // BboxBlob — WKB polygon (matches WacPlug's
        // `pg_ctor::st_make_envelope`).
        | RetShape::BboxBlob
        // WitValueRecord returns — Binary carrying the WTV-framed
        // CBOR envelope. Bridge forwards provider bytes verbatim.
        | RetShape::WitValueRecord { .. }
        | RetShape::OptionWitValueRecord { .. }
        | RetShape::FirstWitValueRecord { .. } => "ftypes::LogicalType::Binary".to_string(),
        RetShape::BoolInt | RetShape::OptionBoolInt => {
            "ftypes::LogicalType::Boolean".to_string()
        }
        // `result<_, E>` unit-OK — SQL NULL. Advertise as Binary
        // so the neutral logical type marshals a NULL cleanly.
        RetShape::Unit => "ftypes::LogicalType::Binary".to_string(),
        // #823 W4b: JSON-text returns (matches WacPlug's Utf8 surface).
        RetShape::ListListU8
        | RetShape::ListBool
        | RetShape::JsonText { .. } => "ftypes::LogicalType::Utf8".to_string(),
        // #823: WIT enum return — advertised as Int64 (the ordinal).
        // Matches WacPlug's convention.
        RetShape::Enum { .. } | RetShape::OptionEnum { .. } => {
            "ftypes::LogicalType::Int64".to_string()
        }
        // #823: tuple-pick — advertised type matches the projected
        // element. Mirrors WacPlug's `dispatch::TuplePick` logical
        // type mapping.
        RetShape::TuplePick { elem, .. } => match elem {
            ListPrimElem::F64 | ListPrimElem::F32 => "ftypes::LogicalType::Float64".to_string(),
            ListPrimElem::S32
            | ListPrimElem::S64
            | ListPrimElem::U32
            | ListPrimElem::U64
            | ListPrimElem::U8 => "ftypes::LogicalType::Int64".to_string(),
            ListPrimElem::Bool => "ftypes::LogicalType::Boolean".to_string(),
            ListPrimElem::String => "ftypes::LogicalType::Utf8".to_string(),
        },
        _ => "ftypes::LogicalType::Binary".to_string(),
    }
}

fn ret_to_logicaltype_lit_stub(r: &RetShape) -> String {
    let _ = r;
    "ftypes::LogicalType::Binary".to_string()
}

fn emit_scalar_arm_body(
    sql: &str,
    invoke_name: &str,
    params: &[ParamShape],
    ret: &RetShape,
    _fallible: bool,
) -> String {
    let mut lines = String::new();
    let mut arg_ident: Vec<String> = Vec::new();
    for (i, p) in params.iter().enumerate() {
        let ident = format!("a{}", i);
        let decode = match p {
            ParamShape::Blob
            | ParamShape::Geom
            | ParamShape::Geog
            | ParamShape::Raster
            | ParamShape::Topology
            // WitValueRecord param — SQL surface is Binary carrying
            // the WTV-framed record envelope. Provider dispatcher
            // ciborium-decodes the payload straight into the
            // wit-bindgen record type (upstream derives serde
            // via `additional_derives`). Wire shape = Blob.
            | ParamShape::WitValueRecord { .. } => format!(
                "                let {ident} = CborValue::Bytes(dfv_blob(&args, {i}, \"{sql}\")?);\n"
            ),
            // Option<T> param the codegen elects to default. Mirrors
            // the WacPlug `emit_scalar_arm_body` convention: no SQL
            // arg is decoded, we just forward `CborValue::Null` so
            // the payload_args order stays aligned with the WIT
            // param position. WacPlug's `param_to_logicaltype_lit`
            // still emits a Utf8 SQL slot for OptionNone; the
            // per-arm decode ignores it.
            ParamShape::OptionNone => format!(
                "                let {ident} = CborValue::Null;\n"
            ),
            ParamShape::F64 => format!(
                "                let {ident} = CborValue::Float(dfv_f64(&args, {i}, \"{sql}\")?);\n"
            ),
            ParamShape::S32 | ParamShape::S64 => format!(
                "                let {ident} = CborValue::Int(dfv_i64(&args, {i}, \"{sql}\")?);\n"
            ),
            ParamShape::U32 | ParamShape::U64 => format!(
                "                let {ident} = CborValue::Uint(dfv_i64(&args, {i}, \"{sql}\")? as u64);\n"
            ),
            ParamShape::Bool => format!(
                "                let {ident} = CborValue::Bool(dfv_bool(&args, {i}, \"{sql}\")?);\n"
            ),
            ParamShape::Text => format!(
                "                let {ident} = CborValue::Text(dfv_text(&args, {i}, \"{sql}\")?);\n"
            ),
            // W3.3 (#543): WIT enum arg. The provider dispatcher already
            // decodes the case ordinal from `CborValue::Uint` (see e.g.
            // `as_pixel_type` in postgis_raster.rs) — dynlink mode
            // forwards the SQL integer as-is; the provider crate handles
            // WIT-enum construction.
            ParamShape::Enum { .. } => format!(
                "                let {ident} = CborValue::Uint(dfv_i64(&args, {i}, \"{sql}\")? as u64);\n"
            ),
            // W2 (#542): `list<primitive>` arg. SQL surface is a JSON-array
            // TEXT literal (e.g. `'[1.0, 2.0, 3.0]'`). Parse locally into
            // a `Vec<T>` and forward as `CborValue::List` — the provider
            // dispatcher accepts CborValue::List of primitives (see e.g.
            // `st-pixels-of-values` in postgis_raster.rs). No provider-side
            // change required.
            ParamShape::ListPrim(elem) => format!(
                "                let {ident} = {{\n\
                                     let text = dfv_text(&args, {i}, \"{sql}\")?;\n\
                                     let v: Vec<{rust_ty}> = serde_json::from_str(&text)\n\
                                         .map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!(\"{sql}: arg {i} must be JSON array of {suffix} ({{}})\", e)))?;\n\
                                     CborValue::List(v.into_iter().map(|x| {cbor_wrap}).collect())\n\
                                 }};\n",
                rust_ty = list_prim_rust_ty(*elem),
                suffix = elem.helper_suffix(),
                cbor_wrap = list_prim_cbor_wrap(*elem),
                ident = ident,
                i = i,
                sql = sql,
            ),
            // #674: `list<list<u8>>` — batched WKB blobs surfaced by
            // postgis's `st_*_batch` family. SQL passes JSON text
            // matching `Vec<Vec<u8>>` (nested arrays of byte
            // integers); parse locally and forward as
            // `CborValue::List(Vec<CborValue::Bytes>)` — the
            // symmetric wire shape for the provider dispatcher.
            ParamShape::ListListU8 => format!(
                "                let {ident} = {{\n\
                                     let text = dfv_text(&args, {i}, \"{sql}\")?;\n\
                                     let v: Vec<Vec<u8>> = serde_json::from_str(&text)\n\
                                         .map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!(\"{sql}: arg {i} must be JSON list<list<u8>> ({{}})\", e)))?;\n\
                                     CborValue::List(v.into_iter().map(CborValue::Bytes).collect())\n\
                                 }};\n",
                ident = ident,
                i = i,
                sql = sql,
            ),
            // #823: `list<borrow<geometry>>` — same wire shape as
            // `ListListU8` (nested byte arrays), but the SQL name is
            // documented as a list of WKB blobs. Parse locally into
            // `Vec<Vec<u8>>` and forward as `CborValue::List<Bytes>`;
            // the provider dispatcher unpacks via
            // `decode_geometry_list` (see e.g. `st-as-geobuf` in
            // postgis-wasm-provider). WacPlug supports true variadic
            // args here — the dynlink bridge takes the single-slot
            // JSON-text path only (the SQL rewriter is expected to
            // collapse variadic geometry lists into one JSON literal
            // before the arm sees it).
            ParamShape::ListGeom => format!(
                "                let {ident} = {{\n\
                                     let text = dfv_text(&args, {i}, \"{sql}\")?;\n\
                                     let v: Vec<Vec<u8>> = serde_json::from_str(&text)\n\
                                         .map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!(\"{sql}: arg {i} must be JSON list<geometry> as list<list<u8>> ({{}})\", e)))?;\n\
                                     CborValue::List(v.into_iter().map(CborValue::Bytes).collect())\n\
                                 }};\n",
                ident = ident,
                i = i,
                sql = sql,
            ),
            // #823: `list<record>` — SQL passes a JSON-array TEXT
            // literal of record-shaped objects (matches WacPlug's
            // `parse_json_list_record_<snake>` helper). Parse locally
            // as `Vec<serde_json::Value>`, ciborium-encode each
            // element into its own byte buffer, and forward as
            // `CborValue::List(Vec<CborValue::Bytes>)`. The provider
            // dispatcher unwraps each `Bytes` payload via
            // `ciborium::from_reader::<UPSTREAM, _>` (upstream WIT
            // record type derives serde::Deserialize under
            // `additional_derives`) and reassembles the
            // `Vec<UPSTREAM>` for the WIT call — mirroring the
            // scalar `WitValueRecord` param wire one list level up.
            ParamShape::ListRecord { .. } => format!(
                "                let {ident} = {{\n\
                                     let text = dfv_text(&args, {i}, \"{sql}\")?;\n\
                                     let values: Vec<serde_json::Value> = serde_json::from_str(&text)\n\
                                         .map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!(\"{sql}: arg {i} must be JSON list<record> ({{}})\", e)))?;\n\
                                     let mut items: Vec<CborValue> = Vec::with_capacity(values.len());\n\
                                     for val in values {{\n\
                                         let mut buf: Vec<u8> = Vec::new();\n\
                                         ciborium::into_writer(&val, &mut buf)\n\
                                             .map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!(\"{sql}: arg {i} ciborium encode record ({{}})\", e)))?;\n\
                                         items.push(CborValue::Bytes(buf));\n\
                                     }}\n\
                                     CborValue::List(items)\n\
                                 }};\n",
                ident = ident,
                i = i,
                sql = sql,
            ),
            // #823: `list<list<primitive>>` (non-u8 element). SQL
            // passes a JSON-array TEXT literal matching `Vec<Vec<T>>`
            // (e.g. `'[[1.0, 2.0], [3.0, 4.0]]'` for
            // `list<list<f64>>`). Parse locally into `Vec<Vec<T>>`
            // and forward as
            // `CborValue::List(Vec<CborValue::List(Vec<CborValue::<Prim>>)>)`.
            // Symmetric with `ListListU8` one primitive layer up;
            // per-element `CborValue` wrap follows the `ListPrim`
            // convention (see `list_prim_cbor_wrap`).
            ParamShape::ListListPrim(elem) => format!(
                "                let {ident} = {{\n\
                                     let text = dfv_text(&args, {i}, \"{sql}\")?;\n\
                                     let v: Vec<Vec<{rust_ty}>> = serde_json::from_str(&text)\n\
                                         .map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!(\"{sql}: arg {i} must be JSON list<list<{suffix}>> ({{}})\", e)))?;\n\
                                     CborValue::List(v.into_iter().map(|inner| CborValue::List(inner.into_iter().map(|x| {cbor_wrap}).collect())).collect())\n\
                                 }};\n",
                rust_ty = list_prim_rust_ty(*elem),
                suffix = elem.helper_suffix(),
                cbor_wrap = list_prim_cbor_wrap(*elem),
                ident = ident,
                i = i,
                sql = sql,
            ),
            _ => format!(
                "                let {ident} = CborValue::Null; // unsupported shape\n"
            ),
        };
        lines.push_str(&decode);
        arg_ident.push(ident);
    }
    lines.push_str(&format!(
        "                let payload_args = alloc::vec![{}];\n",
        arg_ident.join(", ")
    ));
    lines.push_str(&format!(
        "                let resp = call(\"{invoke_name}\", payload_args)?;\n"
    ));
    // Wrap response into ScalarValue.
    let wrap = match ret {
        RetShape::Text => "                match resp { ResponseValue::Text(s) => Ok(ftypes::ScalarValue::Utf8(s)), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        RetShape::Real => "                match resp { ResponseValue::Float(f) => Ok(ftypes::ScalarValue::Float64(f)), ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Float64(i as f64)), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        RetShape::Int => "                match resp { ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Int64(i)), ResponseValue::Uint(u) => Ok(ftypes::ScalarValue::Int64(u as i64)), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        RetShape::Blob
        | RetShape::GeomBlob
        | RetShape::RasterBlob
        | RetShape::TopologyBlob
        // #823: `TopoGeometryViaGeom` — provider's `-via-geom` companion
        // arm emits `CborValue::Bytes` for the assembled MULTI* WKB.
        // Same decode path as `GeomBlob`.
        | RetShape::TopoGeometryViaGeom => "                match resp { ResponseValue::Bytes(b) => Ok(ftypes::ScalarValue::Binary(b)), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        // #823: `IsValidDetailText` — provider emits
        // `CborValue::List([Bool, Text-or-Null, Text-or-Null])`. Bridge
        // renders the PostgreSQL composite-type text form
        // `(valid,"reason","location")` byte-for-byte matching WacPlug's
        // `emit_lib.rs` `RetShape::IsValidDetailText` arm.
        RetShape::IsValidDetailText => "                match resp {\n                    ResponseValue::List(items) if items.len() == 3 => {\n                        let valid = match &items[0] {\n                            ResponseValue::Bool(b) => *b,\n                            other => return Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"is-valid-detail[0] expected Bool, got {:?}\", other))),\n                        };\n                        let reason = match &items[1] {\n                            ResponseValue::Text(s) => s.clone(),\n                            ResponseValue::Null => String::new(),\n                            other => return Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"is-valid-detail[1] expected Text or Null, got {:?}\", other))),\n                        };\n                        let loc = match &items[2] {\n                            ResponseValue::Text(s) => s.clone(),\n                            ResponseValue::Null => String::new(),\n                            other => return Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"is-valid-detail[2] expected Text or Null, got {:?}\", other))),\n                        };\n                        Ok(ftypes::ScalarValue::Utf8(alloc::format!(\"({},\\\"{}\\\",\\\"{}\\\")\", valid, reason, loc)))\n                    }\n                    other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"is-valid-detail: expected 3-element List, got {:?}\", other))),\n                }",
        RetShape::BoolInt => "                match resp { ResponseValue::Bool(b) => Ok(ftypes::ScalarValue::Boolean(b)), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        // Option<T> — provider emits `CborValue::T` on Some or
        // `CborValue::Null` on None; forward None as SQL NULL.
        RetShape::OptionText => "                match resp { ResponseValue::Text(s) => Ok(ftypes::ScalarValue::Utf8(s)), ResponseValue::Null => Ok(ftypes::ScalarValue::Null), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        RetShape::OptionReal => "                match resp { ResponseValue::Float(f) => Ok(ftypes::ScalarValue::Float64(f)), ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Float64(i as f64)), ResponseValue::Null => Ok(ftypes::ScalarValue::Null), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        RetShape::OptionInt => "                match resp { ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Int64(i)), ResponseValue::Uint(u) => Ok(ftypes::ScalarValue::Int64(u as i64)), ResponseValue::Null => Ok(ftypes::ScalarValue::Null), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        // #823: `option<bool>` — provider emits `CborValue::Bool(v)` on
        // Some, `CborValue::Null` on None. Wrap to Boolean / Null. Symmetric
        // with `BoolInt` above but admits the None side.
        RetShape::OptionBoolInt => "                match resp { ResponseValue::Bool(b) => Ok(ftypes::ScalarValue::Boolean(b)), ResponseValue::Null => Ok(ftypes::ScalarValue::Null), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        // Option<blob-shaped> — same Bytes/Null two-way as Blob but
        // admits Null on the None side. Covers OptionBlob / raster /
        // topology / geom, plus the First*Blob dispatch that emits
        // Bytes (first element) or Null (empty list).
        RetShape::OptionBlob
        | RetShape::OptionGeomBlob
        | RetShape::OptionRasterBlob
        | RetShape::OptionTopologyBlob
        | RetShape::FirstGeomBlob
        | RetShape::FirstRasterBlob
        | RetShape::FirstTopologyBlob => "                match resp { ResponseValue::Bytes(b) => Ok(ftypes::ScalarValue::Binary(b)), ResponseValue::Null => Ok(ftypes::ScalarValue::Null), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        // list<T>-then-first primitive projections — provider does
        // the `.first()` conversion and emits `CborValue::<T>` on
        // a non-empty list or `CborValue::Null` on empty. Mirrors
        // the Option<T> wrap arms; Int/Uint are both accepted for
        // FirstInt so U32/U64 element lists lower cleanly.
        RetShape::FirstInt => "                match resp { ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Int64(i)), ResponseValue::Uint(u) => Ok(ftypes::ScalarValue::Int64(u as i64)), ResponseValue::Null => Ok(ftypes::ScalarValue::Null), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        RetShape::FirstReal => "                match resp { ResponseValue::Float(f) => Ok(ftypes::ScalarValue::Float64(f)), ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Float64(i as f64)), ResponseValue::Null => Ok(ftypes::ScalarValue::Null), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        RetShape::FirstText => "                match resp { ResponseValue::Text(s) => Ok(ftypes::ScalarValue::Utf8(s)), ResponseValue::Null => Ok(ftypes::ScalarValue::Null), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        // `result<_, E>` unit-OK — WIT return is `()`. SQL surface
        // is NULL regardless of what the provider dispatcher put
        // in the response payload (some mutator arms emit
        // `CborValue::Null`; others — notably topology mutators
        // — return the modified topology bytes). Discard the
        // payload uniformly.
        RetShape::Unit => "                let _ = resp; Ok(ftypes::ScalarValue::Null)",
        // #823 W4b: `BboxBlob` — provider emits `CborValue::List([Float; 4])`
        // (xmin, ymin, xmax, ymax); decode 4 floats and re-wrap as WKB
        // POLYGON matching WacPlug's `pg_ctor::st_make_envelope`.
        RetShape::BboxBlob => "                let bb = response_bbox_floats(&resp, \"bbox\")?;\n                Ok(ftypes::ScalarValue::Binary(bbox_to_wkb_polygon(bb[0], bb[1], bb[2], bb[3])))",
        // #823 W4b: `ListListU8` / `ListBool` — provider emits
        // `CborValue::List(Vec<Bytes>)` or `CborValue::List(Vec<Bool>)`;
        // render as JSON text (symmetric with WacPlug's
        // `serde_json::to_string(&Vec<Vec<u8>>)` / `Vec<bool>` output).
        RetShape::ListListU8 | RetShape::ListBool => "                let s = response_to_json(&resp, \"list\")?;\n                Ok(ftypes::ScalarValue::Utf8(s))",
        // #823 W4b: `JsonText { ... }` — provider emits `CborValue::List(...)`
        // (nested list of primitives / bytes). Render as JSON text via
        // `response_to_json`. Record-kind sub-variants render as
        // positional arrays here rather than named-field objects
        // (WacPlug's default via serde's record derive) — best-effort
        // for the non-record kinds (ListListPrim, ListTuplePrim,
        // TuplePrim, OptionListPrim, OptionListTuplePrim,
        // OptionTuplePrim, OptionTuplePrimOrOptPrim); record-shaped
        // JsonRetKinds (OptionListPrimRecord / OptionListRecord /
        // ListTupleGeomF64) surface as positional arrays instead of
        // objects (accepted trade-off for the W4b landing).
        RetShape::JsonText { .. } => "                match resp {\n                    ResponseValue::Null => Ok(ftypes::ScalarValue::Null),\n                    other => {\n                        let s = response_to_json(&other, \"json\")?;\n                        Ok(ftypes::ScalarValue::Utf8(s))\n                    }\n                }",
        // #823: WIT enum return — provider emits `CborValue::Uint(ordinal)`.
        // Wrap as SQL Int64. `OptionEnum` accepts `CborValue::Null`
        // for the None case. Both variants ship the same wire form
        // as an Int-typed arm; the classifier keeps the shapes
        // distinct so downstream tools (e.g. metadata dumpers) can
        // recover the enum's case names later.
        RetShape::Enum { .. } => "                match resp { ResponseValue::Uint(u) => Ok(ftypes::ScalarValue::Int64(u as i64)), ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Int64(i)), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"enum-return: unexpected {:?}\", other))) }",
        RetShape::OptionEnum { .. } => "                match resp { ResponseValue::Uint(u) => Ok(ftypes::ScalarValue::Int64(u as i64)), ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Int64(i)), ResponseValue::Null => Ok(ftypes::ScalarValue::Null), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"option-enum-return: unexpected {:?}\", other))) }",
        // WitValueRecord returns — provider ciborium-encodes the
        // WIT-generated record and wraps in `CborValue::Bytes`. We
        // forward the WTV-framed payload verbatim as SQL Binary. The
        // Null path covers `Option<R>` and `list<R>`-then-first empty
        // cases (the provider emits `CborValue::Null` there). Same
        // wire shape as the aggregate finalize wrap for
        // `WitValueRecord*` — see the sibling arm above (~line 2509).
        RetShape::WitValueRecord { .. }
        | RetShape::OptionWitValueRecord { .. }
        | RetShape::FirstWitValueRecord { .. } => "                match resp { ResponseValue::Bytes(b) => Ok(ftypes::ScalarValue::Binary(b)), ResponseValue::Null => Ok(ftypes::ScalarValue::Null), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"wit-value-record: unexpected {:?}\", other))) }",
        // #823: tuple-pick — provider emits `CborValue::List` of
        // primitives (the tuple's positional payload); pick position
        // `index` and wrap per element type. Matches WacPlug's
        // `dispatch::TuplePick` semantics — the only difference is
        // that we decode a `ResponseValue::List` sequence rather than
        // dereferencing a Rust tuple field.
        RetShape::TuplePick { index, elem } => {
            let idx = *index;
            let wrap_expr = match elem {
                ListPrimElem::F64 | ListPrimElem::F32 => {
                    "match __x {\n                            ResponseValue::Float(f) => Ok(ftypes::ScalarValue::Float64(*f)),\n                            ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Float64(*i as f64)),\n                            ResponseValue::Uint(u) => Ok(ftypes::ScalarValue::Float64(*u as f64)),\n                            other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"tuple-pick[{}]: unexpected {:?}\", __idx, other))),\n                        }"
                }
                ListPrimElem::S32
                | ListPrimElem::S64
                | ListPrimElem::U32
                | ListPrimElem::U64
                | ListPrimElem::U8 => {
                    "match __x {\n                            ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Int64(*i)),\n                            ResponseValue::Uint(u) => Ok(ftypes::ScalarValue::Int64(*u as i64)),\n                            other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"tuple-pick[{}]: unexpected {:?}\", __idx, other))),\n                        }"
                }
                ListPrimElem::Bool => {
                    "match __x {\n                            ResponseValue::Bool(b) => Ok(ftypes::ScalarValue::Boolean(*b)),\n                            other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"tuple-pick[{}]: unexpected {:?}\", __idx, other))),\n                        }"
                }
                ListPrimElem::String => {
                    "match __x {\n                            ResponseValue::Text(t) => Ok(ftypes::ScalarValue::Utf8(t.clone())),\n                            other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"tuple-pick[{}]: unexpected {:?}\", __idx, other))),\n                        }"
                }
            };
            let body = format!(
                "                let __idx: usize = {idx};\n\
                 \x20               match &resp {{\n\
                 \x20                   ResponseValue::List(items) => {{\n\
                 \x20                       let __x = items.get(__idx).ok_or_else(|| ftypes::FunctionError::ExecutionError(alloc::format!(\"tuple-pick: index {{}} out of range (len={{}})\", __idx, items.len())))?;\n\
                 \x20                       {wrap_expr}\n\
                 \x20                   }},\n\
                 \x20                   ResponseValue::Null => Ok(ftypes::ScalarValue::Null),\n\
                 \x20                   other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"tuple-pick: expected List, got {{:?}}\", other))),\n\
                 \x20               }}",
                idx = idx,
                wrap_expr = wrap_expr,
            );
            lines.push_str(&body);
            return lines;
        }
        _ => "                Err(ftypes::FunctionError::ExecutionError(\"unsupported return shape\".to_string()))",
    };
    lines.push_str(wrap);
    lines
}

/// Rust element type used by the emitted `serde_json::from_str::<Vec<_>>`
/// invocation for a `ParamShape::ListPrim(elem)` arg. Mirrors the
/// WacPlug-mode helper set — see `emit_lib::JSON_LIST_PRIM_HELPERS`.
///
/// F32 collapses to f64 on the wire because `CborValue::Float` is
/// double-precision; there's no lossy narrowing since the provider
/// dispatcher unwraps `CborValue::Float(f)` back to whichever concrete
/// f32/f64 element type its typed WIT call expects.
fn list_prim_rust_ty(elem: ListPrimElem) -> &'static str {
    match elem {
        ListPrimElem::F64 | ListPrimElem::F32 => "f64",
        ListPrimElem::S32 => "i32",
        ListPrimElem::S64 => "i64",
        ListPrimElem::U32 => "u32",
        ListPrimElem::U64 => "u64",
        ListPrimElem::U8 => "u8",
        ListPrimElem::Bool => "bool",
        ListPrimElem::String => "String",
    }
}

/// Per-element wrap expression the emitted arm applies while lowering a
/// parsed `Vec<T>` into `CborValue::List(Vec<CborValue>)`. `x` is the
/// closure param of the surrounding `into_iter().map(|x| ...)`.
fn list_prim_cbor_wrap(elem: ListPrimElem) -> &'static str {
    match elem {
        ListPrimElem::F64 | ListPrimElem::F32 => "CborValue::Float(x)",
        ListPrimElem::S32 => "CborValue::Int(x as i64)",
        ListPrimElem::S64 => "CborValue::Int(x)",
        ListPrimElem::U32 => "CborValue::Uint(x as u64)",
        ListPrimElem::U64 => "CborValue::Uint(x)",
        ListPrimElem::U8 => "CborValue::Uint(x as u64)",
        ListPrimElem::Bool => "CborValue::Bool(x)",
        ListPrimElem::String => "CborValue::Text(x)",
    }
}

// ============================================================
// Aggregate registry — real dispatch through the dynlink bridge.
// ============================================================
//
// Structure mirrors `emit_lib.rs::build_aggregate_registry_impl`,
// but the finalize body routes through the CBOR envelope instead of
// calling the upstream `pg_agg` WIT function directly. Per-handle
// state (a `Vec<Vec<u8>>` of WKB payloads pushed by `accumulate`) is
// shipped verbatim as `CborValue::List(<Bytes...>)` to the resident
// provider; the provider decodes each element via
// `marshal::decode_geometry_list` and calls the upstream aggregator.
//
// Wire scope: `AccKind::Geom` aggregates with `extra_args.is_empty()`
// and a supported return shape (`GeomBlob` / `OptionGeomBlob` /
// `BboxBlob` / `Bbox3dWkbLineZ` / `Real` / `OptionReal` / `Int` /
// `OptionInt` / `Text` / `OptionText`). Everything else surfaces in
// `list_functions` so the SQL layer sees it, but `finalize` returns
// `UnknownFunction` for out-of-scope arms (Raster, record-typed
// mobilitydb aggregates, etc.). The stubs can be lifted in a
// follow-up once the marshal helpers land here.

fn build_aggregate_registry_impl_dynlink(
    agg_entries: &[AggregateEntry],
) -> (String, usize, usize) {
    if agg_entries.is_empty() {
        return (String::new(), 0, 0);
    }

    // Assign one arm_idx per canonical sql_name; aliases reuse the
    // canonical's arm_idx (same pattern as emit_lib).
    let mut arm_for: BTreeMap<String, usize> = BTreeMap::new();
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

    let mut wired = 0usize;
    let mut stubbed = 0usize;
    let mut metas_block = String::new();
    let mut return_arms = String::new();
    let mut create_arms = String::new();
    let mut finalize_arms = String::new();
    let mut seen_meta: BTreeSet<String> = BTreeSet::new();
    let mut seen_create: BTreeSet<String> = BTreeSet::new();
    let mut emitted_finalize: BTreeSet<usize> = BTreeSet::new();

    for entry in agg_entries {
        let escaped = entry.sql_name.replace('"', "\\\"");
        let arm_idx = *arm_for.get(&entry.sql_name).unwrap();

        // Safety dedupe: two extensions exposing the same canonical
        // name emit meta once (matches emit_lib.rs).
        if !seen_meta.insert(entry.sql_name.clone()) {
            continue;
        }

        let dynlink_wired = is_dynlink_wired_aggregate(&entry.shape);
        if dynlink_wired {
            wired += 1;
        } else {
            stubbed += 1;
        }

        // ---- metadata (canonical only — aliases ride the
        // canonical's `aliases:` literal). ----
        let mut sig_block = String::from("alloc::vec![alloc::vec![ftypes::LogicalType::Binary");
        for p in &entry.shape.extra_args {
            sig_block.push_str(", ");
            sig_block.push_str(&param_to_logicaltype_lit(p));
        }
        sig_block.push_str("]]");
        let cfg_indices: String = (0..entry.shape.extra_args.len())
            .map(|j| format!("{}u32, ", j + 1))
            .collect();
        let accepts_config = !entry.shape.extra_args.is_empty();
        let aliases_lit = aliases_literal_dynlink(&entry.aliases);
        metas_block.push_str(&format!(
            "        ftypes::AggregateFunctionMeta {{\n\
             \x20           name: \"{escaped}\".to_string(),\n\
             \x20           aliases: {aliases_lit},\n\
             \x20           param_types: {sig_block},\n\
             \x20           supports_grouped: true,\n\
             \x20           supports_partial: true,\n\
             \x20           is_order_sensitive: false,\n\
             \x20           accepts_config: {accepts_config},\n\
             \x20           config_arg_indices: alloc::vec![{cfg_indices}],\n\
             \x20       }},\n",
        ));

        // ---- return_type + create arms (canonical + each alias). ----
        let ret_logical = ret_to_logicaltype_lit(&entry.shape.ret);
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
            let body = if dynlink_wired {
                // Provider dispatchers match on kebab-case WIT method names
                // (see e.g. `"st-extent"` in postgis-wasm/crates/provider/src/
                // dispatch/postgis_core.rs). Mirror the scalar arm's fix
                // (commit 2441545) and route on wit_func rather than sql_name.
                //
                // #823 Agent #907 Blocker 4: distinct method-name namespace
                // for the aggregate finalize wire (blob-list) vs. the
                // scalar arm's structured input. The provider ships a
                // separate arm keyed on the `-agg` suffix which decodes
                // `List<Bytes>` per-row payloads via `marshal::decode_*`
                // and returns `Bytes`. Every mobilitydb aggregate finalize
                // call routes through this suffixed name so the existing
                // scalar arm (which may share the base kebab name — e.g.
                // `tint-temporal-avg` at
                // `mobilitydb-wasm-provider::dispatch/mobilitydb_analytics.rs`)
                // keeps working for structured callers.
                let invoke_name =
                    format!("{}-agg", entry.shape.wit_func.replace('_', "-"));
                emit_dynlink_agg_finalize_body(
                    &invoke_name,
                    &entry.shape.ret,
                    &entry.shape.extra_args,
                )
            } else {
                format!(
                    "                let _ = st;\n\
                     \x20               Err(ftypes::FunctionError::UnknownFunction(alloc::format!(\n\
                     \x20                   \"{sql}: aggregate shape not yet wired in dynlink mode\"\n\
                     \x20               )))\n",
                    sql = entry.sql_name.replace('"', "\\\""),
                )
            };
            finalize_arms.push_str(&format!(
                "            {arm_idx}usize => {{\n{body}            }}\n",
            ));
        }
    }

    let impl_str = format!(
        r##"// ─── Dynlink aggregate accumulator state ───
//
// Per-handle state (arm_idx + Vec<Vec<u8>> of accumulated WKB
// payloads + Vec<String> of constant config args). Handle allocation
// is a monotonic u64. `finalize` consumes the handle so subsequent
// calls surface as `ExecutionError`. Wire discipline matches the
// provider's `postgis_core::st-*-aggregate` arms:
// `args = [CborValue::List(<CborValue::Bytes(<wkb>)>*)]`.
//
// The bridge is `#![no_std]` so `std::thread_local!` isn't
// available. Under wasm32-wasip2 each component instance runs on
// exactly one wasm thread (wit-bindgen guest exports are synchronous
// and the bridge never spawns), so a plain `static SyncRefCell<...>`
// with an `unsafe impl Sync` facade is race-free by construction.
use alloc::collections::BTreeMap;
use core::cell::RefCell;

/// Single-threaded-wasm facade: expose `RefCell<T>` as a `Sync`
/// value so it can live in a `static`. Every borrow goes through
/// `RefCell`'s runtime borrow checker, so double-mut-borrow bugs
/// panic instead of racing. Not sound for multi-threaded targets.
struct SyncRefCell<T>(RefCell<T>);
unsafe impl<T> Sync for SyncRefCell<T> {{}}

#[derive(Clone)]
struct AccState {{
    arm_idx: usize,
    blobs: Vec<Vec<u8>>,
    extras: Vec<String>,
}}

static ACCUMULATORS: SyncRefCell<BTreeMap<u64, AccState>> =
    SyncRefCell(RefCell::new(BTreeMap::new()));
static NEXT_ACC_HANDLE: SyncRefCell<u64> = SyncRefCell(RefCell::new(1));

fn alloc_accumulator(arm_idx: usize, extras: Vec<String>) -> u64 {{
    let h = {{
        let mut next = NEXT_ACC_HANDLE.0.borrow_mut();
        let v = *next;
        *next += 1;
        v
    }};
    ACCUMULATORS.0.borrow_mut().insert(h, AccState {{
        arm_idx,
        blobs: Vec::new(),
        extras,
    }});
    h
}}

impl aggregate_function_registry::Guest for Component {{
    fn list_functions() -> Vec<ftypes::AggregateFunctionMeta> {{
        alloc::vec![
{metas_block}        ]
    }}

    fn return_type(
        name: String,
        _input_types: Vec<ftypes::LogicalType>,
    ) -> Result<ftypes::LogicalType, ftypes::FunctionError> {{
        match name.as_str() {{
{return_arms}            other => Err(ftypes::FunctionError::UnknownFunction(other.to_string())),
        }}
    }}

    fn create_accumulator(name: String) -> Result<u64, ftypes::FunctionError> {{
        let arm = match name.as_str() {{
{create_arms}            other => return Err(ftypes::FunctionError::UnknownFunction(other.to_string())),
        }};
        Ok(alloc_accumulator(arm, Vec::new()))
    }}

    fn create_accumulator_with_config(
        name: String,
        config: String,
    ) -> Result<u64, ftypes::FunctionError> {{
        let arm = match name.as_str() {{
{create_arms}            other => return Err(ftypes::FunctionError::UnknownFunction(other.to_string())),
        }};
        Ok(alloc_accumulator(arm, alloc::vec![config]))
    }}

    fn create_accumulator_with_configs(
        name: String,
        configs: Vec<String>,
    ) -> Result<u64, ftypes::FunctionError> {{
        let arm = match name.as_str() {{
{create_arms}            other => return Err(ftypes::FunctionError::UnknownFunction(other.to_string())),
        }};
        Ok(alloc_accumulator(arm, configs))
    }}

    fn accumulate(
        handle: u64,
        value: ftypes::ScalarValue,
    ) -> Result<(), ftypes::FunctionError> {{
        let mut g = ACCUMULATORS.0.borrow_mut();
        let st = g.get_mut(&handle).ok_or_else(|| {{
            ftypes::FunctionError::ExecutionError(alloc::format!(
                "no accumulator at handle {{}}", handle
            ))
        }})?;
        // SQL aggregate semantics: NULL contributions are skipped.
        if matches!(value, ftypes::ScalarValue::Null) {{
            return Ok(());
        }}
        let bytes = match value {{
            ftypes::ScalarValue::Binary(b) => b,
            ftypes::ScalarValue::Utf8(s) => s.into_bytes(),
            _ => return Err(ftypes::FunctionError::ExecutionError(
                "aggregate streaming arg must be BINARY".to_string()
            )),
        }};
        st.blobs.push(bytes);
        Ok(())
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
        let mut g = ACCUMULATORS.0.borrow_mut();
        let src = g.remove(&source).ok_or_else(|| {{
            ftypes::FunctionError::ExecutionError(alloc::format!(
                "merge: source handle {{}} not found", source
            ))
        }})?;
        let tgt = g.get_mut(&target).ok_or_else(|| {{
            ftypes::FunctionError::ExecutionError(alloc::format!(
                "merge: target handle {{}} not found", target
            ))
        }})?;
        if tgt.arm_idx != src.arm_idx {{
            return Err(ftypes::FunctionError::ExecutionError(
                "merge: target and source must come from the same aggregate".to_string()
            ));
        }}
        tgt.blobs.extend(src.blobs);
        Ok(())
    }}

    fn finalize(handle: u64) -> Result<ftypes::ScalarValue, ftypes::FunctionError> {{
        // finalize consumes the accumulator — drop the borrow before
        // invoking through the linker so re-entrant provider calls
        // that touch our own registry don't hit a runtime borrow panic.
        let st = ACCUMULATORS.0.borrow_mut().remove(&handle).ok_or_else(|| {{
            ftypes::FunctionError::ExecutionError(alloc::format!(
                "finalize: no accumulator at handle {{}}", handle
            ))
        }})?;
        match st.arm_idx {{
{finalize_arms}            other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(
                "finalize: unknown aggregate arm {{}}", other
            ))),
        }}
    }}

    fn reset(handle: u64) {{
        let mut g = ACCUMULATORS.0.borrow_mut();
        if let Some(st) = g.get_mut(&handle) {{
            st.blobs.clear();
        }}
    }}

    fn destroy_accumulator(handle: u64) {{
        ACCUMULATORS.0.borrow_mut().remove(&handle);
    }}
}}

"##,
        metas_block = metas_block,
        return_arms = return_arms,
        create_arms = create_arms,
        finalize_arms = finalize_arms,
    );

    (impl_str, wired, stubbed)
}

/// True iff this aggregate's shape maps cleanly to the dynlink wire:
/// blob streaming state + no configs + a return shape the finalize
/// wrapper knows how to unpack.
///
/// The dynlink accumulator is always `Vec<Vec<u8>>` of the SQL-side
/// binary payloads (the SQL `param_types` for every aggregate is
/// `binary`, both for postgis's WKB blobs and for mobilitydb's
/// CBOR-encoded temporal records). The finalize call ships
/// `CborValue::List<Bytes>` to the provider regardless of the
/// upstream WIT input type — the provider owns the decode. So the
/// wired set is gated only on the return-shape wrap the emit
/// currently generates.
fn is_dynlink_wired_aggregate(shape: &interface_db::AggregateShape) -> bool {
    // Extras are admitted for primitive scalar shapes only — the
    // finalize body parses each config string into its CBOR wire
    // form. Non-primitive extras (record, list, geometry) can't be
    // shipped through the config-string ABI without additional
    // marshaling substrate.
    for p in &shape.extra_args {
        if !matches!(
            p,
            ParamShape::F64
                | ParamShape::S32
                | ParamShape::S64
                | ParamShape::U32
                | ParamShape::U64
                | ParamShape::Bool
                | ParamShape::Text
        ) {
            return false;
        }
    }
    let acc_ok = matches!(
        shape.accumulator_kind,
        AccKind::Geom
            // AccKind::Raster admits raster-list aggregates. The
            // finalize body ships the accumulated raster-binary
            // blobs through the same `CborValue::List<Bytes>`
            // envelope as geometry aggregates — the provider's
            // per-arm decoder picks between `decode_geometry` and
            // `decode_raster` by method name.
            | AccKind::Raster
            | AccKind::Record { .. }
            | AccKind::RecordToScalar { .. }
            | AccKind::RecordToTuple { .. }
            | AccKind::RecordToListPrim { .. }
            | AccKind::RecordSetToRecordSet { .. }
    );
    if !acc_ok {
        return false;
    }
    // Raster aggregates that return a record (e.g. `st-summary-stats-agg`)
    // use the same list-of-Bytes accumulator envelope as every other
    // `AccKind::Raster` arm — the finalize wrap for `WitValueRecord`
    // just forwards the provider-side `ResponseValue::Bytes` verbatim
    // to SQL `Binary`. The provider re-serializes the WIT record as
    // CBOR bytes before wrapping in `CborValue::Bytes`; see
    // `postgis-wasm-provider`'s `st-summary-stats-agg` arm. No extra
    // marshaling substrate is required on the emit side beyond the
    // existing WitValueRecord wrap.
    matches!(
        shape.ret,
        RetShape::GeomBlob
            | RetShape::OptionGeomBlob
            | RetShape::Blob
            | RetShape::OptionBlob
            | RetShape::BboxBlob
            | RetShape::Bbox3dWkbLineZ
            | RetShape::Real
            | RetShape::OptionReal
            | RetShape::Int
            | RetShape::OptionInt
            | RetShape::Text
            | RetShape::OptionText
            // Raster-blob returns (raster aggregates). Provider
            // ships the aggregated raster's `as_binary()` as
            // `CborValue::Bytes`; SQL surface is Binary.
            | RetShape::RasterBlob
            | RetShape::OptionRasterBlob
            // list<geometry>-then-first projection: provider emits
            // `CborValue::List<Bytes>` (per-cluster WKB), bridge
            // returns the first element as SQL Binary (or Null on
            // empty). Matches the scalar-side FirstGeomBlob wrap.
            | RetShape::FirstGeomBlob
            | RetShape::FirstRasterBlob
            // Record-shaped returns (mobilitydb temporal aggregators):
            // the provider re-serialises the record as CBOR bytes and
            // we ship them straight through as SQL BINARY.
            | RetShape::WitValueRecord { .. }
            | RetShape::OptionWitValueRecord { .. }
            | RetShape::FirstWitValueRecord { .. }
            // JSON-shaped returns (RecordToTuple / RecordToListPrim):
            // the provider serialises the tuple/list via serde_json
            // and returns a text payload. Bridge forwards as SQL TEXT.
            | RetShape::JsonText { .. }
    )
}

/// Emit the finalize body for a wired aggregate. The accumulator's
/// WKB / raster-binary payloads ride to the provider as
/// `CborValue::List<Bytes>` under `args[0]`; each captured extra
/// config (planner literal, e.g. `st_clusterwithin(geom, 0.5)`'s
/// `0.5`) is parsed from its config-string form into the matching
/// CBOR wire type and appended after the list. The response variant
/// is unwrapped per return shape (Bytes → Binary, List<Float; 4|6> →
/// WKB envelope, List<Bytes>-then-first → Binary for
/// `FirstGeomBlob` / `FirstRasterBlob`, etc.).
fn emit_dynlink_agg_finalize_body(
    invoke_name: &str,
    ret: &RetShape,
    extras: &[ParamShape],
) -> String {
    let invoke = invoke_name.replace('"', "\\\"");
    let mut s = String::new();
    s.push_str(
        "                let geom_list = CborValue::List(\n\
         \x20                   st.blobs.iter().map(|b| CborValue::Bytes(b.clone())).collect(),\n\
         \x20               );\n",
    );
    // Parse each config-string extra into its CBOR wire form.
    // Extras arrive via `create-accumulator-with-config(s)` as raw
    // planner-literal text — parse per WIT param type here so the
    // provider's per-arm decoders (`as_u32` / `as_f64` / …) see the
    // shape they expect.
    let mut payload_idents: Vec<String> = vec!["geom_list".to_string()];
    for (j, p) in extras.iter().enumerate() {
        let ident = format!("extra{}", j);
        let parse = match p {
            ParamShape::F64 => format!(
                "                let {ident} = {{\n\
                 \x20                   let s = st.extras.get({j}).ok_or_else(|| ftypes::FunctionError::ExecutionError(alloc::format!(\"{invoke}: missing extra arg {j}\")))?;\n\
                 \x20                   let v: f64 = s.parse().map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!(\"{invoke}: extra arg {j} not f64: {{}}\", e)))?;\n\
                 \x20                   CborValue::Float(v)\n\
                 \x20               }};\n",
            ),
            ParamShape::U32 | ParamShape::U64 => format!(
                "                let {ident} = {{\n\
                 \x20                   let s = st.extras.get({j}).ok_or_else(|| ftypes::FunctionError::ExecutionError(alloc::format!(\"{invoke}: missing extra arg {j}\")))?;\n\
                 \x20                   let v: u64 = s.parse().map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!(\"{invoke}: extra arg {j} not unsigned int: {{}}\", e)))?;\n\
                 \x20                   CborValue::Uint(v)\n\
                 \x20               }};\n",
            ),
            ParamShape::S32 | ParamShape::S64 => format!(
                "                let {ident} = {{\n\
                 \x20                   let s = st.extras.get({j}).ok_or_else(|| ftypes::FunctionError::ExecutionError(alloc::format!(\"{invoke}: missing extra arg {j}\")))?;\n\
                 \x20                   let v: i64 = s.parse().map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!(\"{invoke}: extra arg {j} not signed int: {{}}\", e)))?;\n\
                 \x20                   CborValue::Int(v)\n\
                 \x20               }};\n",
            ),
            ParamShape::Bool => format!(
                "                let {ident} = {{\n\
                 \x20                   let s = st.extras.get({j}).ok_or_else(|| ftypes::FunctionError::ExecutionError(alloc::format!(\"{invoke}: missing extra arg {j}\")))?;\n\
                 \x20                   let v: bool = s.parse().map_err(|e| ftypes::FunctionError::ExecutionError(alloc::format!(\"{invoke}: extra arg {j} not bool: {{}}\", e)))?;\n\
                 \x20                   CborValue::Bool(v)\n\
                 \x20               }};\n",
            ),
            ParamShape::Text => format!(
                "                let {ident} = {{\n\
                 \x20                   let s = st.extras.get({j}).ok_or_else(|| ftypes::FunctionError::ExecutionError(alloc::format!(\"{invoke}: missing extra arg {j}\")))?;\n\
                 \x20                   CborValue::Text(s.clone())\n\
                 \x20               }};\n",
            ),
            _ => format!(
                "                let {ident} = CborValue::Null; // unsupported extra shape (should be gated by is_dynlink_wired_aggregate)\n",
            ),
        };
        s.push_str(&parse);
        payload_idents.push(ident);
    }
    s.push_str(&format!(
        "                let resp = call(\"{invoke}\", alloc::vec![{}])?;\n",
        payload_idents.join(", "),
    ));
    let wrap = match ret {
        RetShape::GeomBlob
        | RetShape::Blob
        | RetShape::OptionGeomBlob
        | RetShape::OptionBlob
        // RasterBlob shape: raster aggregates emit the aggregated
        // raster's `as_binary()` as `CborValue::Bytes` on the wire.
        // Same envelope as GeomBlob — the SQL surface is Binary
        // either way; the classifier keeps the shape distinct only
        // for provider-side substrate accounting.
        | RetShape::RasterBlob
        | RetShape::OptionRasterBlob => {
            "                match resp {\n\
             \x20                   ResponseValue::Bytes(b) => Ok(ftypes::ScalarValue::Binary(b)),\n\
             \x20                   ResponseValue::Null => Ok(ftypes::ScalarValue::Null),\n\
             \x20                   other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"aggregate: unexpected response {:?}\", other))),\n\
             \x20               }\n"
        }
        // list<geometry>-then-first projection: the provider emits
        // `CborValue::List` of per-cluster WKB payloads; SQL surface
        // is a single Binary (first element) or Null on empty. The
        // classifier assigns this shape when a `list<geometry>`
        // return is folded into a scalar SQL slot — same rule the
        // scalar-side FirstGeomBlob wrap follows.
        RetShape::FirstGeomBlob
        | RetShape::FirstRasterBlob => {
            "                match resp {\n\
             \x20                   ResponseValue::Bytes(b) => Ok(ftypes::ScalarValue::Binary(b)),\n\
             \x20                   ResponseValue::List(items) => match items.into_iter().next() {\n\
             \x20                       Some(ResponseValue::Bytes(b)) => Ok(ftypes::ScalarValue::Binary(b)),\n\
             \x20                       Some(ResponseValue::Null) | None => Ok(ftypes::ScalarValue::Null),\n\
             \x20                       Some(other) => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"aggregate first-of-list: unexpected element {:?}\", other))),\n\
             \x20                   },\n\
             \x20                   ResponseValue::Null => Ok(ftypes::ScalarValue::Null),\n\
             \x20                   other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"aggregate: unexpected response {:?}\", other))),\n\
             \x20               }\n"
        }
        RetShape::BboxBlob => {
            "                match resp {\n\
             \x20                   ResponseValue::Null => Ok(ftypes::ScalarValue::Null),\n\
             \x20                   other => {\n\
             \x20                       let bb = response_bbox_floats(&other, \"aggregate bbox\")?;\n\
             \x20                       Ok(ftypes::ScalarValue::Binary(bbox_to_wkb_polygon(bb[0], bb[1], bb[2], bb[3])))\n\
             \x20                   }\n\
             \x20               }\n"
        }
        RetShape::Bbox3dWkbLineZ => {
            "                match resp {\n\
             \x20                   ResponseValue::Null => Ok(ftypes::ScalarValue::Null),\n\
             \x20                   other => {\n\
             \x20                       let bb = response_bbox3d_floats(&other, \"aggregate bbox3d\")?;\n\
             \x20                       Ok(ftypes::ScalarValue::Binary(bbox3d_to_wkb_linestring_z(bb[0], bb[1], bb[2], bb[3], bb[4], bb[5])))\n\
             \x20                   }\n\
             \x20               }\n"
        }
        RetShape::Real | RetShape::OptionReal => {
            "                match resp {\n\
             \x20                   ResponseValue::Float(f) => Ok(ftypes::ScalarValue::Float64(f)),\n\
             \x20                   ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Float64(i as f64)),\n\
             \x20                   ResponseValue::Uint(u) => Ok(ftypes::ScalarValue::Float64(u as f64)),\n\
             \x20                   ResponseValue::Null => Ok(ftypes::ScalarValue::Null),\n\
             \x20                   other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"aggregate: unexpected response {:?}\", other))),\n\
             \x20               }\n"
        }
        RetShape::Int | RetShape::OptionInt => {
            "                match resp {\n\
             \x20                   ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Int64(i)),\n\
             \x20                   ResponseValue::Uint(u) => Ok(ftypes::ScalarValue::Int64(u as i64)),\n\
             \x20                   ResponseValue::Null => Ok(ftypes::ScalarValue::Null),\n\
             \x20                   other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"aggregate: unexpected response {:?}\", other))),\n\
             \x20               }\n"
        }
        RetShape::Text | RetShape::OptionText => {
            "                match resp {\n\
             \x20                   ResponseValue::Text(t) => Ok(ftypes::ScalarValue::Utf8(t)),\n\
             \x20                   ResponseValue::Null => Ok(ftypes::ScalarValue::Null),\n\
             \x20                   other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"aggregate: unexpected response {:?}\", other))),\n\
             \x20               }\n"
        }
        // Record-shaped returns (mobilitydb temporal aggregators):
        // the provider re-serialises the upstream record as CBOR
        // bytes (`ResponseValue::Bytes`) and we forward as SQL
        // BINARY. `FirstWitValueRecord` is symmetric — the provider
        // has already collapsed to a single record before returning.
        RetShape::WitValueRecord { .. }
        | RetShape::OptionWitValueRecord { .. }
        | RetShape::FirstWitValueRecord { .. } => {
            "                match resp {\n\
             \x20                   ResponseValue::Bytes(b) => Ok(ftypes::ScalarValue::Binary(b)),\n\
             \x20                   ResponseValue::Null => Ok(ftypes::ScalarValue::Null),\n\
             \x20                   other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"aggregate: unexpected response {:?}\", other))),\n\
             \x20               }\n"
        }
        // JSON-shaped returns (RecordToTuple option<tuple<...>>,
        // RecordToListPrim list<primitive>, RecordSetToRecordSet
        // list<record>): provider serialises via serde_json and
        // returns Text; bridge forwards as SQL TEXT. Null is passed
        // through so `option<...>` shapes surface SQL NULL cleanly.
        RetShape::JsonText { .. } => {
            "                match resp {\n\
             \x20                   ResponseValue::Text(t) => Ok(ftypes::ScalarValue::Utf8(t)),\n\
             \x20                   ResponseValue::Null => Ok(ftypes::ScalarValue::Null),\n\
             \x20                   other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"aggregate: unexpected response {:?}\", other))),\n\
             \x20               }\n"
        }
        _ => {
            "                Err(ftypes::FunctionError::ExecutionError(\"aggregate finalize: unsupported return shape\".to_string()))\n"
        }
    };
    s.push_str(wrap);
    s
}

/// Rust literal for a `Vec<String>` of aliases inside the emitted
/// aggregate meta block. Empty aliases collapse to `Vec::new()` so
/// the generated code doesn't drag in an empty `alloc::vec![]`.
fn aliases_literal_dynlink(aliases: &[String]) -> String {
    if aliases.is_empty() {
        return "Vec::new()".to_string();
    }
    let mut s = String::from("alloc::vec![");
    for a in aliases {
        s.push_str(&format!("\"{}\".to_string(), ", a.replace('"', "\\\"")));
    }
    s.push(']');
    s
}

fn pick_primary_shim_dir(
    primary: &str,
    wit_deps_root: &Path,
    _shim_packages: &[datalink_shim_codegen_core::wit_parse::WitPackage],
) -> PathBuf {
    // Try `<primary>-wasm`, then `<primary>`, then any dir under the
    // root that starts with the primary name. Falls back to the root
    // itself. Matches the semantics of emit_lib::pick_primary_shim_dir.
    for cand in [
        wit_deps_root.join(format!("{primary}-wasm")),
        wit_deps_root.join(primary.replace('_', "-")),
    ] {
        if cand.is_dir() {
            return cand;
        }
    }
    if let Ok(rd) = std::fs::read_dir(wit_deps_root) {
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let n = e.file_name();
            let s = n.to_string_lossy();
            if s.starts_with(primary) {
                return e.path();
            }
        }
    }
    wit_deps_root.to_path_buf()
}
