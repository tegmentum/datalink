//! `datafission:extension`-shape dispatch-arm emitter.
//!
//! Counterpart to `datalink-shim-duckdb-emit/src/dispatch.rs` but
//! over the datafission `ScalarValue` variant (defined in
//! `datafission:function-plugin/types`) instead of DuckDB's
//! `Duckvalue`. Consumes the same database-agnostic IR from
//! `datalink_shim_codegen_core::interface_db` (`DispatchEntry`,
//! `DispatchShape`, `ParamShape`, `RetShape`, ...) and renders the
//! Rust source of each match-arm body the dispatch loop calls.
//!
//! ## Scalar-first cut
//!
//! Only the scalar arms (`execute` / `execute-batch` of
//! `scalar-function-registry`) are wired. Aggregates / window /
//! table / type-plugin / spatial-index / system-catalog /
//! index-plugin export traits stub out: their `list-*` methods
//! return empty vectors, their per-call methods return
//! `FunctionError::UnknownFunction` / `SpatialError::UnsupportedOperation` /
//! `TypeError::Internal` / `IndexError::Internal` as appropriate.
//!
//! ## Marshaling shape
//!
//! The datafission `ScalarValue` variant is structurally similar
//! to DuckDB's `Duckvalue`:
//!
//!   * `Null` is its own arm; the dispatch path short-circuits to
//!     null OUT before any per-arm body runs when any input is
//!     null and the function's `propagates_null` metadata is true.
//!     (PostGIS scalars universally propagate; the codegen takes
//!     the conservative path of short-circuiting on Null inputs.)
//!   * Integer arms split by width (`Int8` / `Int16` / `Int32` /
//!     `Int64` / `Uint8` / `Uint16` / `Uint32` / `Uint64`) plus
//!     date / time / timestamp specialisations. The scalar-first
//!     emit unpacks all of these into `i64` (or `as i32` / etc.)
//!     since the underlying interface DB declares the function's
//!     param type and the emit just routes to the matching upstream
//!     WIT arg.
//!   * `Utf8(String)` is the text arm; `Binary(list<u8>)` is the
//!     blob arm. Geometry / Raster / Topology returns surface as
//!     `Binary` (PostGIS hands them out as WKB or raw bytes via
//!     `.as_wkb()` / `.as_binary()` / `.to_bytes()` — the SQL
//!     surface treats them as opaque blobs identically to the
//!     SQLite + DuckDB targets in this scalar-first cut).

use datalink_shim_codegen_core::interface_db::{
    DispatchShape, ParamShape, RetShape,
};

/// Emit the body of one match arm of the scalar dispatch by name
/// in `scalar_function_registry::execute`. The body unpacks the
/// `ScalarValue` arg slice into the WIT-side params, calls the
/// upstream function, and wraps the result back into a
/// `ScalarValue` for the dispatch loop to return.
///
/// `fallible` is the WIT-side `result<T, E>` marker; when true
/// the call is threaded through `.map_err(...)?`. The error
/// payload is wrapped in `FunctionError::ExecutionError` so it
/// surfaces to the SQL caller verbatim.
///
/// `sql_name` is used inside the emitted error strings so an
/// arg-type mismatch identifies the function the SQL caller saw.
///
/// `arm_indent` is the literal whitespace prefix the caller wants
/// each emitted line to start with (so the arms align with the
/// surrounding `match name.as_str() { ... }` body).
pub fn emit_scalar_arm_body(
    shape: &DispatchShape,
    fallible: bool,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let mut s = String::new();
    let mut call_args: Vec<String> = Vec::with_capacity(shape.params.len());

    for (idx, p) in shape.params.iter().enumerate() {
        match p {
            ParamShape::Text => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dfv_text(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::F64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dfv_f64(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::S32 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dfv_i64(&args, {idx}, \"{sql_name}\")? as i32;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::S64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dfv_i64(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::U32 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dfv_i64(&args, {idx}, \"{sql_name}\")? as u32;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::U64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dfv_i64(&args, {idx}, \"{sql_name}\")? as u64;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::Bool => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dfv_bool(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::Blob => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dfv_blob(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::OptionNone => {
                // Mirror sqlite-emit / duckdb-emit: pass `None`
                // for an option<T> param the codegen elects to
                // default.
                call_args.push("None".to_string());
            }
            // Step 1 scalar-first cut: shapes below need either a
            // helper that hasn't been defined here yet
            // (Geom/Geog/Raster/Topology need `from_wkb` /
            // `geog_from_wkb` / `from_raster_binary` /
            // `from_topology_bytes` analogs of sqlite-emit's per-
            // resource decoders), or df-plugin-loader integration
            // that isn't ready (WitValueRecord / ListGeom / Enum /
            // ListPrim / ListRecord / ListTuple). Emit a stub
            // return so the bridge still compiles; the function
            // reports as unknown / unsupported at call time.
            other => {
                let shape_dbg = format!("{:?}", other)
                    .replace('"', "\\\"")
                    .replace('{', "{{")
                    .replace('}', "}}");
                return format!(
                    "{i}let _ = args; // suppress unused-warning when arms expand to a single Err\n\
                     {i}return Err(types::FunctionError::ExecutionError(format!(\n\
                     {i}    \"{sql_name}: datafission param shape not yet wired in scalar-first cut ({shape_dbg})\"\n\
                     {i})));\n",
                );
            }
        }
    }

    // Decide the return-wrap shape before emitting the call. Some
    // RetShape variants emit a `return Err(...)` (deferred wit-
    // value shapes etc.); for those we skip the upstream call
    // entirely to avoid an unreachable-code lint.
    let ret_expr_opt = render_ret_to_scalarvalue(&shape.ret);
    let Some(ret_expr) = ret_expr_opt else {
        let shape_dbg = format!("{:?}", &shape.ret)
            .replace('"', "\\\"")
            .replace('{', "{{")
            .replace('}', "}}");
        return format!(
            "{i}let _ = args; // suppress unused-warning\n\
             {i}return Err(types::FunctionError::ExecutionError(format!(\n\
             {i}    \"{sql_name}: datafission return shape not yet wired in scalar-first cut ({shape_dbg})\"\n\
             {i})));\n",
        );
    };

    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let call_args_str = call_args.join(", ");
    // MethodCall: this is a WIT resource method or constructor.
    // The scalar-first cut defers method dispatch (it requires
    // emitting the resource `use` line + per-resource decode
    // helpers like sqlite-emit's `from_topology_bytes`); emit an
    // ExecutionError stub so the bridge still compiles.
    if shape.method_call.is_some() {
        let _ = (module, func, call_args_str, fallible, ret_expr);
        return format!(
            "{i}let _ = args; // suppress unused-warning\n\
             {i}return Err(types::FunctionError::ExecutionError(format!(\n\
             {i}    \"{sql_name}: WIT resource method/constructor dispatch \
not yet wired in scalar-first cut\"\n\
             {i})));\n",
        );
    }
    let call_line = if fallible {
        format!(
            "{i}let __ret = {module}::{func}({call_args_str})\n\
             {i}    .map_err(|e| types::FunctionError::ExecutionError(\n\
             {i}        format!(\"{sql_name}: {{}}\", shim_err_string(e))))?;\n",
        )
    } else {
        format!(
            "{i}let __ret = {module}::{func}({call_args_str});\n",
        )
    };
    s.push_str(&call_line);

    // Wrap the return value into a ScalarValue.
    s.push_str(&format!("{i}Ok({ret_expr})\n"));
    s
}

/// Render the Rust expression that wraps the upstream `__ret`
/// value into a `ScalarValue`. The scalar-first cut handles the
/// primitive returns + a Binary fallback. Shapes that need
/// per-record wit-value marshaling return `None` and the caller
/// emits an `ExecutionError` without making the upstream call (so
/// wit-bindgen doesn't elide the upstream import).
fn render_ret_to_scalarvalue(ret: &RetShape) -> Option<String> {
    match ret {
        RetShape::Text => {
            Some("types::ScalarValue::Utf8(__ret.into())".to_string())
        }
        RetShape::Real => Some("types::ScalarValue::Float64(__ret)".to_string()),
        // The core IR collapses signed integer returns into a
        // single `Int` arm. Datafission's ScalarValue has the full
        // i8/i16/i32/i64 family but we don't know the original WIT
        // width here, so promote to Int64 for parity with the
        // DuckDB target. Future refinement: extend the core IR
        // with an Int<width> arm.
        RetShape::Int => Some("types::ScalarValue::Int64(__ret as i64)".to_string()),
        RetShape::BoolInt => Some("types::ScalarValue::Boolean(__ret)".to_string()),
        RetShape::Blob => Some("types::ScalarValue::Binary(__ret.into())".to_string()),
        // Geometry / Raster / Topology returns currently round-trip
        // through `as_wkb()` / `as_binary()` / `to_bytes()` and emit
        // as Binary. The scalar-first cut treats them all as opaque
        // blobs the SQL surface can pass through to other functions.
        // Once a follow-up adds typed-value-binding support, these
        // become `ScalarValue::Binary` wrapped in a custom-type
        // declared via type-plugin/multi-custom-type — the host's
        // type-id registry resolves them by name.
        RetShape::GeomBlob => {
            Some("types::ScalarValue::Binary(__ret.as_wkb().into())".to_string())
        }
        RetShape::RasterBlob => {
            Some("types::ScalarValue::Binary(__ret.as_binary().into())".to_string())
        }
        RetShape::TopologyBlob => {
            Some("types::ScalarValue::Binary(__ret.to_bytes().into())".to_string())
        }
        RetShape::OptionText => Some(
            "match __ret { Some(v) => types::ScalarValue::Utf8(v.into()), None => types::ScalarValue::Null }".to_string(),
        ),
        RetShape::OptionReal => Some(
            "match __ret { Some(v) => types::ScalarValue::Float64(v), None => types::ScalarValue::Null }".to_string(),
        ),
        RetShape::OptionInt => Some(
            "match __ret { Some(v) => types::ScalarValue::Int64(v as i64), None => types::ScalarValue::Null }".to_string(),
        ),
        RetShape::OptionBoolInt => Some(
            "match __ret { Some(v) => types::ScalarValue::Boolean(v), None => types::ScalarValue::Null }".to_string(),
        ),
        RetShape::OptionBlob => Some(
            "match __ret { Some(v) => types::ScalarValue::Binary(v.into()), None => types::ScalarValue::Null }".to_string(),
        ),
        RetShape::OptionGeomBlob => Some(
            "match __ret { Some(g) => types::ScalarValue::Binary(g.as_wkb().into()), None => types::ScalarValue::Null }".to_string(),
        ),
        // WitValueRecord / Enum / JsonText / TuplePick / etc. —
        // deferred to a follow-up. Returning None tells the caller
        // to emit an ExecutionError instead of making the upstream
        // call.
        _ => None,
    }
}

/// Map a `ParamShape` variant to the Rust source for the
/// `types::LogicalType` value datafission should see at
/// `list-functions` time. The datafission `LogicalType` enum is
/// the canonical primitive set (Boolean / Int8..Int64 / Uint8..Uint64
/// / Float32/64 / Utf8 / Binary / Date / Time / Timestamp). We pick
/// the widest arm for each primitive shape and use `Binary` for
/// custom-typed shapes (Geom / Geog / Raster / Topology).
///
/// Shapes the scalar-first cut hasn't wired into the dispatch arm
/// yet still produce a sensible declaration here so the planner
/// gets the right surface type:
///
///   - `Geom` / `Geog` / `Raster` / `Topology` → `Binary` (WKB /
///     binary / bytes surface; the SQL caller always passes a
///     BINARY column).
///   - `ListGeom` → `Binary` (variadic blob; each row is one
///     geometry blob).
///   - `ListPrim` / `ListRecord` / `ListTuple` → `Utf8` (JSON
///     array literal at the SQL surface).
///   - `Enum` → `Int64` (case index).
///   - `WitValueRecord` → `Binary` (custom-typed blob; the type
///     plugin's type-id resolves it).
///   - `OptionNone` → `Utf8` (placeholder; the dispatch arm
///     ignores this slot).
pub fn paramshape_to_logicaltype(p: &ParamShape) -> String {
    match p {
        ParamShape::Bool => "types::LogicalType::Boolean".to_string(),
        ParamShape::S32
        | ParamShape::S64
        | ParamShape::U32
        | ParamShape::U64 => "types::LogicalType::Int64".to_string(),
        ParamShape::F64 => "types::LogicalType::Float64".to_string(),
        ParamShape::Text => "types::LogicalType::Utf8".to_string(),
        ParamShape::Blob => "types::LogicalType::Binary".to_string(),
        ParamShape::Geom
        | ParamShape::Geog
        | ParamShape::Raster
        | ParamShape::Topology => "types::LogicalType::Binary".to_string(),
        ParamShape::ListGeom => "types::LogicalType::Binary".to_string(),
        ParamShape::ListPrim(_)
        | ParamShape::ListRecord { .. }
        | ParamShape::ListTuple { .. } => "types::LogicalType::Utf8".to_string(),
        ParamShape::Enum { .. } => "types::LogicalType::Int64".to_string(),
        ParamShape::WitValueRecord { .. } => "types::LogicalType::Binary".to_string(),
        ParamShape::OptionNone => "types::LogicalType::Utf8".to_string(),
    }
}

/// Map a `RetShape` variant to the Rust source for the return
/// `types::LogicalType` value datafission should see. Same
/// canonical-primitive-set rules as the param mapping. Shapes the
/// scalar-first cut hasn't yet wired (FirstWitValueRecord,
/// OptionWitValueRecord, TuplePick, etc.) still produce a sensible
/// declaration so the planner sees the right surface type.
pub fn retshape_to_logicaltype(r: &RetShape) -> String {
    match r {
        RetShape::Text => "types::LogicalType::Utf8".to_string(),
        RetShape::Real => "types::LogicalType::Float64".to_string(),
        RetShape::Int => "types::LogicalType::Int64".to_string(),
        RetShape::BoolInt => "types::LogicalType::Boolean".to_string(),
        RetShape::Blob => "types::LogicalType::Binary".to_string(),
        RetShape::GeomBlob
        | RetShape::RasterBlob
        | RetShape::TopologyBlob
        | RetShape::BboxBlob
        | RetShape::FirstGeomBlob
        | RetShape::FirstRasterBlob
        | RetShape::FirstTopologyBlob => "types::LogicalType::Binary".to_string(),
        RetShape::IsValidDetailText => "types::LogicalType::Utf8".to_string(),
        RetShape::OptionText => "types::LogicalType::Utf8".to_string(),
        RetShape::OptionReal => "types::LogicalType::Float64".to_string(),
        RetShape::OptionInt | RetShape::FirstOptionU32Int | RetShape::FirstInt => {
            "types::LogicalType::Int64".to_string()
        }
        RetShape::OptionBoolInt => "types::LogicalType::Boolean".to_string(),
        RetShape::OptionBlob
        | RetShape::OptionGeomBlob
        | RetShape::OptionRasterBlob
        | RetShape::OptionTopologyBlob => "types::LogicalType::Binary".to_string(),
        RetShape::FirstReal => "types::LogicalType::Float64".to_string(),
        RetShape::FirstText => "types::LogicalType::Utf8".to_string(),
        RetShape::Enum { .. } => "types::LogicalType::Int64".to_string(),
        RetShape::JsonText { .. } => "types::LogicalType::Utf8".to_string(),
        RetShape::TuplePick { elem, .. } => match elem {
            datalink_shim_codegen_core::interface_db::ListPrimElem::F64
            | datalink_shim_codegen_core::interface_db::ListPrimElem::F32 => {
                "types::LogicalType::Float64".to_string()
            }
            datalink_shim_codegen_core::interface_db::ListPrimElem::S32
            | datalink_shim_codegen_core::interface_db::ListPrimElem::S64
            | datalink_shim_codegen_core::interface_db::ListPrimElem::U32
            | datalink_shim_codegen_core::interface_db::ListPrimElem::U64
            | datalink_shim_codegen_core::interface_db::ListPrimElem::U8 => {
                "types::LogicalType::Int64".to_string()
            }
            datalink_shim_codegen_core::interface_db::ListPrimElem::Bool => {
                "types::LogicalType::Boolean".to_string()
            }
            datalink_shim_codegen_core::interface_db::ListPrimElem::String => {
                "types::LogicalType::Utf8".to_string()
            }
        },
        RetShape::WitValueRecord { .. }
        | RetShape::OptionWitValueRecord { .. }
        | RetShape::FirstWitValueRecord { .. } => {
            "types::LogicalType::Binary".to_string()
        }
    }
}
