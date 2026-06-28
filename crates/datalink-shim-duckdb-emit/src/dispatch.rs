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
//!     scalar emit unpacks all of these into `i64` (or `as
//!     i32` / `as u32` / etc.) via the shared `dv_i64` helper.
//!   * Record-typed wit-value payloads ride on `Duckvalue::Complex
//!     { type_expr, json }` — the JSON-direct path described in the
//!     duckdb-emit AGENTS notes. The bridge has no LOCAL serde-ops
//!     codec (`SerdeOpsGuest` is not emitted on the duckdb target),
//!     so the per-record helpers `serde_json::from_str::<UPSTREAM>`
//!     straight into the upstream type. wit-bindgen's
//!     `additional_derives: [serde::Deserialize, serde::Serialize]`
//!     makes UPSTREAM serdeable.

use datalink_shim_codegen_core::interface_db::{
    list_tuple_sig_suffix, DispatchShape, JsonRetKind, ListPrimElem,
    ParamShape, RetShape,
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
            ParamShape::Geom => {
                s.push_str(&format!(
                    "{i}let arg{idx} = from_wkb(dv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Geog => {
                s.push_str(&format!(
                    "{i}let arg{idx} = geog_from_wkb(dv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Raster => {
                s.push_str(&format!(
                    "{i}let arg{idx} = from_raster_binary(dv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Topology => {
                s.push_str(&format!(
                    "{i}let arg{idx} = from_topology_bytes(dv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::OptionNone => {
                call_args.push("None".to_string());
            }
            ParamShape::ListGeom => {
                // Mirrors sqlite-emit: variadic when this is the last
                // param, single-element list otherwise.
                let is_variadic = idx + 1 == shape.params.len();
                if is_variadic {
                    s.push_str(&format!(
                        "{i}let arg{idx}_owned: Vec<Geometry> = args[{idx}..]\n\
                         {i}    .iter()\n\
                         {i}    .enumerate()\n\
                         {i}    .map(|(j, v)| match v {{\n\
                         {i}        types::Duckvalue::Blob(b) => Geometry::from_wkb(b.as_slice())\n\
                         {i}            .map_err(|e| types::Duckerror::Invalidargument(\n\
                         {i}                format!(\"{sql_name}: arg {{}}: {{}}\", {idx} + j, postgis_err_string(e)))),\n\
                         {i}        types::Duckvalue::Text(t) => Geometry::from_wkb(t.as_bytes())\n\
                         {i}            .map_err(|e| types::Duckerror::Invalidargument(\n\
                         {i}                format!(\"{sql_name}: arg {{}}: {{}}\", {idx} + j, postgis_err_string(e)))),\n\
                         {i}        _ => Err(types::Duckerror::Invalidargument(\n\
                         {i}            format!(\"{sql_name}: arg {{}} must be BLOB\", {idx} + j))),\n\
                         {i}    }})\n\
                         {i}    .collect::<Result<Vec<_>, _>>()?;\n\
                         {i}let arg{idx}: Vec<&Geometry> = arg{idx}_owned.iter().collect();\n",
                    ));
                } else {
                    s.push_str(&format!(
                        "{i}let arg{idx}_one = from_wkb(dv_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n\
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
                    "{i}let arg{idx} = match dv_i64(&args, {idx}, \"{sql_name}\")? {{\n{arms}{i}    other => return Err(types::Duckerror::Invalidargument(format!(\n\
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

    // Compose the call expression (handles `method_call` for
    // constructors and instance methods on WIT resources).
    let call_args_str = call_args.join(", ");
    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let call_expr = if let Some(mc) = shape.method_call.as_ref() {
        if mc.is_constructor {
            let pascal = kebab_to_pascal(&mc.resource_kebab);
            format!("{pascal}::new({call_args_str})")
        } else {
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
            ".map_err(|e| types::Duckerror::Invalidargument(format!(\"{sql_name}: {{}}\", shim_err_string(e))))?"
        )
    } else {
        String::new()
    };

    let return_expr = render_return_expr(&shape.ret, &call_expr, &unwrap_chain, sql_name, i);
    s.push_str(i);
    s.push_str(&return_expr);
    s.push('\n');
    s
}

/// Render the full Rust expression that wraps the upstream call
/// into an `Ok(Duckvalue)` (or a block expression equivalent).
fn render_return_expr(
    ret: &RetShape,
    call_expr: &str,
    unwrap_chain: &str,
    sql_name: &str,
    i: &str,
) -> String {
    match ret {
        RetShape::Text => format!(
            "Ok(types::Duckvalue::Text(({call_expr}{unwrap_chain}).into()))"
        ),
        RetShape::Real => format!(
            "Ok(types::Duckvalue::Float64({call_expr}{unwrap_chain}))"
        ),
        RetShape::Int => format!(
            "Ok(types::Duckvalue::Int64({call_expr}{unwrap_chain} as i64))"
        ),
        RetShape::BoolInt => format!(
            "Ok(types::Duckvalue::Boolean({call_expr}{unwrap_chain}))"
        ),
        RetShape::Blob => format!(
            "Ok(types::Duckvalue::Blob(({call_expr}{unwrap_chain}).into()))"
        ),
        RetShape::GeomBlob => {
            if unwrap_chain.is_empty() {
                format!("Ok(types::Duckvalue::Blob({call_expr}.as_wkb().into()))")
            } else {
                format!(
                    "{{\n\
                     {i}    let __r = {call_expr}{unwrap_chain};\n\
                     {i}    Ok(types::Duckvalue::Blob(__r.as_wkb().into()))\n\
                     {i}}}"
                )
            }
        }
        RetShape::RasterBlob => {
            if unwrap_chain.is_empty() {
                format!("Ok(types::Duckvalue::Blob({call_expr}.as_binary().into()))")
            } else {
                format!(
                    "{{\n\
                     {i}    let __r = {call_expr}{unwrap_chain};\n\
                     {i}    Ok(types::Duckvalue::Blob(__r.as_binary().into()))\n\
                     {i}}}"
                )
            }
        }
        RetShape::TopologyBlob => {
            if unwrap_chain.is_empty() {
                format!("Ok(types::Duckvalue::Blob({call_expr}.to_bytes().into()))")
            } else {
                format!(
                    "{{\n\
                     {i}    let __r = {call_expr}{unwrap_chain};\n\
                     {i}    Ok(types::Duckvalue::Blob(__r.to_bytes().into()))\n\
                     {i}}}"
                )
            }
        }
        RetShape::OptionText => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::Duckvalue::Text(v.into()),\n\
             {i}    None => types::Duckvalue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionReal => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::Duckvalue::Float64(v as f64),\n\
             {i}    None => types::Duckvalue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionInt => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::Duckvalue::Int64(v as i64),\n\
             {i}    None => types::Duckvalue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionBoolInt => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::Duckvalue::Boolean(v),\n\
             {i}    None => types::Duckvalue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::Duckvalue::Blob(v.into()),\n\
             {i}    None => types::Duckvalue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionGeomBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::Duckvalue::Blob(v.as_wkb().into()),\n\
             {i}    None => types::Duckvalue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionRasterBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::Duckvalue::Blob(v.as_binary().into()),\n\
             {i}    None => types::Duckvalue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionTopologyBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => types::Duckvalue::Blob(v.to_bytes().into()),\n\
             {i}    None => types::Duckvalue::Null,\n\
             {i}}})"
        ),
        RetShape::FirstGeomBlob => format!(
            "{{\n\
             {i}    let __r: Vec<Geometry> = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(g) => Ok(types::Duckvalue::Blob(g.as_wkb().into())),\n\
             {i}        None => Ok(types::Duckvalue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstRasterBlob => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.into_iter().next() {{\n\
             {i}        Some(r) => Ok(types::Duckvalue::Blob(r.as_binary().into())),\n\
             {i}        None => Ok(types::Duckvalue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstTopologyBlob => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.into_iter().next() {{\n\
             {i}        Some(t) => Ok(types::Duckvalue::Blob(t.to_bytes().into())),\n\
             {i}        None => Ok(types::Duckvalue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstOptionU32Int => format!(
            "{{\n\
             {i}    let __r: Vec<Option<u32>> = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(Some(v)) => Ok(types::Duckvalue::Uint32(*v)),\n\
             {i}        Some(None) | None => Ok(types::Duckvalue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstInt => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(v) => Ok(types::Duckvalue::Int64(*v as i64)),\n\
             {i}        None => Ok(types::Duckvalue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstReal => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(v) => Ok(types::Duckvalue::Float64(*v as f64)),\n\
             {i}        None => Ok(types::Duckvalue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstText => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.into_iter().next() {{\n\
             {i}        Some(v) => Ok(types::Duckvalue::Text(v.into())),\n\
             {i}        None => Ok(types::Duckvalue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::BboxBlob => format!(
            "{{\n\
             {i}    let __bb = {call_expr}{unwrap_chain};\n\
             {i}    let __env = pg_ctor::st_make_envelope(__bb.min_x, __bb.min_y, __bb.max_x, __bb.max_y);\n\
             {i}    Ok(types::Duckvalue::Blob(__env.as_wkb().into()))\n\
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
             {i}    Ok(types::Duckvalue::Text(format!(\n\
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
                 {i}    Ok(types::Duckvalue::Int64(__disc))\n\
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
                 {i}        .map_err(|e| types::Duckerror::Internal(format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
                 {i}    Ok(types::Duckvalue::Text(__json))\n\
                 {i}}}"
            ),
            JsonRetKind::OptionTuplePrim(_) => format!(
                "{{\n\
                 {i}    match {call_expr}{unwrap_chain} {{\n\
                 {i}        Some(__t) => {{\n\
                 {i}            let __json = serde_json::to_string(&__t)\n\
                 {i}                .map_err(|e| types::Duckerror::Internal(format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
                 {i}            Ok(types::Duckvalue::Text(__json))\n\
                 {i}        }}\n\
                 {i}        None => Ok(types::Duckvalue::Null),\n\
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
                 {i}            .map_err(|e| types::Duckerror::Internal(format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
                 {i}        __out.push_str(&__vj);\n\
                 {i}        __out.push(']');\n\
                 {i}    }}\n\
                 {i}    __out.push(']');\n\
                 {i}    Ok(types::Duckvalue::Text(__out))\n\
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
                ListPrimElem::String => ("Text", format!("__r.{index}.into()")),
            };
            format!(
                "{{\n\
                 {i}    let __r = {call_expr}{unwrap_chain};\n\
                 {i}    Ok(types::Duckvalue::{variant}({expr_suffix}))\n\
                 {i}}}"
            )
        }
        RetShape::WitValueRecord { kebab_name, .. } => {
            let snake = kebab_name.replace('-', "_");
            // ret_to_witvalue_<snake> already returns
            // `Result<Duckvalue, Duckerror>`, so emit it bare (no
            // outer `Ok(...)` wrap).
            format!("ret_to_witvalue_{snake}({call_expr}{unwrap_chain})")
        }
        RetShape::OptionWitValueRecord { kebab_name, .. } => {
            let snake = kebab_name.replace('-', "_");
            format!(
                "match {call_expr}{unwrap_chain} {{\n\
                 {i}    Some(__rec) => ret_to_witvalue_{snake}(__rec),\n\
                 {i}    None => Ok(types::Duckvalue::Null),\n\
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
                 {i}        None => Ok(types::Duckvalue::Null),\n\
                 {i}    }}\n\
                 {i}}}"
            )
        }
    }
}

/// kebab-case → PascalCase. wit-bindgen does the same transform
/// when emitting Rust enum / resource idents, so the dispatch arm
/// references `<module>::PixelType::Bool1` consistently with the
/// generated bindings.
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
