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
//! ## Coverage
//!
//! All scalar param/return shapes the interface DB surfaces are
//! wired. Aggregate / window / table / type-plugin / spatial-index /
//! system-catalog / index-plugin export traits stub out: their
//! `list-*` methods return empty vectors, their per-call methods
//! return `FunctionError::UnknownFunction` /
//! `SpatialError::UnsupportedOperation` / `TypeError::Internal` /
//! `IndexError::Internal` as appropriate.
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
//!     date / time / timestamp specialisations. The arms unpack
//!     all of these into `i64` (or `as i32` / etc.) since the
//!     underlying interface DB declares the function's param type
//!     and the emit just routes to the matching upstream WIT arg.
//!   * `Utf8(String)` is the text arm; `Binary(list<u8>)` is the
//!     blob arm. Geometry / Raster / Topology returns surface as
//!     `Binary` (PostGIS hands them out as WKB or raw bytes via
//!     `.as_wkb()` / `.as_binary()` / `.to_bytes()` — the SQL
//!     surface treats them as opaque blobs identically to the
//!     SQLite + DuckDB targets).
//!   * WIT-record params/returns marshal through a magic-prefix
//!     Binary envelope: the SQL surface sees a `BINARY` column whose
//!     bytes carry a `b"WTV\x01"` magic header + a 32-byte type_id
//!     (sha256 over the canonical record shape) + a ciborium-encoded
//!     canonical-CBOR payload. The type_id is baked into the
//!     per-record helper at codegen time; mismatches surface as
//!     `ExecutionError`.

use datalink_shim_codegen_core::interface_db::{
    list_tuple_mixed_sig_suffix, AccKind, AggregateShape, ColumnAffinity,
    DispatchShape, JsonRetKind, ListPrimElem, ParamShape, RetShape,
    ScalarReturnKind, UdtfFieldShape, UdtfOutputRow, UdtfShape,
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
            ParamShape::Geom => {
                s.push_str(&format!(
                    "{i}let arg{idx} = from_wkb(dfv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Geog => {
                s.push_str(&format!(
                    "{i}let arg{idx} = geog_from_wkb(dfv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Raster => {
                s.push_str(&format!(
                    "{i}let arg{idx} = from_raster_binary(dfv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Topology => {
                s.push_str(&format!(
                    "{i}let arg{idx} = from_topology_bytes(dfv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::ListGeom => {
                // `list<borrow<geometry>>` — variadic or single.
                // Variadic when this is the LAST param (e.g.
                // `st_collect(g1, g2, ...)`); otherwise wrap a
                // single blob in a 1-element list.
                let is_variadic = idx + 1 == shape.params.len();
                if is_variadic {
                    s.push_str(&format!(
                        "{i}let arg{idx}_owned: Vec<Geometry> = args[{idx}..]\n\
                         {i}    .iter()\n\
                         {i}    .enumerate()\n\
                         {i}    .map(|(j, v)| match v {{\n\
                         {i}        ftypes::ScalarValue::Binary(b) => Geometry::from_wkb(b.as_slice())\n\
                         {i}            .map_err(|e| types::FunctionError::ExecutionError(format!(\"{sql_name}: arg {{}}: {{}}\", {idx} + j, postgis_err_string(e)))),\n\
                         {i}        ftypes::ScalarValue::Utf8(t) => Geometry::from_wkb(t.as_bytes())\n\
                         {i}            .map_err(|e| types::FunctionError::ExecutionError(format!(\"{sql_name}: arg {{}}: {{}}\", {idx} + j, postgis_err_string(e)))),\n\
                         {i}        _ => Err(types::FunctionError::ExecutionError(format!(\"{sql_name}: arg {{}} must be BINARY\", {idx} + j))),\n\
                         {i}    }})\n\
                         {i}    .collect::<Result<Vec<_>, _>>()?;\n\
                         {i}let arg{idx}: Vec<&Geometry> = arg{idx}_owned.iter().collect();\n",
                    ));
                } else {
                    s.push_str(&format!(
                        "{i}let arg{idx}_one = from_wkb(dfv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n\
                         {i}let arg{idx}_owned: Vec<Geometry> = alloc::vec![arg{idx}_one];\n\
                         {i}let arg{idx}: Vec<&Geometry> = arg{idx}_owned.iter().collect();\n",
                    ));
                }
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Enum {
                wit_module,
                kebab_name,
                cases,
                ..
            } => {
                let type_pascal = kebab_to_pascal(kebab_name);
                let mut arms = String::new();
                for (n, case) in cases.iter().enumerate() {
                    let case_pascal = kebab_to_pascal(case);
                    arms.push_str(&format!(
                        "{i}    {n} => {wit_module}::{type_pascal}::{case_pascal},\n"
                    ));
                }
                let max = cases.len();
                s.push_str(&format!(
                    "{i}let arg{idx} = match dfv_i64(&args, {idx}, \"{sql_name}\")? {{\n{arms}{i}    other => return Err(types::FunctionError::ExecutionError(format!(\n\
                     {i}        \"{sql_name}: arg {idx} ({kebab_name}) out of range: {{}} (valid 0..{max})\",\n\
                     {i}        other,\n\
                     {i}    ))),\n\
                     {i}}};\n"
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::ListRecord { helper_snake, .. } => {
                // #709: `helper_snake` disambiguates cross-interface
                // kebab collisions (e.g. mobilitydb `stbox3d`).
                s.push_str(&format!(
                    "{i}let arg{idx} = parse_json_list_record_{helper_snake}(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::ListPrim(elem) => {
                let suffix = elem.helper_suffix();
                let rust_ty = elem.rust_elem();
                s.push_str(&format!(
                    "{i}let arg{idx}: Vec<{rust_ty}> = parse_json_list_{suffix}(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::ListTuple { elements } => {
                let suffix = list_tuple_sig_suffix(elements);
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
                s.push_str(&format!(
                    "{i}let arg{idx}: Vec<{rust_tuple}> = parse_json_list_tuple_{suffix}(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::ListTupleMixed { elements } => {
                // #724: `list<tuple<E1, E2, ...>>` param where at
                // least one Ei is a same-shim record. Today's
                // surface (mobilitydb): `tfloat-batch-to-parquet`
                // — `list<tuple<string, tfloat-sequence>>`. The
                // per-signature helper handles the fully-qualified
                // `Vec<(String, UPSTREAM_R)>` type; the arm just
                // calls it by suffix.
                let suffix = list_tuple_mixed_sig_suffix(elements);
                s.push_str(&format!(
                    "{i}let arg{idx} = parse_json_list_tuple_{suffix}(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::ListListU8 => {
                // #674: `list<list<u8>>` — batched WKB blobs for the
                // postgis `st_*_batch` family. SQL passes JSON text
                // matching `Vec<Vec<u8>>` (nested arrays of byte
                // integers); decoded via the
                // `parse_json_list_list_u8` helper. Param is passed
                // by reference to match wit-bindgen's
                // `&[Vec<u8>]` binding.
                s.push_str(&format!(
                    "{i}let arg{idx}: Vec<Vec<u8>> = parse_json_list_list_u8(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::ListListPrim(elem) => {
                // #695: `list<list<X>>` over a primitive non-u8
                // element. Mirrors `ListListU8` with the per-element
                // helper name and Rust inner type. Today's surface:
                // postgis raster `st-set-values` + flatgeobuf
                // coord-list constructors.
                let suffix = elem.helper_suffix();
                let rust_ty = elem.rust_elem();
                s.push_str(&format!(
                    "{i}let arg{idx}: Vec<Vec<{rust_ty}>> = parse_json_list_list_{suffix}(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::WitValueRecord {
                helper_snake,
                upstream_by_value,
                ..
            } => {
                // Magic-prefix Binary scheme (4-byte WTV magic +
                // 32-byte type_id + canonical-CBOR payload). The
                // per-record `arg_witvalue_<helper_snake>` helper
                // checks the magic + type_id and ciborium-decodes
                // the payload directly into the upstream record
                // type. #709: `helper_snake` is pre-computed by the
                // classifier — disambiguates cross-interface kebab
                // collisions (mobilitydb `stbox3d`).
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_witvalue_{helper_snake}(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                if *upstream_by_value {
                    call_args.push(format!("arg{idx}"));
                } else {
                    call_args.push(format!("&arg{idx}"));
                }
            }
        }
    }

    let call_args_str = call_args.join(", ");
    let module = &shape.wit_module;
    let func = &shape.wit_func;

    // Method-call composition: WIT resource methods + constructors.
    // Mirror sqlite-emit's pattern.
    let call_expr = if let Some(mc) = shape.method_call.as_ref() {
        if mc.is_constructor {
            let pascal = kebab_to_pascal(&mc.resource_kebab);
            format!("{pascal}::new({call_args_str})")
        } else {
            // call_args[0] is the receiver as `&argN`; method-call form
            // drops the leading `&` and reuses the same arg ident.
            let recv = call_args
                .first()
                .map(|s| s.trim_start_matches('&').to_string())
                .unwrap_or_else(|| "arg0".to_string());
            let rest = call_args
                .iter()
                .skip(1)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            format!("{recv}.{func}({rest})")
        }
    } else {
        format!("{module}::{func}({call_args_str})")
    };

    let unwrap_chain = if fallible {
        format!(
            ".map_err(|e| types::FunctionError::ExecutionError(\
                format!(\"{sql_name}: {{}}\", shim_err_string(e))))?"
        )
    } else {
        String::new()
    };

    let return_expr = render_ret_to_scalarvalue(
        &shape.ret,
        &call_expr,
        &unwrap_chain,
        sql_name,
        i,
    );
    s.push_str(i);
    s.push_str(&return_expr);
    s.push('\n');
    s
}

/// Render the Rust expression that wraps the upstream call into a
/// `Result<ScalarValue, FunctionError>` return for the dispatch arm.
/// `call_expr` is the upstream-call expression (e.g.
/// `pg_acc::st_area(arg0)`), `unwrap_chain` is either empty or
/// `.map_err(...)?` for fallible calls.
fn render_ret_to_scalarvalue(
    ret: &RetShape,
    call_expr: &str,
    unwrap_chain: &str,
    sql_name: &str,
    i: &str,
) -> String {
    match ret {
        RetShape::Text => format!(
            "Ok(types::ScalarValue::Utf8(({call_expr}{unwrap_chain}).into()))"
        ),
        RetShape::Real => format!(
            "Ok(types::ScalarValue::Float64({call_expr}{unwrap_chain}))"
        ),
        // The core IR collapses signed integer returns into a
        // single `Int` arm. Datafission's ScalarValue has the full
        // i8/i16/i32/i64 family but we don't know the original WIT
        // width here, so promote to Int64.
        RetShape::Int => format!(
            "Ok(types::ScalarValue::Int64(({call_expr}{unwrap_chain}) as i64))"
        ),
        RetShape::BoolInt => format!(
            "Ok(types::ScalarValue::Boolean({call_expr}{unwrap_chain}))"
        ),
        RetShape::Blob => format!(
            "Ok(types::ScalarValue::Binary(({call_expr}{unwrap_chain}).into()))"
        ),
        RetShape::GeomBlob => {
            if !unwrap_chain.is_empty() {
                format!(
                    "{{\n\
                     {i}    let __r = {call_expr}{unwrap_chain};\n\
                     {i}    Ok(types::ScalarValue::Binary(__r.as_wkb().into()))\n\
                     {i}}}"
                )
            } else {
                format!("Ok(types::ScalarValue::Binary({call_expr}.as_wkb().into()))")
            }
        }
        RetShape::RasterBlob => {
            if !unwrap_chain.is_empty() {
                format!(
                    "{{\n\
                     {i}    let __r = {call_expr}{unwrap_chain};\n\
                     {i}    Ok(types::ScalarValue::Binary(__r.as_binary().into()))\n\
                     {i}}}"
                )
            } else {
                format!("Ok(types::ScalarValue::Binary({call_expr}.as_binary().into()))")
            }
        }
        RetShape::TopologyBlob => {
            if !unwrap_chain.is_empty() {
                format!(
                    "{{\n\
                     {i}    let __r = {call_expr}{unwrap_chain};\n\
                     {i}    Ok(types::ScalarValue::Binary(__r.to_bytes().into()))\n\
                     {i}}}"
                )
            } else {
                format!("Ok(types::ScalarValue::Binary({call_expr}.to_bytes().into()))")
            }
        }
        // #707: topo-geometry result — route through `geometry()` +
        // `as-wkb()` (the topo-geometry resource has no direct
        // `to-bytes` method). Covers `create_topo_geom` /
        // `topology_create_topo_geom`.
        RetShape::TopoGeometryViaGeom => {
            if !unwrap_chain.is_empty() {
                format!(
                    "{{\n\
                     {i}    let __r = {call_expr}{unwrap_chain};\n\
                     {i}    Ok(types::ScalarValue::Binary(__r.geometry().as_wkb().into()))\n\
                     {i}}}"
                )
            } else {
                format!(
                    "Ok(types::ScalarValue::Binary({call_expr}.geometry().as_wkb().into()))"
                )
            }
        }
        RetShape::OptionText => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::ScalarValue::Utf8(v.into()),\n\
             {i}    None => types::ScalarValue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionReal => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::ScalarValue::Float64(v as f64),\n\
             {i}    None => types::ScalarValue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionInt => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::ScalarValue::Int64(v as i64),\n\
             {i}    None => types::ScalarValue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionBoolInt => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::ScalarValue::Boolean(v),\n\
             {i}    None => types::ScalarValue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::ScalarValue::Binary(v.into()),\n\
             {i}    None => types::ScalarValue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionGeomBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(g) => types::ScalarValue::Binary(g.as_wkb().into()),\n\
             {i}    None => types::ScalarValue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionRasterBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::ScalarValue::Binary(v.as_binary().into()),\n\
             {i}    None => types::ScalarValue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionTopologyBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::ScalarValue::Binary(v.to_bytes().into()),\n\
             {i}    None => types::ScalarValue::Null,\n\
             {i}}})"
        ),
        RetShape::FirstGeomBlob => format!(
            "{{\n\
             {i}    let __r: Vec<Geometry> = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(g) => Ok(types::ScalarValue::Binary(g.as_wkb().into())),\n\
             {i}        None => Ok(types::ScalarValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstRasterBlob => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.into_iter().next() {{\n\
             {i}        Some(r) => Ok(types::ScalarValue::Binary(r.as_binary().into())),\n\
             {i}        None => Ok(types::ScalarValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstTopologyBlob => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.into_iter().next() {{\n\
             {i}        Some(t) => Ok(types::ScalarValue::Binary(t.to_bytes().into())),\n\
             {i}        None => Ok(types::ScalarValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstOptionU32Int => format!(
            "{{\n\
             {i}    let __r: Vec<Option<u32>> = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(Some(v)) => Ok(types::ScalarValue::Uint32(*v)),\n\
             {i}        Some(None) | None => Ok(types::ScalarValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstInt => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(v) => Ok(types::ScalarValue::Int64(*v as i64)),\n\
             {i}        None => Ok(types::ScalarValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstReal => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(v) => Ok(types::ScalarValue::Float64(*v as f64)),\n\
             {i}        None => Ok(types::ScalarValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstText => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.into_iter().next() {{\n\
             {i}        Some(v) => Ok(types::ScalarValue::Utf8(v)),\n\
             {i}        None => Ok(types::ScalarValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::BboxBlob => format!(
            "{{\n\
             {i}    let __bb = {call_expr}{unwrap_chain};\n\
             {i}    let __env = pg_ctor::st_make_envelope(__bb.min_x, __bb.min_y, __bb.max_x, __bb.max_y);\n\
             {i}    Ok(types::ScalarValue::Binary(__env.as_wkb().into()))\n\
             {i}}}"
        ),
        RetShape::IsValidDetailText => format!(
            "{{\n\
             {i}    let (__valid, __reason, __loc) = {call_expr}{unwrap_chain};\n\
             {i}    let __reason_s = __reason.unwrap_or_default();\n\
             {i}    let __loc_s = match __loc {{\n\
             {i}        Some(g) => pg_out::st_as_text(&g),\n\
             {i}        None => alloc::string::String::new(),\n\
             {i}    }};\n\
             {i}    Ok(types::ScalarValue::Utf8(format!(\n\
             {i}        \"({{}},\\\"{{}}\\\",\\\"{{}}\\\")\",\n\
             {i}        __valid, __reason_s, __loc_s\n\
             {i}    )))\n\
             {i}}}"
        ),
        // Gap G3 (#668): bbox3d return — ISO-WKB `LINESTRING Z`
        // blob whose two vertices are the bbox's min and max
        // corners `(xmin, ymin, zmin) -> (xmax, ymax, zmax)`. The
        // diagonal preserves all six coordinates and is parseable
        // by downstream WKB consumers. The bbox3d record's
        // wit-bindgen Rust shape is `{ min_x, min_y, min_z,
        // max_x, max_y, max_z }` (six f64).
        RetShape::Bbox3dWkbLineZ => format!(
            "{{\n\
             {i}    let __bb = {call_expr}{unwrap_chain};\n\
             {i}    let mut __wkb: Vec<u8> = Vec::with_capacity(57);\n\
             {i}    // ISO-WKB header: little-endian, type 1002 (LINESTRING Z), 2 points.\n\
             {i}    __wkb.push(0x01u8);\n\
             {i}    __wkb.extend_from_slice(&1002u32.to_le_bytes());\n\
             {i}    __wkb.extend_from_slice(&2u32.to_le_bytes());\n\
             {i}    __wkb.extend_from_slice(&__bb.min_x.to_le_bytes());\n\
             {i}    __wkb.extend_from_slice(&__bb.min_y.to_le_bytes());\n\
             {i}    __wkb.extend_from_slice(&__bb.min_z.to_le_bytes());\n\
             {i}    __wkb.extend_from_slice(&__bb.max_x.to_le_bytes());\n\
             {i}    __wkb.extend_from_slice(&__bb.max_y.to_le_bytes());\n\
             {i}    __wkb.extend_from_slice(&__bb.max_z.to_le_bytes());\n\
             {i}    Ok(types::ScalarValue::Binary(__wkb.into()))\n\
             {i}}}"
        ),
        RetShape::Enum {
            wit_module,
            kebab_name,
            cases,
            ..
        } => {
            let type_pascal = kebab_to_pascal(kebab_name);
            let mut arms = String::new();
            for (n, case) in cases.iter().enumerate() {
                let case_pascal = kebab_to_pascal(case);
                arms.push_str(&format!(
                    "{i}        {wit_module}::{type_pascal}::{case_pascal} => {n},\n"
                ));
            }
            format!(
                "{{\n\
                 {i}    let __r = {call_expr}{unwrap_chain};\n\
                 {i}    let __disc: i64 = match __r {{\n{arms}{i}    }};\n\
                 {i}    Ok(types::ScalarValue::Int64(__disc))\n\
                 {i}}}"
            )
        }
        // #716: option<enum> — Some → integer discriminant; None → Null.
        RetShape::OptionEnum {
            wit_module,
            kebab_name,
            cases,
            ..
        } => {
            let type_pascal = kebab_to_pascal(kebab_name);
            let mut arms = String::new();
            for (n, case) in cases.iter().enumerate() {
                let case_pascal = kebab_to_pascal(case);
                arms.push_str(&format!(
                    "{i}            {wit_module}::{type_pascal}::{case_pascal} => {n},\n"
                ));
            }
            format!(
                "{{\n\
                 {i}    match {call_expr}{unwrap_chain} {{\n\
                 {i}        Some(__v) => {{\n\
                 {i}            let __disc: i64 = match __v {{\n{arms}{i}            }};\n\
                 {i}            Ok(types::ScalarValue::Int64(__disc))\n\
                 {i}        }}\n\
                 {i}        None => Ok(types::ScalarValue::Null),\n\
                 {i}    }}\n\
                 {i}}}"
            )
        }
        RetShape::JsonText { kind } => match kind {
            // #724: `ListTupleMixed` uses the same
            // `serde_json::to_string(&__r)` template — the
            // upstream `Vec<(String, UPSTREAM_R)>` serializes
            // via the record's wit-bindgen `Serialize` derive.
            JsonRetKind::ListListPrim(_)
            | JsonRetKind::ListTuplePrim(_)
            | JsonRetKind::ListTupleMixed(_)
            | JsonRetKind::TuplePrim(_) => format!(
                "{{\n\
                 {i}    let __r = {call_expr}{unwrap_chain};\n\
                 {i}    let __json = serde_json::to_string(&__r)\n\
                 {i}        .map_err(|e| types::FunctionError::ExecutionError(\n\
                 {i}            format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
                 {i}    Ok(types::ScalarValue::Utf8(__json))\n\
                 {i}}}"
            ),
            JsonRetKind::OptionTuplePrim(_) => format!(
                "{{\n\
                 {i}    match {call_expr}{unwrap_chain} {{\n\
                 {i}        Some(__t) => {{\n\
                 {i}            let __json = serde_json::to_string(&__t)\n\
                 {i}                .map_err(|e| types::FunctionError::ExecutionError(\n\
                 {i}                    format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
                 {i}            Ok(types::ScalarValue::Utf8(__json))\n\
                 {i}        }}\n\
                 {i}        None => Ok(types::ScalarValue::Null),\n\
                 {i}    }}\n\
                 {i}}}"
            ),
            // #630: `option<list<R>>` for an all-primitive record R
            // — Some(vec) → JSON array of objects via serde; None →
            // Datafission NULL. Today's surface: mobilitydb
            // `<date|float|int|tstz>-spanset-from-text`.
            //
            // #716: same template covers `OptionListPrim` (`Vec<X>`),
            // `OptionListTuplePrim` (`Vec<(X1,X2,...)>`), and
            // `OptionTuplePrimOrOptPrim` (tuple with `Option<X>` fields
            // — serde renders `None` as JSON `null`).
            JsonRetKind::OptionListPrimRecord(_)
            | JsonRetKind::OptionListPrim(_)
            | JsonRetKind::OptionListTuplePrim(_)
            | JsonRetKind::OptionTuplePrimOrOptPrim(_) => format!(
                "{{\n\
                 {i}    match {call_expr}{unwrap_chain} {{\n\
                 {i}        Some(__v) => {{\n\
                 {i}            let __json = serde_json::to_string(&__v)\n\
                 {i}                .map_err(|e| types::FunctionError::ExecutionError(\n\
                 {i}                    format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
                 {i}            Ok(types::ScalarValue::Utf8(__json))\n\
                 {i}        }}\n\
                 {i}        None => Ok(types::ScalarValue::Null),\n\
                 {i}    }}\n\
                 {i}}}"
            ),
            JsonRetKind::ListTupleGeomF64 => format!(
                "{{\n\
                 {i}    let __r = {call_expr}{unwrap_chain};\n\
                 {i}    let mut __out = alloc::string::String::from(\"[\");\n\
                 {i}    for (__i, (__g, __v)) in __r.into_iter().enumerate() {{\n\
                 {i}        if __i > 0 {{ __out.push(','); }}\n\
                 {i}        let __wkb = __g.as_wkb();\n\
                 {i}        __out.push_str(\"[\\\"\");\n\
                 {i}        for __b in __wkb {{\n\
                 {i}            use core::fmt::Write as _;\n\
                 {i}            let _ = write!(&mut __out, \"{{:02x}}\", __b);\n\
                 {i}        }}\n\
                 {i}        __out.push_str(\"\\\",\");\n\
                 {i}        let __vj = serde_json::to_string(&__v)\n\
                 {i}            .map_err(|e| types::FunctionError::ExecutionError(\n\
                 {i}                format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
                 {i}        __out.push_str(&__vj);\n\
                 {i}        __out.push(']');\n\
                 {i}    }}\n\
                 {i}    __out.push(']');\n\
                 {i}    Ok(types::ScalarValue::Utf8(__out))\n\
                 {i}}}"
            ),
        },
        RetShape::TuplePick { index, elem } => {
            let (variant, expr_suffix) = match elem {
                ListPrimElem::S32
                | ListPrimElem::S64
                | ListPrimElem::U32
                | ListPrimElem::U64
                | ListPrimElem::U8
                | ListPrimElem::Bool => ("Int64", format!("__r.{index} as i64")),
                ListPrimElem::F64 | ListPrimElem::F32 => {
                    ("Float64", format!("__r.{index} as f64"))
                }
                ListPrimElem::String => ("Utf8", format!("__r.{index}")),
            };
            format!(
                "{{\n\
                 {i}    let __r = {call_expr}{unwrap_chain};\n\
                 {i}    Ok(types::ScalarValue::{variant}({expr_suffix}))\n\
                 {i}}}"
            )
        }
        RetShape::WitValueRecord { helper_snake, .. } => {
            format!("ret_to_witvalue_{helper_snake}({call_expr}{unwrap_chain})")
        }
        RetShape::OptionWitValueRecord { helper_snake, .. } => {
            format!(
                "match {call_expr}{unwrap_chain} {{\n\
                 {i}    Some(__rec) => ret_to_witvalue_{helper_snake}(__rec),\n\
                 {i}    None => Ok(types::ScalarValue::Null),\n\
                 {i}}}"
            )
        }
        RetShape::FirstWitValueRecord { helper_snake, .. } => {
            format!(
                "{{\n\
                 {i}    let __r = {call_expr}{unwrap_chain};\n\
                 {i}    let mut __it = __r.into_iter();\n\
                 {i}    match __it.next() {{\n\
                 {i}        Some(__rec) => ret_to_witvalue_{helper_snake}(__rec),\n\
                 {i}        None => Ok(types::ScalarValue::Null),\n\
                 {i}    }}\n\
                 {i}}}"
            )
        }
        // #677: `list<bool>` / `list<list<u8>>` batch returns —
        // serde-encode the upstream `Vec<bool>` / `Vec<Vec<u8>>`
        // directly to a JSON array text. Symmetric with the
        // param-side `ParamShape::ListListU8` convention.
        RetShape::ListBool | RetShape::ListListU8 => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    let __json = serde_json::to_string(&__r)\n\
             {i}        .map_err(|e| types::FunctionError::ExecutionError(\n\
             {i}            format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
             {i}    Ok(types::ScalarValue::Utf8(__json))\n\
             {i}}}"
        ),
        // #690: `result<_, E>` (unit OK) — discard the `()` value
        // and yield SQL NULL. `unwrap_chain` carries the
        // `.map_err(...)?` clause for fallible calls and is empty
        // otherwise, so the single shape covers both paths.
        RetShape::Unit => format!(
            "let _ = {call_expr}{unwrap_chain};\n{i}Ok(types::ScalarValue::Null)"
        ),
    }
}

/// Map a `ParamShape` variant to the Rust source for the
/// `types::LogicalType` value datafission should see at
/// `list-functions` time. The datafission `LogicalType` enum is
/// the canonical primitive set (Boolean / Int8..Int64 / Uint8..Uint64
/// / Float32/64 / Utf8 / Binary / Date / Time / Timestamp). We pick
/// the widest arm for each primitive shape and use `Binary` for
/// custom-typed shapes (Geom / Geog / Raster / Topology /
/// WitValueRecord — the magic-prefix Binary envelope makes the SQL
/// surface uniform).
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
        | ParamShape::ListTuple { .. }
                | ParamShape::ListTupleMixed { .. }
        | ParamShape::ListListU8
        | ParamShape::ListListPrim(_) => "types::LogicalType::Utf8".to_string(),
        ParamShape::Enum { .. } => "types::LogicalType::Int64".to_string(),
        ParamShape::WitValueRecord { .. } => "types::LogicalType::Binary".to_string(),
        ParamShape::OptionNone => "types::LogicalType::Utf8".to_string(),
    }
}

/// Map a `RetShape` variant to the Rust source for the return
/// `types::LogicalType` value datafission should see. Same
/// canonical-primitive-set rules as the param mapping.
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
        | RetShape::TopoGeometryViaGeom
        | RetShape::BboxBlob
        | RetShape::FirstGeomBlob
        | RetShape::FirstRasterBlob
        | RetShape::FirstTopologyBlob => "types::LogicalType::Binary".to_string(),
        RetShape::IsValidDetailText => "types::LogicalType::Utf8".to_string(),
        // Gap G3 (#668): bbox3d returns surface as an ISO-WKB
        // `LINESTRING Z` blob (min-corner -> max-corner diagonal).
        RetShape::Bbox3dWkbLineZ => "types::LogicalType::Binary".to_string(),
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
        RetShape::Enum { .. } | RetShape::OptionEnum { .. } => "types::LogicalType::Int64".to_string(),
        RetShape::JsonText { .. } => "types::LogicalType::Utf8".to_string(),
        // #677: `list<bool>` / `list<list<u8>>` batch returns
        // rendered as JSON text (symmetric with the param-side
        // ListListU8 convention).
        RetShape::ListBool | RetShape::ListListU8 => "types::LogicalType::Utf8".to_string(),
        RetShape::TuplePick { elem, .. } => match elem {
            ListPrimElem::F64 | ListPrimElem::F32 => "types::LogicalType::Float64".to_string(),
            ListPrimElem::S32
            | ListPrimElem::S64
            | ListPrimElem::U32
            | ListPrimElem::U64
            | ListPrimElem::U8 => "types::LogicalType::Int64".to_string(),
            ListPrimElem::Bool => "types::LogicalType::Boolean".to_string(),
            ListPrimElem::String => "types::LogicalType::Utf8".to_string(),
        },
        RetShape::WitValueRecord { .. }
        | RetShape::OptionWitValueRecord { .. }
        | RetShape::FirstWitValueRecord { .. } => {
            "types::LogicalType::Binary".to_string()
        }
        // #690: `result<_, E>` mutator returns surface as SQL NULL;
        // pick the widest neutral logical type so the advertised
        // surface stays compatible with NULL marshalling.
        RetShape::Unit => "types::LogicalType::Binary".to_string(),
    }
}

/// kebab-case → PascalCase for enum-type and -case names. wit-bindgen's
/// generator does the same conversion when emitting Rust enum idents,
/// so the dispatch arm references `<module>::PixelType::Bool1` etc.
/// consistently with the generated bindings.
fn kebab_to_pascal(s: &str) -> String {
    let mut out = String::new();
    let mut up = true;
    for c in s.chars() {
        if c == '-' || c == '_' {
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

/// W2 Phase 2 mop-up (#555): produce the snake-case suffix used by
/// the per-signature `parse_json_list_tuple_<suffix>` helper.
pub fn list_tuple_sig_suffix(elements: &[ListPrimElem]) -> String {
    elements
        .iter()
        .map(|e| e.helper_suffix())
        .collect::<Vec<_>>()
        .join("_")
}

// ─── Aggregate finalize dispatch ───
//
// Datafission's `aggregate_function_registry@1.0.0` contract is
// handle-based, with a per-handle accumulator state on the guest
// side:
//
//   create-accumulator(name)               -> u64                (init)
//   accumulate(handle, value: scalar-value) -> ()                (per-row)
//   merge(target, source)                  -> ()                 (combine)
//   finalize(handle)                       -> scalar-value       (emit)
//   reset(handle), destroy-accumulator(handle)
//
// The bridge maintains a global `BTreeMap<u64, AccState>` where
// `AccState { arm_idx, blobs, extras }` carries the accumulator
// arm-index, the streaming blobs collected via `accumulate`, and
// any constant configs passed at `create-accumulator-with-configs`
// time. `finalize` then dispatches by arm-index to the per-arm
// body emitted here.
//
// Unlike the SQLite per-row step (which holds raw blobs in a
// thread_local map keyed by context-id) the Datafission shape
// holds them in the accumulator struct directly — handles are the
// host's identity for the group.
//
// The per-arm body returned here is what goes inside the
// `<arm_idx>usize => { ... }` finalize match. It expects bindings
// `st: AccState` in scope (the popped accumulator).
pub fn emit_aggregate_finalize_body(
    shape: &AggregateShape,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let mut s = String::new();

    // #607 Phase 2: AccKind::Record aggregates (mobilitydb
    // temporal-type aggregators) carry per-row WTV-framed record
    // payloads in `st.blobs` (the per-shape-arms WitValueRecord
    // arm path encodes `ScalarValue::Binary` with the WTV magic
    // prefix + 32-byte type-id + ciborium CBOR, and `accumulate`
    // pushes those raw bytes verbatim into the accumulator state).
    // Route to a dedicated emitter that decodes each blob via the
    // per-record `arg_witvalue_<snake>` helper, runs the upstream
    // aggregator, and frames the result back via the matching
    // `ret_to_witvalue_<snake>` encoder.
    if let AccKind::Record { .. } = &shape.accumulator_kind {
        return emit_aggregate_finalize_body_record(shape, sql_name, arm_indent);
    }
    // #614: RecordToScalar — same record-decode pattern as
    // AccKind::Record on the input side, primitive-scalar wrap on
    // the output side. Dedicated emitter so the Geom/Raster
    // scaffolding below stays clean.
    if let AccKind::RecordToScalar { .. } = &shape.accumulator_kind {
        return emit_aggregate_finalize_body_record_to_scalar(
            shape, sql_name, arm_indent,
        );
    }
    // #640: RecordToTuple — same record-decode input side; output
    // side serialises the upstream Rust tuple to JSON-array text
    // and wraps it in ScalarValue::Utf8 (None → ScalarValue::Null
    // when optional). Today's surface: mobilitydb
    // `tint-range-aggregate`.
    if let AccKind::RecordToTuple { .. } = &shape.accumulator_kind {
        return emit_aggregate_finalize_body_record_to_tuple(
            shape, sql_name, arm_indent,
        );
    }

    // Decode accumulated blobs into a typed Vec<Resource>; build
    // refs slice for the upstream call.
    match &shape.accumulator_kind {
        AccKind::Geom => {
            s.push_str(&format!(
                "{i}let geoms: Vec<Geometry> = st.blobs.iter()\n\
                 {i}    .map(|b| Geometry::from_wkb(b))\n\
                 {i}    .collect::<Result<Vec<_>, _>>()\n\
                 {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                 {i}        format!(\"{sql_name}: {{}}\", postgis_err_string(e))))?;\n\
                 {i}let refs: Vec<&Geometry> = geoms.iter().collect();\n",
            ));
        }
        AccKind::Raster => {
            // from_raster_binary returns Result<Raster, FunctionError>;
            // it already wraps the error so we propagate with `?`.
            s.push_str(&format!(
                "{i}let rasters: Vec<Raster> = st.blobs.iter()\n\
                 {i}    .map(|b| from_raster_binary(b.as_slice(), \"{sql_name}\"))\n\
                 {i}    .collect::<Result<Vec<_>, _>>()?;\n\
                 {i}let refs: Vec<&Raster> = rasters.iter().collect();\n",
            ));
        }
        AccKind::Record { .. } => unreachable!("handled by Phase 2 stub above"),
        AccKind::RecordToScalar { .. } => {
            unreachable!("handled by #614 RecordToScalar stub above")
        }
        AccKind::RecordToTuple { .. } => {
            unreachable!("handled by #640 RecordToTuple stub above")
        }
    }

    // Marshal extras (configs passed at create-accumulator-with-
    // configs time) into Rust-typed bindings. Each config is a
    // JSON-encoded string; the host JSON-encodes constant arg slots
    // when calling the constructor.
    let mut call_extras: Vec<String> = Vec::new();
    if !shape.extra_args.is_empty() {
        for (j, p) in shape.extra_args.iter().enumerate() {
            let getc = format!(
                "{i}let extra{j}_str = st.extras.get({j})\n\
                 {i}    .ok_or_else(|| ftypes::FunctionError::ExecutionError(\n\
                 {i}        format!(\"{sql_name}: missing config arg #{j}\")))?;\n",
            );
            match p {
                ParamShape::Text => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: &str = extra{j}_str.as_str();\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::F64 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: f64 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S32 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: i32 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S64 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: i64 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U32 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: u32 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U64 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: u64 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Bool => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: bool = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::OptionNone => {
                    call_extras.push("None".to_string());
                }
                ParamShape::Blob
                | ParamShape::Geom
                | ParamShape::Geog
                | ParamShape::Raster
                | ParamShape::Topology
                | ParamShape::ListGeom
                | ParamShape::WitValueRecord { .. }
                | ParamShape::Enum { .. }
                | ParamShape::ListPrim(_)
                | ParamShape::ListRecord { .. }
                | ParamShape::ListTuple { .. }
                | ParamShape::ListTupleMixed { .. }
                | ParamShape::ListListU8
                | ParamShape::ListListPrim(_) => {
                    return format!(
                        "{i}Err(ftypes::FunctionError::ExecutionError(\
                         format!(\"{sql_name}: aggregate config arg #{j} shape not wired\")))",
                    );
                }
            }
        }
    }

    let call_args = if call_extras.is_empty() {
        "&refs".to_string()
    } else {
        format!("&refs, {}", call_extras.join(", "))
    };

    match shape.ret {
        RetShape::GeomBlob => {
            s.push_str(&format!(
                "{i}let r = {module}::{func}({call_args})\n\
                 {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                 {i}        format!(\"{sql_name}: {{}}\", postgis_err_string(e))))?;\n\
                 {i}Ok(ftypes::ScalarValue::Binary(r.as_wkb()))",
            ));
        }
        RetShape::RasterBlob => {
            s.push_str(&format!(
                "{i}let r = {module}::{func}({call_args})\n\
                 {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                 {i}        format!(\"{sql_name}: {{}}\", shim_err_string(e))))?;\n\
                 {i}Ok(ftypes::ScalarValue::Binary(r.as_binary()))",
            ));
        }
        RetShape::FirstGeomBlob => {
            s.push_str(&format!(
                "{i}let r: Vec<Geometry> = {module}::{func}({call_args})\n\
                 {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                 {i}        format!(\"{sql_name}: {{}}\", postgis_err_string(e))))?;\n\
                 {i}match r.first() {{\n\
                 {i}    Some(g) => Ok(ftypes::ScalarValue::Binary(g.as_wkb())),\n\
                 {i}    None => Ok(ftypes::ScalarValue::Null),\n\
                 {i}}}",
            ));
        }
        RetShape::FirstOptionU32Int => {
            s.push_str(&format!(
                "{i}let r: Vec<Option<u32>> = {module}::{func}({call_args})\n\
                 {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                 {i}        format!(\"{sql_name}: {{}}\", postgis_err_string(e))))?;\n\
                 {i}match r.first() {{\n\
                 {i}    Some(Some(v)) => Ok(ftypes::ScalarValue::Int64(*v as i64)),\n\
                 {i}    Some(None) | None => Ok(ftypes::ScalarValue::Null),\n\
                 {i}}}",
            ));
        }
        // Gap G3 (#668): bbox3d return as ISO-WKB `LINESTRING Z`
        // blob (min-corner -> max-corner diagonal). Non-fallible
        // upstream (`st-extent-threed -> bbox3d`).
        RetShape::Bbox3dWkbLineZ => {
            s.push_str(&format!(
                "{i}let __bb = {module}::{func}({call_args});\n\
                 {i}let mut __wkb: Vec<u8> = Vec::with_capacity(57);\n\
                 {i}// ISO-WKB header: little-endian, type 1002 (LINESTRING Z), 2 points.\n\
                 {i}__wkb.push(0x01u8);\n\
                 {i}__wkb.extend_from_slice(&1002u32.to_le_bytes());\n\
                 {i}__wkb.extend_from_slice(&2u32.to_le_bytes());\n\
                 {i}__wkb.extend_from_slice(&__bb.min_x.to_le_bytes());\n\
                 {i}__wkb.extend_from_slice(&__bb.min_y.to_le_bytes());\n\
                 {i}__wkb.extend_from_slice(&__bb.min_z.to_le_bytes());\n\
                 {i}__wkb.extend_from_slice(&__bb.max_x.to_le_bytes());\n\
                 {i}__wkb.extend_from_slice(&__bb.max_y.to_le_bytes());\n\
                 {i}__wkb.extend_from_slice(&__bb.max_z.to_le_bytes());\n\
                 {i}Ok(ftypes::ScalarValue::Binary(__wkb.into()))",
            ));
        }
        _ => {
            s.push_str(&format!(
                "{i}Err(ftypes::FunctionError::ExecutionError(\
                 format!(\"{sql_name}: aggregate return shape not wired\")))",
            ));
        }
    }
    s
}

/// Map an `AggregateShape` ret to the LogicalType the
/// `return_type` method should advertise. Same FROZEN logical-set
/// rules as scalar `retshape_to_logicaltype`. Mirrors the per-arm
/// finalize encoder.
pub fn aggregate_ret_logicaltype(shape: &AggregateShape) -> String {
    retshape_to_logicaltype(&shape.ret)
}

/// #607 Phase 2: Datafission-target aggregate finalize body for
/// `AccKind::Record` — mobilitydb temporal-type aggregators.
///
/// Pattern (lazy decode per DD1):
///   1. `st.blobs` holds the per-row WTV-framed wit-value bytes
///      that `accumulate` collected via `ScalarValue::Binary`.
///   2. For each blob, synthesise a one-element ScalarValue slice
///      and call the existing per-record `arg_witvalue_<snake>`
///      helper — that helper validates the magic + type-id and
///      ciborium-decodes the CBOR payload straight into UPSTREAM.
///   3. The upstream aggregator runs against the collected
///      `&[UpstreamRecord]` slice.
///   4. The `option<R>` result encodes back via the matching
///      `ret_to_witvalue_<snake>` helper, which produces a
///      `ScalarValue::Binary(framed_bytes)`.
///
/// Extras (config args passed at create-accumulator-with-configs
/// time) are not supported on the AccKind::Record surface today —
/// no known temporal-aggregate signature uses them. The body
/// errors clearly if one materialises so the diagnostic is loud
/// rather than emitting a half-wired body.
fn emit_aggregate_finalize_body_record(
    shape: &AggregateShape,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let AccKind::Record { input, output } = &shape.accumulator_kind else {
        unreachable!("invariant: caller checks AccKind::Record");
    };
    // #612 (OQ1): decode side resolves the per-record helper on the
    // INPUT record; encode side resolves on the OUTPUT record. For
    // same-record aggregates the two snakes are identical.
    // #709: use RecordSpec's pre-computed `helper_snake` so cross-
    // interface kebab collisions route to disambiguated names.
    let in_snake = &input.helper_snake;
    let out_snake = &output.helper_snake;

    let mut s = String::new();
    s.push_str(&format!(
        "{i}let mut upstream_vec = Vec::with_capacity(st.blobs.len());\n\
         {i}for b in &st.blobs {{\n\
         {i}    // Slice trick: arg_witvalue_<snake> takes the standard\n\
         {i}    // `&[ScalarValue]` / idx signature. Synthesise a\n\
         {i}    // one-element slice so we can reuse the helper from\n\
         {i}    // the aggregate finalize site. The helper validates\n\
         {i}    // the WTV magic + type-id and ciborium-decodes the\n\
         {i}    // CBOR payload into UPSTREAM.\n\
         {i}    let __args = [ftypes::ScalarValue::Binary(b.clone())];\n\
         {i}    upstream_vec.push(arg_witvalue_{in_snake}(&__args, 0, \"{sql_name}\")?);\n\
         {i}}}\n",
    ));

    // Phase 2 pilot doesn't support record-typed aggregates with
    // extras — same posture as sqlite + duckdb. Surface clearly.
    if !shape.extra_args.is_empty() {
        s.push_str(&format!(
            "{i}return Err(ftypes::FunctionError::ExecutionError(\n\
             {i}    format!(\"{sql_name}: AccKind::Record aggregate with extra args not yet wired\")));\n",
        ));
        return s;
    }

    // Option<R'> result encodes via the output-record's ret helper,
    // which may differ from the input record (#612 OQ1: e.g. decode
    // via `arg_witvalue_tgeompoint_sequence`, encode via
    // `ret_to_witvalue_stbox`).
    s.push_str(&format!(
        "{i}let __r = {module}::{func}(&upstream_vec);\n\
         {i}match __r {{\n\
         {i}    Some(__rec) => ret_to_witvalue_{out_snake}(__rec),\n\
         {i}    None => Ok(ftypes::ScalarValue::Null),\n\
         {i}}}\n",
    ));
    s
}

/// #614: Datafission-target aggregate finalize body for
/// `AccKind::RecordToScalar` — mobilitydb trajectory-pattern
/// counters. Input side mirrors `emit_aggregate_finalize_body_record`
/// (decode each `st.blobs` entry via the per-input-record
/// `arg_witvalue_<in_snake>` helper); output side wraps the
/// primitive return in the matching `ScalarValue` variant rather
/// than routing through `ret_to_witvalue_<snake>`.
///
/// Extras (configs serialised at `create-accumulator-with-configs`
/// time) follow the same JSON-decoded shape the Geom/Raster
/// aggregate path uses (`serde_json::from_str` per-extra).
fn emit_aggregate_finalize_body_record_to_scalar(
    shape: &AggregateShape,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let AccKind::RecordToScalar { input, output, optional } = &shape.accumulator_kind else {
        unreachable!("invariant: caller checks AccKind::RecordToScalar");
    };
    let optional = *optional;
    // #709: use pre-computed helper snake.
    let in_snake = &input.helper_snake;

    let mut s = String::new();
    s.push_str(&format!(
        "{i}let mut upstream_vec = Vec::with_capacity(st.blobs.len());\n\
         {i}for b in &st.blobs {{\n\
         {i}    let __args = [ftypes::ScalarValue::Binary(b.clone())];\n\
         {i}    upstream_vec.push(arg_witvalue_{in_snake}(&__args, 0, \"{sql_name}\")?);\n\
         {i}}}\n",
    ));

    // Re-decode extras into Rust-typed bindings — same JSON-config
    // shape as the Geom/Raster aggregate finalize path.
    let mut call_extras: Vec<String> = Vec::new();
    if !shape.extra_args.is_empty() {
        for (j, p) in shape.extra_args.iter().enumerate() {
            let getc = format!(
                "{i}let extra{j}_str = st.extras.get({j})\n\
                 {i}    .ok_or_else(|| ftypes::FunctionError::ExecutionError(\n\
                 {i}        format!(\"{sql_name}: missing config arg #{j}\")))?;\n",
            );
            match p {
                ParamShape::Text => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: &str = extra{j}_str.as_str();\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::F64 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: f64 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S32 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: i32 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S64 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: i64 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U32 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: u32 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U64 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: u64 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Bool => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: bool = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::OptionNone => {
                    call_extras.push("None".to_string());
                }
                ParamShape::Blob
                | ParamShape::Geom
                | ParamShape::Geog
                | ParamShape::Raster
                | ParamShape::Topology
                | ParamShape::ListGeom
                | ParamShape::WitValueRecord { .. }
                | ParamShape::Enum { .. }
                | ParamShape::ListPrim(_)
                | ParamShape::ListRecord { .. }
                | ParamShape::ListTuple { .. }
                | ParamShape::ListTupleMixed { .. }
                | ParamShape::ListListU8
                | ParamShape::ListListPrim(_) => {
                    return format!(
                        "{i}Err(ftypes::FunctionError::ExecutionError(\
                         format!(\"{sql_name}: aggregate config arg #{j} shape not wired\")))",
                    );
                }
            }
        }
    }

    let call_args = if call_extras.is_empty() {
        "&upstream_vec".to_string()
    } else {
        format!("&upstream_vec, {}", call_extras.join(", "))
    };

    // #637: `optional = true` wraps `Some(v)` per the native scalar
    // variant and emits `ScalarValue::Null` on `None`.
    let some_wrap = match output {
        ScalarReturnKind::F64 | ScalarReturnKind::F32 => {
            "ftypes::ScalarValue::Float64(v as f64)".to_string()
        }
        ScalarReturnKind::Bool => "ftypes::ScalarValue::Boolean(v)".to_string(),
        ScalarReturnKind::U32
        | ScalarReturnKind::S32
        | ScalarReturnKind::U64
        | ScalarReturnKind::S64
        | ScalarReturnKind::U8 => "ftypes::ScalarValue::Int64(v as i64)".to_string(),
    };
    let wrap = if optional {
        format!(
            "match __r {{ Some(v) => Ok({some_wrap}), None => Ok(ftypes::ScalarValue::Null) }}",
        )
    } else {
        match output {
            ScalarReturnKind::F64 | ScalarReturnKind::F32 => {
                format!("Ok(ftypes::ScalarValue::Float64(__r as f64))")
            }
            ScalarReturnKind::Bool => {
                format!("Ok(ftypes::ScalarValue::Boolean(__r))")
            }
            ScalarReturnKind::U32
            | ScalarReturnKind::S32
            | ScalarReturnKind::U64
            | ScalarReturnKind::S64
            | ScalarReturnKind::U8 => {
                format!("Ok(ftypes::ScalarValue::Int64(__r as i64))")
            }
        }
    };
    s.push_str(&format!(
        "{i}let __r = {module}::{func}({call_args});\n\
         {i}{wrap}\n",
    ));
    s
}

/// #640: Datafission-target aggregate finalize body for
/// `AccKind::RecordToTuple` — mobilitydb `tint-range-aggregate`
/// (and any future record-input aggregate returning a primitive
/// tuple). Input side mirrors
/// `emit_aggregate_finalize_body_record_to_scalar` (decode each
/// `st.blobs` entry via the per-input-record `arg_witvalue_<in_snake>`
/// helper); output side serialises the upstream Rust tuple to
/// JSON-array text via `serde_json::to_string` and wraps it in
/// `ScalarValue::Utf8`. The `optional = true` path emits
/// `None → ScalarValue::Null` / `Some(t) → JSON text`.
fn emit_aggregate_finalize_body_record_to_tuple(
    shape: &AggregateShape,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let AccKind::RecordToTuple { input, output: _, optional } = &shape.accumulator_kind else {
        unreachable!("invariant: caller checks AccKind::RecordToTuple");
    };
    let optional = *optional;
    // #709: use pre-computed helper snake.
    let in_snake = &input.helper_snake;

    let mut s = String::new();
    s.push_str(&format!(
        "{i}let mut upstream_vec = Vec::with_capacity(st.blobs.len());\n\
         {i}for b in &st.blobs {{\n\
         {i}    let __args = [ftypes::ScalarValue::Binary(b.clone())];\n\
         {i}    upstream_vec.push(arg_witvalue_{in_snake}(&__args, 0, \"{sql_name}\")?);\n\
         {i}}}\n",
    ));

    // Re-decode extras into Rust-typed bindings — same JSON-config
    // shape as the RecordToScalar aggregate finalize path.
    let mut call_extras: Vec<String> = Vec::new();
    if !shape.extra_args.is_empty() {
        for (j, p) in shape.extra_args.iter().enumerate() {
            let getc = format!(
                "{i}let extra{j}_str = st.extras.get({j})\n\
                 {i}    .ok_or_else(|| ftypes::FunctionError::ExecutionError(\n\
                 {i}        format!(\"{sql_name}: missing config arg #{j}\")))?;\n",
            );
            match p {
                ParamShape::Text => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: &str = extra{j}_str.as_str();\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::F64 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: f64 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S32 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: i32 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S64 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: i64 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U32 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: u32 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U64 => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: u64 = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Bool => {
                    s.push_str(&getc);
                    s.push_str(&format!(
                        "{i}let extra{j}: bool = serde_json::from_str(extra{j}_str)\n\
                         {i}    .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
                         {i}        format!(\"{sql_name}: arg #{j} parse: {{}}\", e)))?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::OptionNone => {
                    call_extras.push("None".to_string());
                }
                ParamShape::Blob
                | ParamShape::Geom
                | ParamShape::Geog
                | ParamShape::Raster
                | ParamShape::Topology
                | ParamShape::ListGeom
                | ParamShape::WitValueRecord { .. }
                | ParamShape::Enum { .. }
                | ParamShape::ListPrim(_)
                | ParamShape::ListRecord { .. }
                | ParamShape::ListTuple { .. }
                | ParamShape::ListTupleMixed { .. }
                | ParamShape::ListListU8
                | ParamShape::ListListPrim(_) => {
                    return format!(
                        "{i}Err(ftypes::FunctionError::ExecutionError(\
                         format!(\"{sql_name}: aggregate config arg #{j} shape not wired\")))",
                    );
                }
            }
        }
    }

    let call_args = if call_extras.is_empty() {
        "&upstream_vec".to_string()
    } else {
        format!("&upstream_vec, {}", call_extras.join(", "))
    };

    // JSON-encode the upstream Rust tuple. serde-derives produce a
    // fixed-length JSON array (same render as
    // `JsonRetKind::TuplePrim` / `OptionTuplePrim` on the scalar
    // surface). `optional = true` wraps `Some(t)` in Utf8 and emits
    // ScalarValue::Null on `None`.
    let body = if optional {
        format!(
            "match __r {{\n\
             {i}    Some(__t) => {{\n\
             {i}        let __json = serde_json::to_string(&__t)\n\
             {i}            .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
             {i}                format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
             {i}        Ok(ftypes::ScalarValue::Utf8(__json))\n\
             {i}    }}\n\
             {i}    None => Ok(ftypes::ScalarValue::Null),\n\
             {i}}}",
        )
    } else {
        format!(
            "{{\n\
             {i}    let __json = serde_json::to_string(&__r)\n\
             {i}        .map_err(|e| ftypes::FunctionError::ExecutionError(\n\
             {i}            format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
             {i}    Ok(ftypes::ScalarValue::Utf8(__json))\n\
             {i}}}",
        )
    };
    s.push_str(&format!(
        "{i}let __r = {module}::{func}({call_args});\n\
         {i}{body}\n",
    ));
    s
}

// ─── UDTF (table function) dispatch ───
//
// Datafission's `table_function_registry@1.0.0` contract is
// iterator-based:
//
//   begin(name, args)               -> u64               (open)
//   next-row(handle)                -> option<result<row>> (one row)
//   end(handle)                     -> ()                (close)
//
// On `begin` the bridge marshals the args, calls the upstream WIT
// function ONCE, materialises the rows into `Vec<Vec<scalar-value>>`,
// stores them in a per-handle state, and returns the handle. Each
// `next_row` call peels one row off the head; `end` drops the
// state. This trades streaming for simplicity — the postgis +
// mobilitydb UDTF surface returns bounded rowsets (typically <
// 1K rows per call) so eager materialisation is acceptable.
//
// Returned body goes inside `"sql_name" => { ... }` in `begin`.
// Expects the bridge prelude to have a `UDTF_STATE` thread-local
// + an `alloc_udtf_handle()` allocator (both emitted by the
// UDTF_STATE_BLOCK prelude).
pub fn emit_udtf_begin_body(
    shape: &UdtfShape,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let mut s = String::new();

    // Same early-bail as duckdb-emit: a mid-body `return Err(...)`
    // from emit_udtf_param_marshal_df would still leave the
    // upstream call site with the wrong arg count under rustc's
    // unreachable-code type-check. Detect unsupported list shapes
    // up front and emit only the error return.
    if let Some((idx, shape_name)) = shape.params.iter().enumerate().find_map(|(idx, p)| {
        match p {
            ParamShape::ListGeom => Some((idx, "list<geometry>")),
            ParamShape::ListRecord { .. } => Some((idx, "list<record>")),
            ParamShape::ListTuple { .. } => Some((idx, "list<tuple>")),
            ParamShape::ListTupleMixed { .. } => Some((idx, "list<tuple<mixed>>")),
            ParamShape::ListPrim(_) => Some((idx, "list<primitive>")),
            ParamShape::ListListU8 => Some((idx, "list<list<u8>>")),
            ParamShape::ListListPrim(_) => Some((idx, "list<list<primitive>>")),
            ParamShape::Enum { .. } => Some((idx, "enum")),
            _ => None,
        }
    }) {
        return format!(
            "{i}Err(ftypes::FunctionError::ExecutionError(format!(\
             \"{sql_name}: UDTF param #{idx} ({shape_name}) not wired\")))",
        );
    }

    let (decls, call_args) =
        emit_udtf_param_marshal_df(&shape.params, sql_name, i);
    s.push_str(&decls);
    let call_args_str = call_args.join(", ");

    let unwrap = if shape.fallible {
        format!(
            ".map_err(|e| ftypes::FunctionError::ExecutionError(format!(\"{sql_name}: {{}}\", shim_err_string(e))))?"
        )
    } else {
        String::new()
    };
    s.push_str(&format!(
        "{i}let __upstream = {module}::{func}({call_args_str}){unwrap};\n",
    ));

    match &shape.output_row {
        UdtfOutputRow::SingleGeom => {
            s.push_str(&format!(
                "{i}let mut rows: Vec<Vec<ftypes::ScalarValue>> = Vec::with_capacity(__upstream.len());\n\
                 {i}for __g in __upstream.iter() {{\n\
                 {i}    rows.push(alloc::vec![ftypes::ScalarValue::Binary(__g.as_wkb())]);\n\
                 {i}}}\n\
                 {i}Ok(alloc_udtf_handle(rows))",
            ));
        }
        UdtfOutputRow::SinglePrimitive { affinity } => {
            let (variant, expr) = match affinity {
                ColumnAffinity::Integer => ("Int64", "*__v as i64"),
                ColumnAffinity::Real => ("Float64", "*__v as f64"),
                ColumnAffinity::Text => ("Utf8", "__v.clone()"),
                ColumnAffinity::Blob => ("Binary", "__v.clone()"),
            };
            s.push_str(&format!(
                "{i}let mut rows: Vec<Vec<ftypes::ScalarValue>> = Vec::with_capacity(__upstream.len());\n\
                 {i}for __v in __upstream.iter() {{\n\
                 {i}    rows.push(alloc::vec![ftypes::ScalarValue::{variant}({expr})]);\n\
                 {i}}}\n\
                 {i}Ok(alloc_udtf_handle(rows))",
            ));
        }
        UdtfOutputRow::Record { fields } => {
            let mut row_exprs: Vec<String> = Vec::new();
            for f in fields {
                let snake = f.name.replace('-', "_");
                let expr = match f.field_shape {
                    UdtfFieldShape::Int => format!(
                        "ftypes::ScalarValue::Int64(__row.{snake} as i64)"
                    ),
                    UdtfFieldShape::Real => format!(
                        "ftypes::ScalarValue::Float64(__row.{snake} as f64)"
                    ),
                    UdtfFieldShape::Text => format!(
                        "ftypes::ScalarValue::Utf8(__row.{snake}.clone())"
                    ),
                    UdtfFieldShape::Blob => format!(
                        "ftypes::ScalarValue::Binary(__row.{snake}.clone())"
                    ),
                    UdtfFieldShape::GeomBlob => format!(
                        "ftypes::ScalarValue::Binary(__row.{snake}.as_wkb())"
                    ),
                    UdtfFieldShape::OptionInt => format!(
                        "match __row.{snake} {{ Some(v) => ftypes::ScalarValue::Int64(v as i64), None => ftypes::ScalarValue::Null }}"
                    ),
                    UdtfFieldShape::OptionReal => format!(
                        "match __row.{snake} {{ Some(v) => ftypes::ScalarValue::Float64(v as f64), None => ftypes::ScalarValue::Null }}"
                    ),
                    UdtfFieldShape::OptionText => format!(
                        "match &__row.{snake} {{ Some(v) => ftypes::ScalarValue::Utf8(v.clone()), None => ftypes::ScalarValue::Null }}"
                    ),
                    UdtfFieldShape::OptionBlob => format!(
                        "match &__row.{snake} {{ Some(v) => ftypes::ScalarValue::Binary(v.clone()), None => ftypes::ScalarValue::Null }}"
                    ),
                    UdtfFieldShape::OptionGeomBlob => format!(
                        "match &__row.{snake} {{ Some(v) => ftypes::ScalarValue::Binary(v.as_wkb()), None => ftypes::ScalarValue::Null }}"
                    ),
                    UdtfFieldShape::Unsupported => {
                        "ftypes::ScalarValue::Null".to_string()
                    }
                };
                row_exprs.push(expr);
            }
            let row_block = row_exprs.join(", ");
            s.push_str(&format!(
                "{i}let mut rows: Vec<Vec<ftypes::ScalarValue>> = Vec::with_capacity(__upstream.len());\n\
                 {i}for __row in __upstream.into_iter() {{\n\
                 {i}    rows.push(alloc::vec![{row_block}]);\n\
                 {i}}}\n\
                 {i}Ok(alloc_udtf_handle(rows))",
            ));
        }
        UdtfOutputRow::Unwired { reason } => {
            let r = reason.replace('"', "\\\"");
            s.push_str(&format!(
                "{i}Err(ftypes::FunctionError::ExecutionError(format!(\"{sql_name}: UDTF row shape unwired: {r}\")))",
            ));
        }
    }
    s
}

/// Emit per-UDTF column_info entries for `output_schema(name)`.
/// Returns the inner list literal (no surrounding `vec![...]`).
pub fn emit_udtf_column_info(shape: &UdtfShape) -> String {
    let mut s = String::new();
    let single_geom_col_name = match &shape.output_row {
        UdtfOutputRow::SingleGeom => {
            datalink_shim_codegen_core::interface_db
                ::single_geom_column_name_for("geom")
        }
        _ => "value",
    };
    match &shape.output_row {
        UdtfOutputRow::SingleGeom => {
            s.push_str(&format!(
                "ftypes::ColumnInfo {{ name: \"{single_geom_col_name}\".into(), ty: ftypes::LogicalType::Binary }},",
            ));
        }
        UdtfOutputRow::SinglePrimitive { affinity } => {
            let logical = match affinity {
                ColumnAffinity::Integer => "ftypes::LogicalType::Int64",
                ColumnAffinity::Real => "ftypes::LogicalType::Float64",
                ColumnAffinity::Text => "ftypes::LogicalType::Utf8",
                ColumnAffinity::Blob => "ftypes::LogicalType::Binary",
            };
            s.push_str(&format!(
                "ftypes::ColumnInfo {{ name: \"value\".into(), ty: {logical} }},",
            ));
        }
        UdtfOutputRow::Record { fields } => {
            for f in fields {
                let logical = match f.field_shape {
                    UdtfFieldShape::Int | UdtfFieldShape::OptionInt => {
                        "ftypes::LogicalType::Int64"
                    }
                    UdtfFieldShape::Real | UdtfFieldShape::OptionReal => {
                        "ftypes::LogicalType::Float64"
                    }
                    UdtfFieldShape::Text | UdtfFieldShape::OptionText => {
                        "ftypes::LogicalType::Utf8"
                    }
                    UdtfFieldShape::Blob
                    | UdtfFieldShape::OptionBlob
                    | UdtfFieldShape::GeomBlob
                    | UdtfFieldShape::OptionGeomBlob
                    | UdtfFieldShape::Unsupported => "ftypes::LogicalType::Binary",
                };
                let col_name = f.name.replace('"', "\\\"");
                s.push_str(&format!(
                    "ftypes::ColumnInfo {{ name: \"{col_name}\".into(), ty: {logical} }},",
                ));
            }
        }
        UdtfOutputRow::Unwired { .. } => {
            s.push_str(
                "ftypes::ColumnInfo { name: \"value\".into(), ty: ftypes::LogicalType::Binary },",
            );
        }
    }
    s
}

/// UDTF param marshalling — datafission flavour.
fn emit_udtf_param_marshal_df(
    params: &[ParamShape],
    sql_name: &str,
    i: &str,
) -> (String, Vec<String>) {
    let mut s = String::new();
    let mut call_args: Vec<String> = Vec::with_capacity(params.len());

    for (idx, p) in params.iter().enumerate() {
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
            ParamShape::Geom => {
                s.push_str(&format!(
                    "{i}let arg{idx} = from_wkb(dfv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Geog => {
                s.push_str(&format!(
                    "{i}let arg{idx} = geog_from_wkb(dfv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Raster => {
                s.push_str(&format!(
                    "{i}let arg{idx} = from_raster_binary(dfv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Topology => {
                s.push_str(&format!(
                    "{i}let arg{idx} = from_topology_bytes(dfv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::OptionNone => {
                call_args.push("None".to_string());
            }
            ParamShape::WitValueRecord { helper_snake, .. } => {
                // #709: use pre-computed helper snake.
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_witvalue_{helper_snake}(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Enum { .. }
            | ParamShape::ListGeom
            | ParamShape::ListRecord { .. }
            | ParamShape::ListTuple { .. }
            | ParamShape::ListTupleMixed { .. }
            | ParamShape::ListPrim(_)
            | ParamShape::ListListU8
            | ParamShape::ListListPrim(_) => {
                s.push_str(&format!(
                    "{i}return Err(ftypes::FunctionError::ExecutionError(format!(\"{sql_name}: UDTF param shape not wired\")));\n",
                ));
            }
        }
    }
    (s, call_args)
}
