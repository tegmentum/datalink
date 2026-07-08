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
        | ParamShape::ListTuple { .. }
        | ParamShape::ListTupleMixed { .. }
        | ParamShape::ListListU8
        | ParamShape::ListListPrim(_)
        | ParamShape::ListListRecord { .. } => "types::Logicaltype::Text".to_string(),
        ParamShape::Enum { .. } => "types::Logicaltype::Int64".to_string(),
        ParamShape::WitValueRecord { kebab_name: _, .. } => {
            // WIT-value records ferry as JSON strings via
            // `Duckvalue::Complex { type_expr, json }`. The
            // REGISTRATION-time logical type just needs to give
            // DuckDB a resolvable SQL type — VARCHAR matches the
            // JSON payload and, crucially, is a valid SQL ident
            // that DuckDB can parse in `SELECT CAST(NULL AS
            // VARCHAR) AS x` (the ducklink core's type resolver).
            // The record's SEMANTIC identity is preserved at
            // runtime by `Complexvalue.type_expr` (which carries
            // the full `<pkg>@<ver>/<iface>/<kebab>` symbolic),
            // not by this logical-type name.
            "types::Logicaltype::Complex(\"VARCHAR\".into())".to_string()
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
        | RetShape::TopoGeometryViaGeom
        | RetShape::BboxBlob
        | RetShape::FirstGeomBlob
        | RetShape::FirstRasterBlob
        | RetShape::FirstTopologyBlob => "types::Logicaltype::Blob".to_string(),
        RetShape::IsValidDetailText => "types::Logicaltype::Text".to_string(),
        // Gap G3 (#668): bbox3d returns surface as an ISO-WKB
        // `LINESTRING Z` blob (min-corner -> max-corner diagonal).
        RetShape::Bbox3dWkbLineZ => "types::Logicaltype::Blob".to_string(),
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
        // #716: option<enum> mirrors on the Int64 side; the None arm
        // is handled by the emit dispatch, not the type mapping.
        RetShape::Enum { .. } | RetShape::OptionEnum { .. } => "types::Logicaltype::Int64".to_string(),
        // JsonText: nested compound returns rendered as JSON
        // string for SQL `json_each` consumption.
        RetShape::JsonText { .. } => "types::Logicaltype::Text".to_string(),
        // #677: `list<bool>` / `list<list<u8>>` batch returns
        // rendered as JSON text (symmetric with the param-side
        // ListListU8 convention).
        RetShape::ListBool | RetShape::ListListU8 => "types::Logicaltype::Text".to_string(),
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
        RetShape::WitValueRecord { kebab_name: _, .. }
        | RetShape::OptionWitValueRecord { kebab_name: _, .. }
        | RetShape::FirstWitValueRecord { kebab_name: _, .. } => {
            // See ParamShape::WitValueRecord above — VARCHAR is
            // the resolvable-by-DuckDB stand-in for the JSON
            // wit-value payload; runtime `Complexvalue.type_expr`
            // preserves the record's semantic identity.
            "types::Logicaltype::Complex(\"VARCHAR\".into())".to_string()
        }
        // #690: `result<_, E>` mutator returns surface as SQL NULL;
        // pick a neutral declared type so the planner sees a
        // working surface (the runtime always returns Null).
        RetShape::Unit => "types::Logicaltype::Blob".to_string(),
    }
}

// ─── Aggregate registration ───
//
// Mirrors `render` but against `Capabilitykind::Aggregate` and
// the aggregate-specific callback / handle table. Each aggregate
// in the interface DB becomes one call to
// `registry.register(name, &args, &ret, AggregateCallback::new(handle), Some(&opts))`.
//
// The arg/return Logicaltype declarations come from the same
// `paramshape_to_logicaltype` / `retshape_to_logicaltype` helpers
// the scalar path uses — the streaming accumulator column is the
// first arg (always `Blob` for Geom / Raster shapes) followed by
// any `extra_args` constants.
pub fn render_aggregates(
    plan: &shim_bridge_codegen_core::BridgePlan,
    agg_entries: &[datalink_shim_codegen_core::interface_db::AggregateEntry],
) -> anyhow::Result<String> {
    use datalink_shim_codegen_core::interface_db::AccKind;
    let mut det: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    for ext in &plan.extensions {
        for ag in &ext.aggregates {
            det.insert(ag.canonical_name.clone(), true);
            for alias in &ag.aliases {
                det.insert(alias.clone(), true);
            }
        }
    }

    let mut s = String::new();
    s.push_str(AGGREGATE_REGISTER_PRELUDE);

    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut arm_idx: usize = 0;
    for entry in agg_entries {
        // Phase 1A: each AggregateEntry now carries its canonical
        // sql_name + `aliases: Vec<String>` inline. Iterate canonical
        // + each alias so the per-name handle / arm_idx assignments
        // mirror the pre-Phase-1A per-entry walk (where each alias
        // was a separate `AggregateEntry`). The arm_idx counter still
        // advances per registered name so `build_aggregate_arms`'s
        // dispatch arms stay aligned.
        for name in std::iter::once(entry.sql_name.clone())
            .chain(entry.aliases.iter().cloned())
        {
            if !seen.insert(name.clone()) {
                // first writer wins — mirrors build_aggregate_arms's dedupe.
                continue;
            }
            // Arg 0 is the streaming accumulator column: Blob for both
            // Geom and Raster aggregates (the WKB / raster-binary the
            // bridge decodes inside the dispatch arm).
            //
            // #607 Phase 1: AccKind::Record aggregates are not yet
            // wired on the DuckDB target — the dispatch arm short-
            // circuits to a runtime stub. Skip the register entry so
            // DuckDB doesn't advertise an unimplemented aggregate;
            // Phase 2 (per the aggregate-substrate plan) replaces this
            // skip with the real Logicaltype mapping.
            let mut args_block = String::new();
            let acc_logical = match &entry.shape.accumulator_kind {
                AccKind::Geom | AccKind::Raster => "types::Logicaltype::Blob",
                // Record / RecordToScalar / RecordToTuple accumulators
                // stream wit-value payloads through column 0. Host-side
                // registration on the DuckDB target is still pending
                // wider runtime wiring (the dispatch arm is emitted, but
                // the `register_aggregates` block doesn't currently
                // advertise the Logicaltype::Complex accumulator —
                // matches the pre-#611 pattern for AccKind::Record).
                AccKind::Record { .. }
                | AccKind::RecordToScalar { .. }
                | AccKind::RecordToTuple { .. }
                // #830: RecordToListPrim — record-in / primitive-list-out
                // aggregate. Full DuckDB runtime wiring lives in a follow-
                // up; skip registration here for the same reason as the
                // Record family above.
                | AccKind::RecordToListPrim { .. }
                // #799: RecordSetToRecordSet — nested-list aggregate
                // (`<int|float|date|tstz>-spanset-aggregate-union`).
                // Full DuckDB runtime wiring lives in a follow-up;
                // skip registration here for the same reason as the
                // Record family above.
                | AccKind::RecordSetToRecordSet { .. } => continue,
            };
            args_block.push_str(&format!(
                "            runtime::Funcarg {{\n\
                 \x20               name: Some(\"arg0\".into()),\n\
                 \x20               logical: {acc_logical},\n\
                 \x20           }},\n",
            ));
            for (i, p) in entry.shape.extra_args.iter().enumerate() {
                let logical = paramshape_to_logicaltype(p);
                let i1 = i + 1;
                args_block.push_str(&format!(
                    "            runtime::Funcarg {{\n\
                     \x20               name: Some(\"arg{i1}\".into()),\n\
                     \x20               logical: {logical},\n\
                     \x20           }},\n",
                ));
            }
            let ret_logical = retshape_to_logicaltype(&entry.shape.ret);
            let deterministic =
                det.get(&name).copied().unwrap_or(true);
            let attrs = if deterministic {
                "types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS"
            } else {
                "types::Funcflags::STATELESS"
            };
            let sql_name = name.replace('"', "\\\"");
            s.push_str(&format!(
                r##"    {{
        let handle = NEXT_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        aggregate_handle_table()
            .lock()
            .expect("aggregate handle mutex poisoned")
            .insert(handle, {arm_idx}usize);
        let callback = runtime::AggregateCallback::new(handle);
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
            ));
            arm_idx += 1;
        }
    }

    s.push_str("    Ok(())\n}\n");
    Ok(s)
}

const AGGREGATE_REGISTER_PRELUDE: &str = r##"
fn register_aggregates() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Aggregate)
        .ok_or_else(|| {
            types::Duckerror::Internal(
                "host did not expose aggregate capability".into(),
            )
        })?;
    let registry = match capability {
        runtime::Capability::Aggregate(r) => r,
        _ => {
            return Err(types::Duckerror::Internal(
                "aggregate capability returned unexpected variant".into(),
            ));
        }
    };
"##;

// ─── Window-function registration (#661) ───
//
// The @4.0.0 contract has no separate window-registry resource —
// the engine treats a window function as an aggregate plus FRAME
// access (see aggregate-incr-dispatch.wit). The bridge registers
// each classified window function via the same `aggregate-registry`
// the standard aggregate path uses, slotting the returned handle
// into `window_handle_table` so the `call_aggregate_window` arm can
// route by handle to the per-arm dispatch body.
//
// Streaming arg 0 is always Blob (WKB partition rows for the
// postgis cluster surface); extras follow with their classified
// Logicaltype. The return Logicaltype is derived from the
// `WindowReturn` discriminant:
//   * OptionU32  -> Int64 (NULL for noise points)
//   * U32        -> Int64 (cluster id)
//   * GeomBlob   -> Blob  (per-cluster GeometryCollection WKB)
pub fn render_windows(
    window_entries: &[datalink_shim_codegen_core::interface_db::WindowEntry],
) -> anyhow::Result<String> {
    use datalink_shim_codegen_core::interface_db::WindowReturn;

    let mut s = String::new();
    s.push_str(WINDOW_REGISTER_PRELUDE);

    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut arm_idx: usize = 0;
    for entry in window_entries {
        if !seen.insert(entry.sql_name.clone()) {
            continue;
        }
        // Arg 0 is the streaming geometry column (WKB), Logicaltype::Blob.
        let mut args_block = String::new();
        args_block.push_str(
            "            runtime::Funcarg {\n\
             \x20               name: Some(\"arg0\".into()),\n\
             \x20               logical: types::Logicaltype::Blob,\n\
             \x20           },\n",
        );
        for (i, p) in entry.shape.extra_args.iter().enumerate() {
            let logical = paramshape_to_logicaltype(p);
            let i1 = i + 1;
            args_block.push_str(&format!(
                "            runtime::Funcarg {{\n\
                 \x20               name: Some(\"arg{i1}\".into()),\n\
                 \x20               logical: {logical},\n\
                 \x20           }},\n",
            ));
        }
        let ret_logical = match &entry.shape.returns {
            WindowReturn::OptionU32 | WindowReturn::U32 => "types::Logicaltype::Int64",
            WindowReturn::GeomBlob => "types::Logicaltype::Blob",
        };
        let sql_name = entry.sql_name.replace('"', "\\\"");
        s.push_str(&format!(
            r##"    {{
        let handle = NEXT_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        window_handle_table()
            .lock()
            .expect("window handle mutex poisoned")
            .insert(handle, {arm_idx}usize);
        let callback = runtime::AggregateCallback::new(handle);
        let args: Vec<runtime::Funcarg> = vec![
{args_block}        ];
        let opts = runtime::Funcopts {{
            description: Some("{sql_name} (sqlink-shim-codegen window)".into()),
            tags: vec!["{sql_name}".into(), "window".into()],
            attributes: types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS,
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
        ));
        arm_idx += 1;
    }

    s.push_str("    Ok(())\n}\n");
    Ok(s)
}

const WINDOW_REGISTER_PRELUDE: &str = r##"
fn register_windows() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Aggregate)
        .ok_or_else(|| {
            types::Duckerror::Internal(
                "host did not expose aggregate capability (window registration)".into(),
            )
        })?;
    let registry = match capability {
        runtime::Capability::Aggregate(r) => r,
        _ => {
            return Err(types::Duckerror::Internal(
                "aggregate capability returned unexpected variant (window registration)".into(),
            ));
        }
    };
"##;

// ─── Table function (UDTF) registration ───
//
// Mirrors `render` but against `Capabilitykind::Table` and the
// `table-callback` resource. Per-UDTF the codegen emits:
//   * Funcarg list per param (Logicaltype derived from ParamShape)
//   * Columndef list per output row column (derived from
//     UdtfOutputRow + UdtfFieldShape)
//   * register(name, args, columns, table_callback, opts)
pub fn render_tables(
    plan: &shim_bridge_codegen_core::BridgePlan,
    udtf_entries: &[datalink_shim_codegen_core::interface_db::UdtfEntry],
) -> anyhow::Result<String> {
    use datalink_shim_codegen_core::interface_db::{
        ColumnAffinity, UdtfFieldShape, UdtfOutputRow,
    };
    // TableFn doesn't carry an is_deterministic flag (UDTFs are
    // assumed deterministic over their args; the runtime treats
    // them as table sources rather than projections). `plan` is
    // kept for symmetry with the scalar / aggregate render entry
    // points but unused here.
    let _ = plan;

    let mut s = String::new();
    s.push_str(TABLE_REGISTER_PRELUDE);

    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut arm_idx: usize = 0;
    for entry in udtf_entries {
        if !seen.insert(entry.sql_name.clone()) {
            continue;
        }
        // ── Funcarg list (one per WIT param)
        let mut args_block = String::new();
        for (i, p) in entry.shape.params.iter().enumerate() {
            let logical = paramshape_to_logicaltype(p);
            args_block.push_str(&format!(
                "            runtime::Funcarg {{\n\
                 \x20               name: Some(\"arg{i}\".into()),\n\
                 \x20               logical: {logical},\n\
                 \x20           }},\n",
            ));
        }
        // ── Columndef list (one per visible output row column)
        let mut cols_block = String::new();
        match &entry.shape.output_row {
            UdtfOutputRow::SingleGeom => {
                let col_name = datalink_shim_codegen_core::interface_db
                    ::single_geom_column_name_for(&entry.sql_name);
                cols_block.push_str(&format!(
                    "            runtime::Columndef {{\n\
                     \x20               name: \"{col_name}\".into(),\n\
                     \x20               logical: types::Logicaltype::Blob,\n\
                     \x20           }},\n",
                ));
            }
            UdtfOutputRow::SinglePrimitive { affinity } => {
                let logical = match affinity {
                    ColumnAffinity::Integer => "types::Logicaltype::Int64",
                    ColumnAffinity::Real => "types::Logicaltype::Float64",
                    ColumnAffinity::Text => "types::Logicaltype::Text",
                    ColumnAffinity::Blob => "types::Logicaltype::Blob",
                };
                cols_block.push_str(&format!(
                    "            runtime::Columndef {{\n\
                     \x20               name: \"value\".into(),\n\
                     \x20               logical: {logical},\n\
                     \x20           }},\n",
                ));
            }
            UdtfOutputRow::Record { fields } => {
                for f in fields {
                    let logical = match f.field_shape {
                        UdtfFieldShape::Int | UdtfFieldShape::OptionInt => {
                            "types::Logicaltype::Int64"
                        }
                        UdtfFieldShape::Real | UdtfFieldShape::OptionReal => {
                            "types::Logicaltype::Float64"
                        }
                        UdtfFieldShape::Text | UdtfFieldShape::OptionText => {
                            "types::Logicaltype::Text"
                        }
                        UdtfFieldShape::Blob
                        | UdtfFieldShape::OptionBlob
                        | UdtfFieldShape::GeomBlob
                        | UdtfFieldShape::OptionGeomBlob
                        | UdtfFieldShape::Unsupported => "types::Logicaltype::Blob",
                    };
                    let col_name = f.name.replace('"', "\\\"");
                    cols_block.push_str(&format!(
                        "            runtime::Columndef {{\n\
                         \x20               name: \"{col_name}\".into(),\n\
                         \x20               logical: {logical},\n\
                         \x20           }},\n",
                    ));
                }
            }
            UdtfOutputRow::Unwired { .. } => {
                // Still register a placeholder column so DuckDB
                // can route the call to call_table; the dispatch
                // arm will return Unsupported with the reason.
                cols_block.push_str(
                    "            runtime::Columndef {\n\
                     \x20               name: \"value\".into(),\n\
                     \x20               logical: types::Logicaltype::Blob,\n\
                     \x20           },\n",
                );
            }
        }
        let sql_name = entry.sql_name.replace('"', "\\\"");
        s.push_str(&format!(
            r##"    {{
        let handle = NEXT_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        table_handle_table()
            .lock()
            .expect("table handle mutex poisoned")
            .insert(handle, {arm_idx}usize);
        let callback = runtime::TableCallback::new(handle);
        let args: Vec<runtime::Funcarg> = vec![
{args_block}        ];
        let columns: Vec<runtime::Columndef> = vec![
{cols_block}        ];
        registry.register(
            "{sql_name}",
            &args,
            &columns,
            callback,
            None,
        )?;
    }}
"##,
        ));
        arm_idx += 1;
    }

    s.push_str("    Ok(())\n}\n");
    Ok(s)
}

const TABLE_REGISTER_PRELUDE: &str = r##"
fn register_tables() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| {
            types::Duckerror::Internal(
                "host did not expose table capability".into(),
            )
        })?;
    let registry = match capability {
        runtime::Capability::Table(r) => r,
        _ => {
            return Err(types::Duckerror::Internal(
                "table capability returned unexpected variant".into(),
            ));
        }
    };
"##;

// ─── Cast registration (#624) ───
//
// Per-cast `catalog::register-cast` against the `duckdb:extension`
// catalog interface. Each cast wraps an already-wired scalar
// (`cast.function_name` is a SQL function name that the scalar
// dispatch has an arm for). At register time we allocate a new
// handle, slot it into the SAME `handle_table` the scalars use —
// mapping `cast_handle -> scalar_arm_idx` — and register a
// `CastCallback::new(cast_handle)` with the host. At dispatch time
// `call_cast(handle, value)` forwards into `call_scalar(handle,
// vec![value], ctx)` which reads the same `handle_table`, finds
// the scalar arm, and runs the existing scalar body. One arm-index
// space; the cast contract just provides an alternate entry point.
//
// `source_kind` drives the cast-spec shape:
//   * `castsourcekind::stringliteral`  -> kind=Implicit, from=VARCHAR
//     (DuckDB auto-promotes literals).
//   * `castsourcekind::any`            -> kind=Explicit, from=BLOB
//     (any-expr cast over the bridge's BLOB-backed custom types).
//   * `castsourcekind::geographycolumn`-> kind=Explicit, from=GEOGRAPHY
//     (column-typed geography cast to geometry).
//
// `to` is the cast's target_type uppercased to match the SQL-side
// type name the bridge advertises via column_types / catalog
// aliases.
//
// A cast whose `function_name` is NOT a wired scalar is skipped
// with an `[duckdb-target]` diagnostic at codegen time. The
// caller is responsible for surfacing the count back to the
// maintainer (mirrors the scalar/aggregate/UDTF unwired-reason
// pattern).
pub fn render_casts(
    plan: &BridgePlan,
    scalar_entries: &[(DispatchEntry, bool)],
) -> anyhow::Result<String> {
    // Build (sql_name -> scalar_arm_idx) mirroring `render`'s
    // dedupe so the cast's slot lookup lines up with the dispatch
    // arm `call_scalar` will route through.
    let mut name_to_arm: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut arm_idx: usize = 0;
    for (entry, _fallible) in scalar_entries {
        if !seen.insert(entry.sql_name.clone()) {
            continue;
        }
        name_to_arm.insert(entry.sql_name.clone(), arm_idx);
        arm_idx += 1;
    }

    let mut s = String::new();
    s.push_str(CAST_REGISTER_PRELUDE);

    let mut unwired: Vec<(String, String, String)> = Vec::new();
    for ext in &plan.extensions {
        for cast in &ext.cast_rewrites {
            let scalar_arm = match name_to_arm.get(&cast.function_name) {
                Some(&i) => i,
                None => {
                    unwired.push((
                        cast.target_type.clone(),
                        cast.source_kind.clone(),
                        cast.function_name.clone(),
                    ));
                    continue;
                }
            };
            push_cast_registration(&mut s, scalar_arm, cast);
        }
    }

    if !unwired.is_empty() {
        eprintln!(
            "[duckdb-target] {} cast(s) not wired (function not in scalar registry):",
            unwired.len()
        );
        for (t, k, f) in &unwired {
            eprintln!("  - CAST(<{}> AS {}) -> {}", k, t, f);
        }
    }

    s.push_str("    Ok(())\n}\n");
    Ok(s)
}

const CAST_REGISTER_PRELUDE: &str = r##"
fn register_casts() -> Result<(), types::Duckerror> {
"##;

fn push_cast_registration(
    out: &mut String,
    scalar_arm_idx: usize,
    cast: &shim_bridge_codegen_core::CastRewrite,
) {
    let (kind_variant, from_type) = match cast.source_kind.as_str() {
        // PostGIS / mobilitydb extraction surfaces these as
        // `castsourcekind::<variant>` values from the shim
        // interface DB. Map each to a (CastKind, from-type) pair.
        "castsourcekind::stringliteral" => ("Implicit", "VARCHAR"),
        "castsourcekind::any" => ("Explicit", "BLOB"),
        "castsourcekind::geographycolumn" => ("Explicit", "GEOGRAPHY"),
        // Unknown source-kind: assume explicit, BLOB-typed source.
        // The host's cast registry will reject the spec if the
        // type names don't resolve; the diagnostic surfaces at
        // load() time rather than silently dropping the cast.
        _ => ("Explicit", "BLOB"),
    };
    let to_type = cast.target_type.to_uppercase();
    let fn_name = cast.function_name.replace('"', "\\\"");
    let source_kind = cast.source_kind.replace('"', "\\\"");
    out.push_str(&format!(
        r##"    {{
        // CAST(<{source_kind}> AS {to_type}) -> {fn_name}
        let handle = NEXT_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Slot the cast handle into the SCALAR handle table so
        // call_cast can forward into call_scalar at the matching
        // arm. One arm-index space; the cast contract just
        // provides an alternate entry point.
        handle_table()
            .lock()
            .expect("scalar handle mutex poisoned")
            .insert(handle, {scalar_arm_idx}usize);
        let callback = runtime::CastCallback::new(handle);
        let spec = catalog::CastSpec {{
            from: "{from_type}".into(),
            to: "{to_type}".into(),
            kind: catalog::CastKind::{kind_variant},
        }};
        catalog::register_cast(&spec, callback).map_err(|e| {{
            types::Duckerror::Internal(format!(
                "register-cast({fn_name}: {from_type} -> {to_type}): {{}}", e
            ))
        }})?;
    }}
"##,
        source_kind = source_kind,
        to_type = to_type,
        fn_name = fn_name,
        scalar_arm_idx = scalar_arm_idx,
        from_type = from_type,
        kind_variant = kind_variant,
    ));
}
