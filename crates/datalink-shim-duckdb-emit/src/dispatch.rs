//! `duckdb:extension`-shape dispatch-arm emitter.
//!
//! Counterpart to `datalink-shim-sqlite-emit/src/dispatch.rs` but
//! over `Duckvalue` (the FROZEN DuckDB variant in
//! `duckdb:extension/types`) instead of `SqlValue`. Consumes the
//! same database-agnostic IR from
//! `datalink_shim_codegen_core::interface_db` (`DispatchEntry`,
//! `DispatchShape`, `ParamShape`, `RetShape`, ...) and renders the
//! Rust source of each match-arm body the dispatch loop calls.
//!
//! ## Scalar-first cut
//!
//! Step 4 of PLAN-shim-codegen-datalink-migration wires only the
//! scalar arms (`call_scalar` / `call_scalar_batch`). Aggregates,
//! UDTFs, pragmas, and casts emit as `Duckerror::Unsupported` so
//! the bridge stays loadable; they are picked up in follow-up
//! steps once a ducklink-loader smoke harness exists.
//!
//! ## Marshaling shape
//!
//! `Duckvalue` differs from `SqlValue` in two structural ways the
//! emit handles inline:
//!
//!   * NULL is its own arm (`Duckvalue::Null`) rather than a
//!     SQLite `SqlValue::Null`; the dispatch loop short-circuits
//!     before any per-arm code runs (DuckDB's default null
//!     propagation matches PostGIS scalar semantics).
//!   * Integer arms split into `Int64` / `Int32` / `Uint64` /
//!     `Uint32` / `Int16` / `Int8` / `Uint16` / `Uint8`. The
//!     scalar-first emit unpacks all of these into `i64` (or `as
//!     i32` / `as u32` / etc.) since the underlying interface DB
//!     declares the function param type and the emit just routes
//!     to the matching upstream WIT arg.

use datalink_shim_codegen_core::interface_db::{
    DispatchShape, ParamShape, RetShape,
};

/// Emit the body of one match arm of `call_scalar`. The body
/// unpacks the `Duckvalue` arg slice into the WIT-side params,
/// calls the upstream function, and wraps the result back into a
/// `Duckvalue` for the dispatch loop to return.
///
/// `fallible` is the WIT-side `result<T, E>` marker; when true
/// the call is threaded through `.map_err(...)?`. The error
/// payload is wrapped in `Duckerror::Invalidargument` so it
/// surfaces to the SQL caller verbatim.
///
/// `sql_name` is used inside the emitted error strings so an
/// arg-type mismatch identifies the function the SQL caller saw.
///
/// `arm_indent` is the literal whitespace prefix the caller wants
/// each emitted line to start with (so the arms align with the
/// surrounding `match handle { ... }` body).
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
                    "{i}let arg{idx} = dv_text(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                // `dv_text` returns `&str` already; pass directly.
                // `.as_str()` on `&str` is an unstable nightly
                // feature (`str_as_str`, rust-lang/rust#130366).
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::F64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dv_f64(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::S32 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dv_i64(&args, {idx}, \"{sql_name}\")? as i32;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::S64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dv_i64(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::U32 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dv_i64(&args, {idx}, \"{sql_name}\")? as u32;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::U64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dv_i64(&args, {idx}, \"{sql_name}\")? as u64;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::Bool => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dv_bool(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::Blob => {
                s.push_str(&format!(
                    "{i}let arg{idx} = dv_blob(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::OptionNone => {
                // Mirror sqlite-emit: pass `None` for an
                // option<T> param the codegen elects to default.
                call_args.push("None".to_string());
            }
            // Step 4 scalar-first cut: shapes below need either a
            // helper that hasn't been defined here yet
            // (Geom/Geog/Raster/Topology need `from_wkb` /
            // `geog_from_wkb` / `from_raster_binary` /
            // `from_topology_bytes` analogs of sqlite-emit's per-
            // resource decoders), or ducklink-loader integration
            // that isn't ready (WitValueRecord / ListGeom / Enum /
            // ListPrim / ListRecord / ListTuple). Emit a stub
            // return so the bridge still compiles; the function
            // reports as unsupported at call time.
            other => {
                let shape_dbg = format!("{:?}", other)
                    .replace('"', "\\\"")
                    .replace('{', "{{")
                    .replace('}', "}}");
                return format!(
                    "{i}let _ = args; // suppress unused-warning when arms expand to a single Err\n\
                     {i}return Err(types::Duckerror::Unsupported(format!(\n\
                     {i}    \"{sql_name}: DuckDB param shape not yet wired in Step 4 cut ({shape_dbg})\"\n\
                     {i})));\n",
                );
            }
        }
    }

    // Decide the return-wrap shape before emitting the call. Some
    // RetShape variants emit a `return Err(...)` (deferred wit-
    // value shapes etc.); for those we skip the upstream call
    // entirely to avoid an unreachable-code lint.
    let ret_expr_opt = render_ret_to_duckvalue(&shape.ret);
    let Some(ret_expr) = ret_expr_opt else {
        let shape_dbg = format!("{:?}", &shape.ret)
            .replace('"', "\\\"")
            .replace('{', "{{")
            .replace('}', "}}");
        return format!(
            "{i}let _ = args; // suppress unused-warning\n\
             {i}return Err(types::Duckerror::Unsupported(format!(\n\
             {i}    \"{sql_name}: DuckDB return shape not yet wired in Step 4 cut ({shape_dbg})\"\n\
             {i})));\n",
        );
    };

    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let call_args_str = call_args.join(", ");
    // MethodCall: this is a WIT resource method or constructor.
    // Step 4 scalar-first cut defers method dispatch (it requires
    // emitting the resource `use` line + per-resource decode
    // helpers like sqlite-emit's `from_topology_bytes`); emit an
    // Unsupported stub so the bridge still compiles.
    if shape.method_call.is_some() {
        let _ = (module, func, call_args_str, fallible, ret_expr);
        return format!(
            "{i}let _ = args; // suppress unused-warning\n\
             {i}return Err(types::Duckerror::Unsupported(format!(\n\
             {i}    \"{sql_name}: WIT resource method/constructor dispatch \
not yet wired in Step 4 cut\"\n\
             {i})));\n",
        );
    }
    let call_line = if fallible {
        format!(
            "{i}let __ret = {module}::{func}({call_args_str})\n\
             {i}    .map_err(|e| types::Duckerror::Invalidargument(\n\
             {i}        format!(\"{sql_name}: {{}}\", shim_err_string(e))))?;\n",
        )
    } else {
        format!(
            "{i}let __ret = {module}::{func}({call_args_str});\n",
        )
    };
    s.push_str(&call_line);

    // Wrap the return value into a Duckvalue.
    s.push_str(&format!("{i}Ok({ret_expr})\n"));
    s
}

/// Render the Rust expression that wraps the upstream `__ret`
/// value into a `Duckvalue`. The scalar-first cut handles the
/// primitive returns + a Blob fallback. Shapes that need
/// per-record wit-value marshaling return `None` and the caller
/// emits an `Unsupported` Duckerror without making the upstream
/// call (so wit-bindgen doesn't elide the upstream import).
fn render_ret_to_duckvalue(ret: &RetShape) -> Option<String> {
    match ret {
        RetShape::Text => {
            Some("types::Duckvalue::Text(__ret.into())".to_string())
        }
        RetShape::Real => Some("types::Duckvalue::Float64(__ret)".to_string()),
        // The core IR collapses signed integer returns into a
        // single `Int` arm that the SQLite emit promotes to i64
        // (SqlValue::Integer is signed-i64-only). DuckDB has a
        // wider integer family but we don't know the original WIT
        // width here, so promote to Int64 for parity. Future
        // refinement: extend the core IR with an Int<width> arm.
        RetShape::Int => Some("types::Duckvalue::Int64(__ret as i64)".to_string()),
        RetShape::BoolInt => Some("types::Duckvalue::Boolean(__ret)".to_string()),
        RetShape::Blob => Some("types::Duckvalue::Blob(__ret.into())".to_string()),
        // Geometry / Raster / Topology returns currently round-trip
        // through `as_wkb()` / `as_binary()` / `to_bytes()` and emit
        // as Blob. The scalar-first cut treats them all as opaque
        // blobs the SQL surface can pass through to other functions.
        // Once a follow-up adds typed-value-binding support, these
        // become `Duckvalue::Complex(...)` entries.
        RetShape::GeomBlob => {
            Some("types::Duckvalue::Blob(__ret.as_wkb().into())".to_string())
        }
        RetShape::RasterBlob => {
            Some("types::Duckvalue::Blob(__ret.as_binary().into())".to_string())
        }
        RetShape::TopologyBlob => {
            Some("types::Duckvalue::Blob(__ret.to_bytes().into())".to_string())
        }
        RetShape::OptionText => Some(
            "match __ret { Some(v) => types::Duckvalue::Text(v.into()), None => types::Duckvalue::Null }".to_string(),
        ),
        RetShape::OptionReal => Some(
            "match __ret { Some(v) => types::Duckvalue::Float64(v), None => types::Duckvalue::Null }".to_string(),
        ),
        RetShape::OptionInt => Some(
            "match __ret { Some(v) => types::Duckvalue::Int64(v as i64), None => types::Duckvalue::Null }".to_string(),
        ),
        RetShape::OptionBoolInt => Some(
            "match __ret { Some(v) => types::Duckvalue::Boolean(v), None => types::Duckvalue::Null }".to_string(),
        ),
        RetShape::OptionBlob => Some(
            "match __ret { Some(v) => types::Duckvalue::Blob(v.into()), None => types::Duckvalue::Null }".to_string(),
        ),
        RetShape::OptionGeomBlob => Some(
            "match __ret { Some(g) => types::Duckvalue::Blob(g.as_wkb().into()), None => types::Duckvalue::Null }".to_string(),
        ),
        // WitValueRecord / Enum / JsonText / TuplePick / etc. —
        // deferred to a follow-up step. Returning None tells the
        // caller to emit an Unsupported Duckerror instead of
        // making the upstream call.
        _ => None,
    }
}
