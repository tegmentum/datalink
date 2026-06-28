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
    DispatchShape, JsonRetKind, ListPrimElem, ParamShape, RetShape,
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
            ParamShape::ListRecord { kebab_name, .. } => {
                let snake = kebab_name.replace('-', "_");
                s.push_str(&format!(
                    "{i}let arg{idx} = parse_json_list_record_{snake}(&args, {idx}, \"{sql_name}\")?;\n",
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
            ParamShape::WitValueRecord {
                kebab_name,
                upstream_by_value,
                ..
            } => {
                // Magic-prefix Binary scheme (4-byte WTV magic +
                // 32-byte type_id + canonical-CBOR payload). The
                // per-record `arg_witvalue_<snake>` helper checks
                // the magic + type_id and ciborium-decodes the
                // payload directly into the upstream record type.
                let snake = kebab_name.replace('-', "_");
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_witvalue_{snake}(&args, {idx}, \"{sql_name}\")?;\n",
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
        RetShape::JsonText { kind } => match kind {
            JsonRetKind::ListListPrim(_)
            | JsonRetKind::ListTuplePrim(_)
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
        RetShape::WitValueRecord { kebab_name, .. } => {
            let snake = kebab_name.replace('-', "_");
            format!("ret_to_witvalue_{snake}({call_expr}{unwrap_chain})")
        }
        RetShape::OptionWitValueRecord { kebab_name, .. } => {
            let snake = kebab_name.replace('-', "_");
            format!(
                "match {call_expr}{unwrap_chain} {{\n\
                 {i}    Some(__rec) => ret_to_witvalue_{snake}(__rec),\n\
                 {i}    None => Ok(types::ScalarValue::Null),\n\
                 {i}}}"
            )
        }
        RetShape::FirstWitValueRecord { kebab_name, .. } => {
            let snake = kebab_name.replace('-', "_");
            format!(
                "{{\n\
                 {i}    let __r = {call_expr}{unwrap_chain};\n\
                 {i}    let mut __it = __r.into_iter();\n\
                 {i}    match __it.next() {{\n\
                 {i}        Some(__rec) => ret_to_witvalue_{snake}(__rec),\n\
                 {i}        None => Ok(types::ScalarValue::Null),\n\
                 {i}    }}\n\
                 {i}}}"
            )
        }
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
        | ParamShape::ListTuple { .. } => "types::LogicalType::Utf8".to_string(),
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
