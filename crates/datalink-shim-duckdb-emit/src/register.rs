//! Emit `fn register_scalars() -> Result<(), types::Duckerror>`.
//!
//! Each scalar in the interface DB becomes one call to
//! `registry.register(name, &args, &ret, callback, Some(&opts))`
//! against the runtime's scalar-capability registry. The handle
//! returned by `runtime::ScalarCallback::new(handle)` routes every
//! invocation back to a `SCALAR_ARMS` match in the dispatch impl —
//! the same `handle` we slotted into `handle_table` at register
//! time.
//!
//! ## Funcarg / Logicaltype derivation
//!
//! Per-arg `Logicaltype` is computed from the IR-side `ParamShape`
//! variant (via `paramshape_to_logicaltype`) so DuckDB's planner
//! receives real width hints rather than the previous uniform
//! `Text`-default. The same per-arg shape drives the dispatch arm
//! body in `dispatch.rs`, so registration declarations and dispatch
//! decoding agree by construction.
//!
//! ## Return type
//!
//! Same idea for the return Logicaltype: derived from `RetShape`
//! via `retshape_to_logicaltype`. Today's PostGIS scalars uniformly
//! return Blob (WKB) or Float64 (lengths / distances) or Boolean
//! (predicates) — all of which now map to their precise arm rather
//! than a `Text` placeholder.
//!
//! ## Iteration order vs dispatch arm-idx
//!
//! We walk the post-classifier `scalar_entries` list (not the raw
//! BridgePlan) with a `seen` set on `sql_name`. The first writer
//! claims an arm_idx; subsequent entries with the same SQL name
//! (typically canonical+alias collisions) are skipped. This mirrors
//! `emit_lib::build_scalar_arms` so the (handle → arm_idx) map
//! `register_scalars()` installs lines up with the actual dispatch
//! match arms it points at.
//!
//! ## NULL handling
//!
//! Default DuckDB behavior: arguments that are NULL short-circuit
//! to NULL without invoking the function. PostGIS scalars are
//! uniformly null-propagating in practice, so the scalar-first cut
//! does NOT call `runtime-ext.register-scalar-ex` (which would let
//! us mark `null-handling: special`). The base `runtime.scalar-
//! registry.register` is sufficient.

use anyhow::Result;
use shim_bridge_codegen_core::BridgePlan;

use datalink_shim_codegen_core::interface_db::{
    DispatchEntry, ParamShape, RetShape,
};

/// Render the `register_scalars()` body. Walks `scalar_entries` in
/// declaration order with a `sql_name` dedupe (matching
/// `emit_lib::build_scalar_arms`), allocates one handle per unique
/// SQL name, slots `(handle, arm_idx)` into `handle_table`, and
/// registers the scalar with per-arg `Logicaltype` widths derived
/// from the `ParamShape` IR.
///
/// `plan` is consulted only for the `is_deterministic` flag on
/// each scalar (the post-classifier `DispatchEntry` doesn't carry
/// that bit; it lives on the `BridgePlan` side).
pub fn render(
    plan: &BridgePlan,
    scalar_entries: &[(DispatchEntry, bool)],
) -> Result<String> {
    let mut det: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    for ext in &plan.extensions {
        for sc in &ext.scalars {
            det.insert(sc.canonical_name.clone(), sc.is_deterministic);
            for alias in &sc.aliases {
                det.insert(alias.clone(), sc.is_deterministic);
            }
        }
    }

    let mut s = String::new();
    s.push_str(REGISTER_PRELUDE);

    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut arm_idx: usize = 0;
    for (entry, _fallible) in scalar_entries {
        if !seen.insert(entry.sql_name.clone()) {
            // first writer wins — mirrors emit_lib::build_scalar_arms
            // so handle→arm_idx points at the actual dispatch arm.
            continue;
        }
        let deterministic =
            det.get(&entry.sql_name).copied().unwrap_or(true);
        push_registration(
            &mut s,
            arm_idx,
            &entry.sql_name,
            &entry.shape.params,
            &entry.shape.ret,
            deterministic,
        );
        arm_idx += 1;
    }

    s.push_str("    Ok(())\n}\n");
    Ok(s)
}

const REGISTER_PRELUDE: &str = r##"
fn register_scalars() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| {
            types::Duckerror::Internal(
                "host did not expose scalar capability".into(),
            )
        })?;
    let registry = match capability {
        runtime::Capability::Scalar(r) => r,
        _ => {
            return Err(types::Duckerror::Internal(
                "scalar capability returned unexpected variant".into(),
            ));
        }
    };
"##;

fn push_registration(
    out: &mut String,
    arm_idx: usize,
    sql_name: &str,
    params: &[ParamShape],
    ret: &RetShape,
    deterministic: bool,
) {
    let mut args_block = String::new();
    for (i, p) in params.iter().enumerate() {
        let logical = paramshape_to_logicaltype(p);
        args_block.push_str(&format!(
            "            runtime::Funcarg {{\n\
             \x20               name: Some(\"arg{i}\".into()),\n\
             \x20               logical: {logical},\n\
             \x20           }},\n",
            i = i,
            logical = logical,
        ));
    }
    let ret_logical = retshape_to_logicaltype(ret);
    let attrs = if deterministic {
        "types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS"
    } else {
        "types::Funcflags::STATELESS"
    };
    out.push_str(&format!(
        r##"    {{
        let handle = NEXT_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        handle_table()
            .lock()
            .expect("scalar handle mutex poisoned")
            .insert(handle, {arm_idx}usize);
        let callback = runtime::ScalarCallback::new(handle);
        let args: Vec<runtime::Funcarg> = vec![
{args_block}        ];
        let opts = runtime::Funcopts {{
            description: Some("{sql_name} (sqlink-shim-codegen)".into()),
            tags: vec!["{sql_name}".into()],
            attributes: {attrs},
        }};
        registry.register(
            "{sql_name}",
            &args,
            &{ret_logical},
            callback,
            Some(&opts),
        )?;
    }}
"##,
        arm_idx = arm_idx,
        sql_name = sql_name.replace('"', "\\\""),
        args_block = args_block,
        attrs = attrs,
        ret_logical = ret_logical,
    ));
}

/// Map a `ParamShape` variant to the Rust source for the
/// `types::Logicaltype` value DuckDB should see at registration
/// time. The DuckDB FROZEN logical set is narrower than the IR
/// (Boolean / Int64 / Float64 / Text / Blob / Complex(name)); we
/// pick the widest arm for each primitive shape and use `Complex`
/// for typed-value-binding records (wit-value path).
///
/// Shapes the scalar-first cut hasn't wired into the dispatch arm
/// yet (Geom / Geog / Raster / Topology / ListGeom / ListPrim /
/// ListRecord / ListTuple) still produce a sensible declaration
/// here so DuckDB's planner gets the right surface type even
/// before the dispatch arm itself stops returning Unsupported:
///
///   - `Geom` / `Geog` / `Raster` / `Topology` → `Blob` (WKB /
///     binary / bytes surface; the SQL caller always passes a
///     BLOB column).
///   - `ListGeom` → `Blob` (variadic blob; each row is one
///     geometry blob).
///   - `ListPrim` / `ListRecord` / `ListTuple` → `Text` (JSON
///     array literal at the SQL surface).
///   - `Enum` → `Int64` (case index).
///   - `WitValueRecord` → `Complex(kebab_name)` so the runtime can
///     route typed-value-binding payloads by symbolic name.
///   - `OptionNone` → `Text` (placeholder; the dispatch arm
///     ignores this slot).
pub fn paramshape_to_logicaltype(p: &ParamShape) -> String {
    match p {
        ParamShape::Bool => "types::Logicaltype::Boolean".to_string(),
        ParamShape::S32
        | ParamShape::S64
        | ParamShape::U32
        | ParamShape::U64 => "types::Logicaltype::Int64".to_string(),
        ParamShape::F64 => "types::Logicaltype::Float64".to_string(),
        ParamShape::Text => "types::Logicaltype::Text".to_string(),
        ParamShape::Blob => "types::Logicaltype::Blob".to_string(),
        ParamShape::Geom
        | ParamShape::Geog
        | ParamShape::Raster
        | ParamShape::Topology => "types::Logicaltype::Blob".to_string(),
        ParamShape::ListGeom => "types::Logicaltype::Blob".to_string(),
        ParamShape::ListPrim(_)
        | ParamShape::ListRecord { .. }
        | ParamShape::ListTuple { .. } => "types::Logicaltype::Text".to_string(),
        ParamShape::Enum { .. } => "types::Logicaltype::Int64".to_string(),
        ParamShape::WitValueRecord { kebab_name, .. } => {
            let n = kebab_name.replace('"', "\\\"");
            format!("types::Logicaltype::Complex(\"{n}\".into())")
        }
        ParamShape::OptionNone => "types::Logicaltype::Text".to_string(),
    }
}

/// Map a `RetShape` variant to the Rust source for the return
/// `types::Logicaltype` value DuckDB should see. Same FROZEN
/// logical-set rules as the param mapping. Shapes the scalar-first
/// cut hasn't yet wired (FirstWitValueRecord, OptionWitValueRecord,
/// TuplePick, etc.) still produce a sensible declaration so the
/// planner sees the right surface type.
pub fn retshape_to_logicaltype(r: &RetShape) -> String {
    match r {
        RetShape::Text => "types::Logicaltype::Text".to_string(),
        RetShape::Real => "types::Logicaltype::Float64".to_string(),
        RetShape::Int => "types::Logicaltype::Int64".to_string(),
        RetShape::BoolInt => "types::Logicaltype::Boolean".to_string(),
        RetShape::Blob => "types::Logicaltype::Blob".to_string(),
        RetShape::GeomBlob
        | RetShape::RasterBlob
        | RetShape::TopologyBlob
        | RetShape::BboxBlob
        | RetShape::FirstGeomBlob
        | RetShape::FirstRasterBlob
        | RetShape::FirstTopologyBlob => "types::Logicaltype::Blob".to_string(),
        RetShape::IsValidDetailText => "types::Logicaltype::Text".to_string(),
        RetShape::OptionText => "types::Logicaltype::Text".to_string(),
        RetShape::OptionReal => "types::Logicaltype::Float64".to_string(),
        RetShape::OptionInt | RetShape::FirstOptionU32Int | RetShape::FirstInt => {
            "types::Logicaltype::Int64".to_string()
        }
        RetShape::OptionBoolInt => "types::Logicaltype::Boolean".to_string(),
        RetShape::OptionBlob
        | RetShape::OptionGeomBlob
        | RetShape::OptionRasterBlob
        | RetShape::OptionTopologyBlob => "types::Logicaltype::Blob".to_string(),
        RetShape::FirstReal => "types::Logicaltype::Float64".to_string(),
        RetShape::FirstText => "types::Logicaltype::Text".to_string(),
        // Enum return marshals to the case-index integer.
        RetShape::Enum { .. } => "types::Logicaltype::Int64".to_string(),
        // JsonText: nested compound returns rendered as JSON
        // string for SQL `json_each` consumption.
        RetShape::JsonText { .. } => "types::Logicaltype::Text".to_string(),
        // TuplePick: surface element kind drives the declaration.
        RetShape::TuplePick { elem, .. } => match elem {
            datalink_shim_codegen_core::interface_db::ListPrimElem::F64
            | datalink_shim_codegen_core::interface_db::ListPrimElem::F32 => {
                "types::Logicaltype::Float64".to_string()
            }
            datalink_shim_codegen_core::interface_db::ListPrimElem::S32
            | datalink_shim_codegen_core::interface_db::ListPrimElem::S64
            | datalink_shim_codegen_core::interface_db::ListPrimElem::U32
            | datalink_shim_codegen_core::interface_db::ListPrimElem::U64
            | datalink_shim_codegen_core::interface_db::ListPrimElem::U8 => {
                "types::Logicaltype::Int64".to_string()
            }
            datalink_shim_codegen_core::interface_db::ListPrimElem::Bool => {
                "types::Logicaltype::Boolean".to_string()
            }
            datalink_shim_codegen_core::interface_db::ListPrimElem::String => {
                "types::Logicaltype::Text".to_string()
            }
        },
        RetShape::WitValueRecord { kebab_name, .. }
        | RetShape::OptionWitValueRecord { kebab_name, .. }
        | RetShape::FirstWitValueRecord { kebab_name, .. } => {
            let n = kebab_name.replace('"', "\\\"");
            format!("types::Logicaltype::Complex(\"{n}\".into())")
        }
    }
}
