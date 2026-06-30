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
    list_tuple_sig_suffix, AccKind, AggregateShape, ColumnAffinity,
    DispatchShape, JsonRetKind, ListPrimElem, ParamShape, RetShape,
    ScalarReturnKind, UdtfFieldShape, UdtfOutputRow, UdtfShape,
    WindowReturn, WindowShape,
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
            ParamShape::ListListU8 => {
                // #674: `list<list<u8>>` — batched WKB blobs for the
                // postgis `st_*_batch` family. SQL passes a JSON
                // text matching `Vec<Vec<u8>>` (nested arrays of
                // byte integers); the `parse_json_list_list_u8`
                // helper decodes via serde_json and the dispatch
                // arm passes `&arg{idx}` to the WIT call
                // (wit-bindgen takes `list<list<u8>>` as
                // `&[Vec<u8>]`).
                s.push_str(&format!(
                    "{i}let arg{idx}: Vec<Vec<u8>> = parse_json_list_list_u8(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::ListListPrim(elem) => {
                // #695: `list<list<X>>` over a primitive non-u8
                // element (`list<list<f64>>` on the postgis raster
                // surface; flatgeobuf coord-list shapes). Mirrors
                // `ListListU8` with the per-element helper name
                // and Rust inner type.
                let suffix = elem.helper_suffix();
                let rust_ty = elem.rust_elem();
                s.push_str(&format!(
                    "{i}let arg{idx}: Vec<Vec<{rust_ty}>> = parse_json_list_list_{suffix}(&args, {idx}, \"{sql_name}\")?;\n",
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
        // #707: topo-geometry result — route through `geometry()` +
        // `as-wkb()` (the topo-geometry resource has no direct
        // `to-bytes` method). Covers `create_topo_geom` /
        // `topology_create_topo_geom`.
        RetShape::TopoGeometryViaGeom => {
            if unwrap_chain.is_empty() {
                format!(
                    "Ok(types::Duckvalue::Blob({call_expr}.geometry().as_wkb().into()))"
                )
            } else {
                format!(
                    "{{\n\
                     {i}    let __r = {call_expr}{unwrap_chain};\n\
                     {i}    Ok(types::Duckvalue::Blob(__r.geometry().as_wkb().into()))\n\
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
             {i}    Ok(types::Duckvalue::Blob(__wkb))\n\
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
            // #630: `option<list<R>>` for an all-primitive record R
            // — Some(vec) → JSON array of objects via serde; None →
            // DuckDB NULL. Today's surface: mobilitydb
            // `<date|float|int|tstz>-spanset-from-text`.
            JsonRetKind::OptionListPrimRecord(_) => format!(
                "{{\n\
                 {i}    match {call_expr}{unwrap_chain} {{\n\
                 {i}        Some(__v) => {{\n\
                 {i}            let __json = serde_json::to_string(&__v)\n\
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
        // #677: `list<bool>` / `list<list<u8>>` batch returns —
        // serde-encode the upstream `Vec<bool>` / `Vec<Vec<u8>>`
        // directly to a JSON array text. Symmetric with the
        // param-side `ParamShape::ListListU8` convention.
        RetShape::ListBool | RetShape::ListListU8 => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    let __json = serde_json::to_string(&__r)\n\
             {i}        .map_err(|e| types::Duckerror::Internal(format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
             {i}    Ok(types::Duckvalue::Text(__json))\n\
             {i}}}"
        ),
        // #690: `result<_, E>` (unit OK) — discard the `()` value
        // and yield SQL NULL. `unwrap_chain` carries the
        // `.map_err(...)?` clause for fallible calls and is empty
        // otherwise, so the single shape covers both paths.
        RetShape::Unit => format!(
            "let _ = {call_expr}{unwrap_chain};\n{i}Ok(types::Duckvalue::Null)"
        ),
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

// ─── Aggregate dispatch ───
//
// DuckDB's aggregate ABI hands the guest the entire group as
// `rows: rowbatch` in one call (see callback-dispatch.wit:11
// `call-aggregate`). The guest performs a whole-group fold and
// returns a single `duckvalue`. There is no separate
// init/step/finalize round-trip — the host gathers the rows
// inside DuckDB's aggregate engine and delivers them as a single
// `list<list<duckvalue>>`.
//
// Each row is `list<duckvalue>` ordered as the registration
// declared its args. Column 0 is the streaming arg (a Blob
// carrying the WKB for `Geom` accumulators or the
// PostGIS-raster binary for `Raster` accumulators); columns
// 1..N are extras that must be constant across rows
// (PostgreSQL semantics — `set_or_validate_extras` enforces this
// in sqlite-emit; here we read them from the first non-null row
// and validate on subsequent rows).
//
// Returns the per-arm body — the caller wraps it in
// `<arm_idx>usize => { ... }`.
pub fn emit_aggregate_arm_body(
    shape: &AggregateShape,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let mut s = String::new();

    // #607 Phase 2 + #612 (OQ1): AccKind::Record aggregates
    // (mobilitydb temporal-type aggregators) route to a dedicated
    // emitter. DuckDB's whole-group fold ABI delivers every row in
    // one `call_aggregate` invocation — no accumulator state across
    // calls — so the body decodes column 0 of each row via the
    // per-input-record `arg_witvalue_<snake>` helper, runs the
    // upstream aggregator against the collected `&[Upstream]` slice,
    // and encodes the `Option<R>` result back via the matching
    // per-output-record `ret_to_witvalue_<snake>` encoder. For
    // same-record aggregates `input.kebab_name == output.kebab_name`;
    // for different-record cases (#612: e.g. `tgeompoint-st-extent`
    // returns `option<stbox>`) the two snakes diverge.
    if matches!(&shape.accumulator_kind, AccKind::Record { .. }) {
        return emit_aggregate_arm_body_record(shape, sql_name, arm_indent);
    }
    // #614: RecordToScalar — same record-decode pattern as
    // AccKind::Record on the input side, primitive-scalar wrap on
    // the output side. Dedicated emitter so the Geom/Raster
    // scaffolding below stays clean.
    if matches!(&shape.accumulator_kind, AccKind::RecordToScalar { .. }) {
        return emit_aggregate_arm_body_record_to_scalar(
            shape, sql_name, arm_indent,
        );
    }
    // #640: RecordToTuple — same record-decode input side; output
    // side serialises the upstream Rust tuple to JSON-array text
    // and wraps it in Duckvalue::Text (None → Duckvalue::Null when
    // optional). Today's surface: mobilitydb `tint-range-aggregate`.
    if matches!(&shape.accumulator_kind, AccKind::RecordToTuple { .. }) {
        return emit_aggregate_arm_body_record_to_tuple(
            shape, sql_name, arm_indent,
        );
    }

    // Accumulator iteration: walk `rows`, skip rows whose
    // streaming arg is NULL, collect raw blobs. Mirrors
    // sqlite-emit's per-row push semantics (#548 W3.2) but
    // performs the whole fold inline rather than across xStep
    // callbacks.
    let (decode_call, resource_ty, err_helper) = match &shape.accumulator_kind {
        AccKind::Geom => (
            "Geometry::from_wkb(b)",
            "Geometry",
            "postgis_err_string",
        ),
        AccKind::Raster => (
            "from_raster_binary(b.as_slice(), \"AGG_NAME\")",
            "Raster",
            "raster_err_string",
        ),
        AccKind::Record { .. } => unreachable!("handled above"),
        AccKind::RecordToScalar { .. } => unreachable!("handled above"),
        AccKind::RecordToTuple { .. } => unreachable!("handled above"),
    };
    let decode_call = decode_call.replace("AGG_NAME", sql_name);

    // For aggregates with extras, latch the constant args from
    // the first non-null row's tail and validate against
    // subsequent rows. PostgreSQL semantics: SQL aggregate
    // constant args MUST be uniform within a group.
    let extras_pre = if shape.extra_args.is_empty() {
        String::new()
    } else {
        format!(
            "{i}let mut extras: Option<Vec<types::Duckvalue>> = None;\n",
        )
    };

    // Constant aggregate args MUST be uniform across rows by the
    // SQL standard; PostgreSQL / DuckDB's planner enforces this
    // upstream of the guest call. We latch the first non-null
    // row's tail and rely on the host's validation rather than
    // re-checking per row (Duckvalue lacks PartialEq, so a
    // post-hoc drift check would require manual variant matching).
    let extras_latch = if shape.extra_args.is_empty() {
        String::new()
    } else {
        format!(
            "{i}    if extras.is_none() {{\n\
             {i}        extras = Some(row[1..].to_vec());\n\
             {i}    }}\n",
        )
    };

    s.push_str(&extras_pre);
    s.push_str(&format!(
        "{i}let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(rows.len());\n\
         {i}for row in &rows {{\n\
         {i}    if row.is_empty() {{ continue; }}\n\
         {i}    if matches!(row[0], types::Duckvalue::Null) {{ continue; }}\n\
         {i}    let bytes = dv_blob(row, 0, \"{sql_name}\")?;\n\
         {i}    blobs.push(bytes.to_vec());\n\
         {extras_latch}{i}}}\n",
    ));

    // Decode accumulated blobs. The decode helper differs by
    // accumulator kind; both produce a `Vec<&Resource>` so the
    // call site below is uniform.
    match &shape.accumulator_kind {
        AccKind::Geom => {
            s.push_str(&format!(
                "{i}let resources: Vec<{resource_ty}> = blobs.iter()\n\
                 {i}    .map(|b| {decode_call})\n\
                 {i}    .collect::<Result<Vec<_>, _>>()\n\
                 {i}    .map_err(|e| types::Duckerror::Invalidargument(\n\
                 {i}        format!(\"{sql_name}: {{}}\", {err_helper}(e))))?;\n\
                 {i}let refs: Vec<&{resource_ty}> = resources.iter().collect();\n",
            ));
        }
        AccKind::Raster => {
            // from_raster_binary returns Result<Raster, types::Duckerror>;
            // it already wraps the error so we propagate with `?`.
            s.push_str(&format!(
                "{i}let resources: Vec<{resource_ty}> = blobs.iter()\n\
                 {i}    .map(|b| {decode_call})\n\
                 {i}    .collect::<Result<Vec<_>, _>>()?;\n\
                 {i}let refs: Vec<&{resource_ty}> = resources.iter().collect();\n",
            ));
        }
        AccKind::Record { .. } => unreachable!("handled by emit_aggregate_arm_body_record above"),
        AccKind::RecordToScalar { .. } => {
            unreachable!("handled by emit_aggregate_arm_body_record_to_scalar above")
        }
        AccKind::RecordToTuple { .. } => {
            unreachable!("handled by emit_aggregate_arm_body_record_to_tuple above")
        }
    }

    // Marshal extras (constant across rows) into Rust-typed
    // bindings. Only the primitive shapes seen in postgis/
    // mobilitydb aggregate signatures are supported; geom/record/
    // enum extras bail with a clear error.
    let mut call_extras: Vec<String> = Vec::new();
    if !shape.extra_args.is_empty() {
        s.push_str(&format!(
            "{i}let extras = extras.unwrap_or_default();\n",
        ));
        for (j, p) in shape.extra_args.iter().enumerate() {
            match p {
                ParamShape::Text => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_text(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::F64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_f64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")? as i32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")? as u32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")? as u64;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Bool => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_bool(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Blob => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_blob(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::OptionNone => {
                    call_extras.push("None".to_string());
                }
                ParamShape::Geom
                | ParamShape::Geog
                | ParamShape::Raster
                | ParamShape::Topology
                | ParamShape::ListGeom
                | ParamShape::WitValueRecord { .. }
                | ParamShape::Enum { .. }
                | ParamShape::ListPrim(_)
                | ParamShape::ListRecord { .. }
                | ParamShape::ListTuple { .. }
                | ParamShape::ListListU8
                | ParamShape::ListListPrim(_) => {
                    // Record / list / enum extras are not part of
                    // the postgis or mobilitydb aggregate surfaces
                    // today. Bail clearly so the unwired-symbol
                    // diagnostic surfaces it.
                    return format!(
                        "{i}Err(types::Duckerror::Unsupported(format!(\
                         \"{sql_name}: aggregate extra arg #{j} shape not wired\")))",
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

    // Encode the upstream result back into a Duckvalue per
    // RetShape. Mirrors the four ret shapes covered by sqlite-emit's
    // `emit_aggregate_finalize_body` plus `Real` / `Int` for any
    // future aggregate whose IR carries a primitive return.
    match shape.ret {
        RetShape::GeomBlob => {
            s.push_str(&format!(
                "{i}let r = {module}::{func}({call_args})\n\
                 {i}    .map_err(|e| types::Duckerror::Invalidargument(\n\
                 {i}        format!(\"{sql_name}: {{}}\", postgis_err_string(e))))?;\n\
                 {i}Ok(types::Duckvalue::Blob(r.as_wkb()))",
            ));
        }
        RetShape::RasterBlob => {
            s.push_str(&format!(
                "{i}let r = {module}::{func}({call_args})\n\
                 {i}    .map_err(|e| types::Duckerror::Invalidargument(\n\
                 {i}        format!(\"{sql_name}: {{}}\", shim_err_string(e))))?;\n\
                 {i}Ok(types::Duckvalue::Blob(r.as_binary()))",
            ));
        }
        RetShape::FirstGeomBlob => {
            s.push_str(&format!(
                "{i}let r: Vec<Geometry> = {module}::{func}({call_args})\n\
                 {i}    .map_err(|e| types::Duckerror::Invalidargument(\n\
                 {i}        format!(\"{sql_name}: {{}}\", postgis_err_string(e))))?;\n\
                 {i}match r.first() {{\n\
                 {i}    Some(g) => Ok(types::Duckvalue::Blob(g.as_wkb())),\n\
                 {i}    None => Ok(types::Duckvalue::Null),\n\
                 {i}}}",
            ));
        }
        RetShape::FirstOptionU32Int => {
            s.push_str(&format!(
                "{i}let r: Vec<Option<u32>> = {module}::{func}({call_args})\n\
                 {i}    .map_err(|e| types::Duckerror::Invalidargument(\n\
                 {i}        format!(\"{sql_name}: {{}}\", postgis_err_string(e))))?;\n\
                 {i}match r.first() {{\n\
                 {i}    Some(Some(v)) => Ok(types::Duckvalue::Int64(*v as i64)),\n\
                 {i}    Some(None) | None => Ok(types::Duckvalue::Null),\n\
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
                 {i}Ok(types::Duckvalue::Blob(__wkb))",
            ));
        }
        _ => {
            s.push_str(&format!(
                "{i}Err(types::Duckerror::Unsupported(format!(\
                 \"{sql_name}: aggregate return shape not wired\")))",
            ));
        }
    }
    s
}

/// #607 Phase 2 + #612 (OQ1): DuckDB-target aggregate body for
/// `AccKind::Record` — mobilitydb temporal-type aggregators.
///
/// DuckDB's `call_aggregate` ABI hands the guest the entire group
/// as `rows: rowbatch` in a single invocation (no init/step/finalize
/// round-trip; no accumulator state across calls). The body:
///
///   1. Walks `rows`, skipping NULL/empty entries. Each non-null
///      row's column 0 carries a `Duckvalue::Complex { type_expr,
///      json }` wit-value carrier; the existing per-input-record
///      `arg_witvalue_<in_snake>` helper unwraps the carrier and
///      `serde_json::from_str::<UPSTREAM>(&cmplx.json)` decodes the
///      payload straight into the upstream record.
///
///   2. Calls the upstream aggregator with `&upstream_vec` — wit-
///      bindgen lowers `list<R>` as `&[R]` so the slice coerces
///      cleanly.
///
///   3. Encodes the `Option<R>` result back via the matching
///      per-output-record `ret_to_witvalue_<out_snake>` helper.
///      `None` returns `Duckvalue::Null` (PostgreSQL aggregate
///      semantics over all-NULL groups).
///
/// Same-record aggregates have `input.kebab_name ==
/// output.kebab_name` so the two snakes match; for the OQ1
/// different-record case (e.g. `tgeompoint-st-extent` returns
/// `option<stbox>` from `list<tgeompoint-sequence>`) the snakes
/// diverge — `collect_referenced_records` must walk both kebabs so
/// both codec sites are emitted.
///
/// Extras (constant args beyond the streaming list) are not
/// supported on the AccKind::Record surface today — no known
/// mobilitydb temporal-aggregate signature uses them. The body
/// errors clearly if one materialises rather than emitting a half-
/// wired body the compiler still accepts.
fn emit_aggregate_arm_body_record(
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
    let in_snake = input.kebab_name.replace('-', "_");
    let out_snake = output.kebab_name.replace('-', "_");

    let mut s = String::new();

    if !shape.extra_args.is_empty() {
        s.push_str(&format!(
            "{i}return Err(types::Duckerror::Invalidargument(\n\
             {i}    format!(\"{sql_name}: AccKind::Record aggregate with extra args not yet wired\")));\n",
        ));
        return s;
    }

    // Walk rows; skip NULL/empty entries; decode column 0 to
    // UPSTREAM via the per-input-record helper.
    s.push_str(&format!(
        "{i}let mut upstream_vec = Vec::with_capacity(rows.len());\n\
         {i}for row in &rows {{\n\
         {i}    if row.is_empty() {{ continue; }}\n\
         {i}    if matches!(row[0], types::Duckvalue::Null) {{ continue; }}\n\
         {i}    upstream_vec.push(arg_witvalue_{in_snake}(row, 0, \"{sql_name}\")?);\n\
         {i}}}\n",
    ));

    // Upstream call + encode. `&upstream_vec` coerces to `&[R]`
    // which wit-bindgen lowers as `list<R>`. Option<R'> result
    // encodes via the OUTPUT record's ret helper (may differ from
    // input for OQ1 / #612 cases).
    s.push_str(&format!(
        "{i}let __r = {module}::{func}(&upstream_vec);\n\
         {i}match __r {{\n\
         {i}    Some(__rec) => ret_to_witvalue_{out_snake}(__rec),\n\
         {i}    None => Ok(types::Duckvalue::Null),\n\
         {i}}}",
    ));
    s
}

/// #614: DuckDB-target aggregate body for
/// `AccKind::RecordToScalar` — mobilitydb trajectory-pattern
/// counters. Walks `rows` and decodes column 0 of each non-null
/// row via the per-input-record `arg_witvalue_<in_snake>` helper
/// (identical to the Record path), then wraps the primitive
/// upstream return in the matching `Duckvalue` variant.
///
/// Extras are latched from the first non-null row's tail (same
/// pattern the Geom/Raster aggregate path uses) and re-decoded
/// before the upstream call.
fn emit_aggregate_arm_body_record_to_scalar(
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
    let in_snake = input.kebab_name.replace('-', "_");

    let mut s = String::new();

    // Extras latch (constant across rows by SQL semantics, so the
    // first non-null row's tail is the canonical extras vector).
    let extras_pre = if shape.extra_args.is_empty() {
        String::new()
    } else {
        format!("{i}let mut extras: Option<Vec<types::Duckvalue>> = None;\n")
    };
    let extras_latch = if shape.extra_args.is_empty() {
        String::new()
    } else {
        format!(
            "{i}    if extras.is_none() {{\n\
             {i}        extras = Some(row[1..].to_vec());\n\
             {i}    }}\n",
        )
    };

    s.push_str(&extras_pre);
    s.push_str(&format!(
        "{i}let mut upstream_vec = Vec::with_capacity(rows.len());\n\
         {i}for row in &rows {{\n\
         {i}    if row.is_empty() {{ continue; }}\n\
         {i}    if matches!(row[0], types::Duckvalue::Null) {{ continue; }}\n\
         {i}    upstream_vec.push(arg_witvalue_{in_snake}(row, 0, \"{sql_name}\")?);\n\
         {extras_latch}{i}}}\n",
    ));

    // Re-decode extras into Rust-typed bindings. Same per-shape
    // arms as the Geom/Raster aggregate finalize path.
    let mut call_extras: Vec<String> = Vec::new();
    if !shape.extra_args.is_empty() {
        s.push_str(&format!(
            "{i}let extras = extras.unwrap_or_default();\n",
        ));
        for (j, p) in shape.extra_args.iter().enumerate() {
            match p {
                ParamShape::Text => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_text(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::F64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_f64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")? as i32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")? as u32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")? as u64;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Bool => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_bool(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Blob => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_blob(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::OptionNone => {
                    call_extras.push("None".to_string());
                }
                ParamShape::Geom
                | ParamShape::Geog
                | ParamShape::Raster
                | ParamShape::Topology
                | ParamShape::ListGeom
                | ParamShape::WitValueRecord { .. }
                | ParamShape::Enum { .. }
                | ParamShape::ListPrim(_)
                | ParamShape::ListRecord { .. }
                | ParamShape::ListTuple { .. }
                | ParamShape::ListListU8
                | ParamShape::ListListPrim(_) => {
                    return format!(
                        "{i}Err(types::Duckerror::Unsupported(format!(\
                         \"{sql_name}: aggregate extra arg #{j} shape not wired\")))",
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
    // variant and emits `Duckvalue::Null` on `None`.
    let some_wrap = match output {
        ScalarReturnKind::F64 | ScalarReturnKind::F32 => {
            "types::Duckvalue::Float64(v as f64)".to_string()
        }
        ScalarReturnKind::Bool => "types::Duckvalue::Boolean(v)".to_string(),
        ScalarReturnKind::U32
        | ScalarReturnKind::S32
        | ScalarReturnKind::U64
        | ScalarReturnKind::S64
        | ScalarReturnKind::U8 => "types::Duckvalue::Int64(v as i64)".to_string(),
    };
    let wrap = if optional {
        format!(
            "match __r {{ Some(v) => Ok({some_wrap}), None => Ok(types::Duckvalue::Null) }}",
        )
    } else {
        match output {
            ScalarReturnKind::F64 | ScalarReturnKind::F32 => {
                format!("Ok(types::Duckvalue::Float64(__r as f64))")
            }
            ScalarReturnKind::Bool => {
                format!("Ok(types::Duckvalue::Boolean(__r))")
            }
            ScalarReturnKind::U32
            | ScalarReturnKind::S32
            | ScalarReturnKind::U64
            | ScalarReturnKind::S64
            | ScalarReturnKind::U8 => format!("Ok(types::Duckvalue::Int64(__r as i64))"),
        }
    };
    s.push_str(&format!(
        "{i}let __r = {module}::{func}({call_args});\n\
         {i}{wrap}",
    ));
    s
}

/// #640: DuckDB-target aggregate body for
/// `AccKind::RecordToTuple` — mobilitydb `tint-range-aggregate`
/// (and any future record-input aggregate returning a primitive
/// tuple). Walks `rows` and decodes column 0 of each non-null row
/// via the per-input-record `arg_witvalue_<in_snake>` helper
/// (identical to the RecordToScalar input path), then serialises
/// the upstream Rust tuple to JSON-array text via
/// `serde_json::to_string` and wraps it in `Duckvalue::Text`.
///
/// Extras are latched + re-decoded with the same shape arms the
/// RecordToScalar path uses; today's surface
/// (`tint-range-aggregate`) takes no extras.
fn emit_aggregate_arm_body_record_to_tuple(
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
    let in_snake = input.kebab_name.replace('-', "_");

    let mut s = String::new();

    // Extras latch (constant across rows by SQL semantics, so the
    // first non-null row's tail is the canonical extras vector).
    let extras_pre = if shape.extra_args.is_empty() {
        String::new()
    } else {
        format!("{i}let mut extras: Option<Vec<types::Duckvalue>> = None;\n")
    };
    let extras_latch = if shape.extra_args.is_empty() {
        String::new()
    } else {
        format!(
            "{i}    if extras.is_none() {{\n\
             {i}        extras = Some(row[1..].to_vec());\n\
             {i}    }}\n",
        )
    };

    s.push_str(&extras_pre);
    s.push_str(&format!(
        "{i}let mut upstream_vec = Vec::with_capacity(rows.len());\n\
         {i}for row in &rows {{\n\
         {i}    if row.is_empty() {{ continue; }}\n\
         {i}    if matches!(row[0], types::Duckvalue::Null) {{ continue; }}\n\
         {i}    upstream_vec.push(arg_witvalue_{in_snake}(row, 0, \"{sql_name}\")?);\n\
         {extras_latch}{i}}}\n",
    ));

    // Re-decode extras into Rust-typed bindings. Same per-shape
    // arms as the RecordToScalar aggregate finalize path.
    let mut call_extras: Vec<String> = Vec::new();
    if !shape.extra_args.is_empty() {
        s.push_str(&format!(
            "{i}let extras = extras.unwrap_or_default();\n",
        ));
        for (j, p) in shape.extra_args.iter().enumerate() {
            match p {
                ParamShape::Text => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_text(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::F64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_f64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")? as i32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")? as u32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_i64(&extras, {j}, \"{sql_name}\")? as u64;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Bool => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_bool(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Blob => {
                    s.push_str(&format!(
                        "{i}let extra{j} = dv_blob(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::OptionNone => {
                    call_extras.push("None".to_string());
                }
                ParamShape::Geom
                | ParamShape::Geog
                | ParamShape::Raster
                | ParamShape::Topology
                | ParamShape::ListGeom
                | ParamShape::WitValueRecord { .. }
                | ParamShape::Enum { .. }
                | ParamShape::ListPrim(_)
                | ParamShape::ListRecord { .. }
                | ParamShape::ListTuple { .. }
                | ParamShape::ListListU8
                | ParamShape::ListListPrim(_) => {
                    return format!(
                        "{i}Err(types::Duckerror::Unsupported(format!(\
                         \"{sql_name}: aggregate extra arg #{j} shape not wired\")))",
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
    // surface). `optional = true` wraps `Some(t)` in Text and emits
    // Duckvalue::Null on `None`.
    let body = if optional {
        format!(
            "match __r {{\n\
             {i}    Some(__t) => {{\n\
             {i}        let __json = serde_json::to_string(&__t)\n\
             {i}            .map_err(|e| types::Duckerror::Internal(format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
             {i}        Ok(types::Duckvalue::Text(__json))\n\
             {i}    }}\n\
             {i}    None => Ok(types::Duckvalue::Null),\n\
             {i}}}",
        )
    } else {
        format!(
            "{{\n\
             {i}    let __json = serde_json::to_string(&__r)\n\
             {i}        .map_err(|e| types::Duckerror::Internal(format!(\"{sql_name}: encode JSON: {{}}\", e)))?;\n\
             {i}    Ok(types::Duckvalue::Text(__json))\n\
             {i}}}",
        )
    };
    s.push_str(&format!(
        "{i}let __r = {module}::{func}({call_args});\n\
         {i}{body}",
    ));
    s
}

// ─── UDTF (table function) dispatch ───
//
// DuckDB's table-function ABI calls the guest's `call-table`
// dispatch with `args: list<duckvalue>` and expects back a
// `result<resultset, duckerror>` where `resultset = list<list<
// duckvalue>>` (outer = rows, inner = columns). Unlike the SQLite
// vtab path, which streams via xColumn / xNext callbacks, the
// DuckDB host eagerly materialises the whole rowset in one shot —
// the bridge marshals args, calls the upstream Rust function ONCE,
// transforms its `Vec<Row>` into a per-row `Vec<Duckvalue>`
// according to `UdtfOutputRow`, and returns the whole table.
//
// The returned body goes inside `<arm_idx>usize => { ... }` in the
// `call_table` match.
pub fn emit_udtf_call_body(
    shape: &UdtfShape,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let mut s = String::new();

    // Detect any params whose shape we can't marshal — if found,
    // short-circuit the whole arm to an error return. Mid-body
    // `return Err(...)` from emit_udtf_param_marshal would still
    // produce code rustc type-checks (unreachable code is still
    // checked), so a partial arg list would mismatch the upstream
    // signature. Bail out cleanly here instead.
    if let Some((idx, shape_name)) = shape.params.iter().enumerate().find_map(|(idx, p)| {
        match p {
            ParamShape::ListGeom => Some((idx, "list<geometry>")),
            ParamShape::ListRecord { .. } => Some((idx, "list<record>")),
            ParamShape::ListTuple { .. } => Some((idx, "list<tuple>")),
            ParamShape::ListPrim(_) => Some((idx, "list<primitive>")),
            ParamShape::ListListU8 => Some((idx, "list<list<u8>>")),
            ParamShape::ListListPrim(_) => Some((idx, "list<list<primitive>>")),
            _ => None,
        }
    }) {
        return format!(
            "{i}Err(types::Duckerror::Unsupported(format!(\
             \"{sql_name}: UDTF param #{idx} ({shape_name}) not wired\")))",
        );
    }

    // ── Param marshalling — covers the subset that postgis +
    // mobilitydb UDTF call sites use today (primitives, Geom /
    // Geog / Raster / Topology, WitValueRecord, Enum).
    let (decls, call_args) =
        emit_udtf_param_marshal(&shape.params, sql_name, i);
    s.push_str(&decls);
    let call_args_str = call_args.join(", ");

    // ── Upstream call. Always fully materialises into Vec<Row>.
    let unwrap = if shape.fallible {
        format!(
            ".map_err(|e| types::Duckerror::Invalidargument(format!(\"{sql_name}: {{}}\", shim_err_string(e))))?"
        )
    } else {
        String::new()
    };
    s.push_str(&format!(
        "{i}let __upstream = {module}::{func}({call_args_str}){unwrap};\n",
    ));

    // ── Row materialiser. Each row → Vec<Duckvalue> (one per
    // visible column). The four output_row variants drive the
    // per-row encoding recipe.
    match &shape.output_row {
        UdtfOutputRow::SingleGeom => {
            s.push_str(&format!(
                "{i}let mut rows: Vec<Vec<types::Duckvalue>> = Vec::with_capacity(__upstream.len());\n\
                 {i}for __g in __upstream.iter() {{\n\
                 {i}    rows.push(alloc::vec![types::Duckvalue::Blob(__g.as_wkb())]);\n\
                 {i}}}\n\
                 {i}Ok(rows)",
            ));
        }
        UdtfOutputRow::SinglePrimitive { affinity } => {
            let (variant, expr) = match affinity {
                ColumnAffinity::Integer => ("Int64", "*__v as i64"),
                ColumnAffinity::Real => ("Float64", "*__v as f64"),
                ColumnAffinity::Text => ("Text", "__v.clone()"),
                ColumnAffinity::Blob => ("Blob", "__v.clone()"),
            };
            s.push_str(&format!(
                "{i}let mut rows: Vec<Vec<types::Duckvalue>> = Vec::with_capacity(__upstream.len());\n\
                 {i}for __v in __upstream.iter() {{\n\
                 {i}    rows.push(alloc::vec![types::Duckvalue::{variant}({expr})]);\n\
                 {i}}}\n\
                 {i}Ok(rows)",
            ));
        }
        UdtfOutputRow::Record { fields } => {
            let mut row_exprs: Vec<String> = Vec::new();
            for f in fields {
                let snake = f.name.replace('-', "_");
                let expr = match f.field_shape {
                    UdtfFieldShape::Int => format!(
                        "types::Duckvalue::Int64(__row.{snake} as i64)"
                    ),
                    UdtfFieldShape::Real => format!(
                        "types::Duckvalue::Float64(__row.{snake} as f64)"
                    ),
                    UdtfFieldShape::Text => format!(
                        "types::Duckvalue::Text(__row.{snake}.clone())"
                    ),
                    UdtfFieldShape::Blob => format!(
                        "types::Duckvalue::Blob(__row.{snake}.clone())"
                    ),
                    UdtfFieldShape::GeomBlob => format!(
                        "types::Duckvalue::Blob(__row.{snake}.as_wkb())"
                    ),
                    UdtfFieldShape::OptionInt => format!(
                        "match __row.{snake} {{ Some(v) => types::Duckvalue::Int64(v as i64), None => types::Duckvalue::Null }}"
                    ),
                    UdtfFieldShape::OptionReal => format!(
                        "match __row.{snake} {{ Some(v) => types::Duckvalue::Float64(v as f64), None => types::Duckvalue::Null }}"
                    ),
                    UdtfFieldShape::OptionText => format!(
                        "match &__row.{snake} {{ Some(v) => types::Duckvalue::Text(v.clone()), None => types::Duckvalue::Null }}"
                    ),
                    UdtfFieldShape::OptionBlob => format!(
                        "match &__row.{snake} {{ Some(v) => types::Duckvalue::Blob(v.clone()), None => types::Duckvalue::Null }}"
                    ),
                    UdtfFieldShape::OptionGeomBlob => format!(
                        "match &__row.{snake} {{ Some(v) => types::Duckvalue::Blob(v.as_wkb()), None => types::Duckvalue::Null }}"
                    ),
                    UdtfFieldShape::Unsupported => {
                        "types::Duckvalue::Null".to_string()
                    }
                };
                row_exprs.push(expr);
            }
            let row_block = row_exprs.join(", ");
            s.push_str(&format!(
                "{i}let mut rows: Vec<Vec<types::Duckvalue>> = Vec::with_capacity(__upstream.len());\n\
                 {i}for __row in __upstream.into_iter() {{\n\
                 {i}    rows.push(alloc::vec![{row_block}]);\n\
                 {i}}}\n\
                 {i}Ok(rows)",
            ));
        }
        UdtfOutputRow::Unwired { reason } => {
            let r = reason.replace('"', "\\\"");
            s.push_str(&format!(
                "{i}Err(types::Duckerror::Unsupported(format!(\"{sql_name}: UDTF row shape unwired: {r}\")))",
            ));
        }
    }

    s
}

/// Marshal UDTF args from `Vec<Duckvalue>` into Rust-typed
/// bindings. Mirrors the marshalling block in
/// `emit_scalar_arm_body` but kept independent so a scalar emit
/// refactor doesn't drag UDTF along. Returns the decl block + the
/// list of `call_args` (each is the Rust expression to pass as
/// the corresponding param at the upstream call site).
fn emit_udtf_param_marshal(
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
            ParamShape::WitValueRecord { kebab_name, .. } => {
                let snake = kebab_name.replace('-', "_");
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_witvalue_{snake}(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Enum {
                wit_module, kebab_name, cases, ..
            } => {
                let type_pascal = kebab_to_pascal(kebab_name);
                let mut arms = String::new();
                for (n, case) in cases.iter().enumerate() {
                    let case_pascal = kebab_to_pascal(case);
                    arms.push_str(&format!(
                        "{i}    {n} => {wit_module}::{type_pascal}::{case_pascal},\n",
                    ));
                }
                let max = cases.len();
                s.push_str(&format!(
                    "{i}let arg{idx} = match dv_i64(&args, {idx}, \"{sql_name}\")? {{\n{arms}{i}    other => return Err(types::Duckerror::Invalidargument(format!(\n\
                     {i}        \"{sql_name}: arg {idx} ({kebab_name}) out of range: {{}} (valid 0..{max})\",\n\
                     {i}        other,\n\
                     {i}    ))),\n\
                     {i}}};\n",
                ));
                call_args.push(format!("arg{idx}"));
            }
            // Param shapes UDTFs in the postgis+mobilitydb surfaces
            // don't use (ListGeom / ListRecord / ListTuple / ListPrim).
            // Emit a runtime error so the surface stays loadable
            // and any future UDTF that does use them surfaces a
            // clear unwired diagnostic.
            ParamShape::ListGeom
            | ParamShape::ListRecord { .. }
            | ParamShape::ListTuple { .. }
            | ParamShape::ListPrim(_)
            | ParamShape::ListListU8
            | ParamShape::ListListPrim(_) => {
                s.push_str(&format!(
                    "{i}return Err(types::Duckerror::Unsupported(format!(\"{sql_name}: UDTF list-param shape not wired\")));\n",
                ));
            }
        }
    }
    (s, call_args)
}

// ─── Window-function dispatch (#661) ───
//
// `call-aggregate-window` is invoked once per output row: the host
// hands the bridge the WHOLE partition's rows + a half-open
// `[frame.start, frame.end)` row range and expects one scalar back
// for the row this frame belongs to.
//
// The postgis cluster surface (`st-cluster-dbscan`, `-kmeans`,
// `-intersecting`, `-within`) is whole-partition compute: one
// upstream call labels every row in the partition. The pilot emit
// recomputes per call (no per-handle caching) and picks the label
// at `frame.end - 1` (the canonical "current row" position for the
// default UNBOUNDED PRECEDING -> CURRENT ROW frame). The result is
// correct under any frame for partition-invariant labellings; a
// future optimisation can cache the labels Vec keyed by handle and
// drop the cache when an empty partition arrives (the contract
// signals end-of-partition that way).
pub fn emit_window_arm_body(
    shape: &WindowShape,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let module = &shape.wit_module;
    let func = &shape.wit_func;
    let mut s = String::new();

    // Empty partition: contract signals end-of-partition / cache
    // drop. With no cache to drop yet (pilot), just return Null.
    s.push_str(&format!(
        "{i}if partition.is_empty() {{\n\
         {i}    return Ok(types::Duckvalue::Null);\n\
         {i}}}\n",
    ));

    // Decode the partition's column-0 (WKB) into a Vec<Geometry>.
    s.push_str(&format!(
        "{i}let mut __geoms: Vec<Geometry> = Vec::with_capacity(partition.len());\n\
         {i}for (__ri, __row) in partition.iter().enumerate() {{\n\
         {i}    if __row.is_empty() {{\n\
         {i}        return Err(types::Duckerror::Invalidargument(format!(\n\
         {i}            \"{sql_name}: partition row {{}} is empty\", __ri\n\
         {i}        )));\n\
         {i}    }}\n\
         {i}    let bytes = match &__row[0] {{\n\
         {i}        types::Duckvalue::Blob(b) => b.as_slice(),\n\
         {i}        types::Duckvalue::Text(t) => t.as_bytes(),\n\
         {i}        types::Duckvalue::Null => {{\n\
         {i}            return Err(types::Duckerror::Invalidargument(format!(\n\
         {i}                \"{sql_name}: partition row {{}} col 0 is NULL\", __ri\n\
         {i}            )));\n\
         {i}        }}\n\
         {i}        _ => {{\n\
         {i}            return Err(types::Duckerror::Invalidargument(format!(\n\
         {i}                \"{sql_name}: partition row {{}} col 0 must be BLOB\", __ri\n\
         {i}            )));\n\
         {i}        }}\n\
         {i}    }};\n\
         {i}    __geoms.push(Geometry::from_wkb(bytes).map_err(|e| {{\n\
         {i}        types::Duckerror::Invalidargument(format!(\n\
         {i}            \"{sql_name}: row {{}} WKB decode: {{}}\", __ri, postgis_err_string(e)\n\
         {i}        ))\n\
         {i}    }})?);\n\
         {i}}}\n",
    ));

    // Decode extras from partition[0][1..] (SQL window constants are
    // uniform across the partition). Only primitive extras supported.
    let mut call_extras: Vec<String> = Vec::new();
    for (j, p) in shape.extra_args.iter().enumerate() {
        let arg_idx = j + 1;
        match p {
            ParamShape::F64 => {
                s.push_str(&format!(
                    "{i}let __extra{j} = dv_f64(&partition[0], {arg_idx}, \"{sql_name}\")?;\n",
                ));
                call_extras.push(format!("__extra{j}"));
            }
            ParamShape::S32 => {
                s.push_str(&format!(
                    "{i}let __extra{j} = dv_i64(&partition[0], {arg_idx}, \"{sql_name}\")? as i32;\n",
                ));
                call_extras.push(format!("__extra{j}"));
            }
            ParamShape::S64 => {
                s.push_str(&format!(
                    "{i}let __extra{j} = dv_i64(&partition[0], {arg_idx}, \"{sql_name}\")?;\n",
                ));
                call_extras.push(format!("__extra{j}"));
            }
            ParamShape::U32 => {
                s.push_str(&format!(
                    "{i}let __extra{j} = dv_i64(&partition[0], {arg_idx}, \"{sql_name}\")? as u32;\n",
                ));
                call_extras.push(format!("__extra{j}"));
            }
            ParamShape::U64 => {
                s.push_str(&format!(
                    "{i}let __extra{j} = dv_i64(&partition[0], {arg_idx}, \"{sql_name}\")? as u64;\n",
                ));
                call_extras.push(format!("__extra{j}"));
            }
            ParamShape::Bool => {
                s.push_str(&format!(
                    "{i}let __extra{j} = dv_bool(&partition[0], {arg_idx}, \"{sql_name}\")?;\n",
                ));
                call_extras.push(format!("__extra{j}"));
            }
            _ => {
                return format!(
                    "{i}return Err(types::Duckerror::Unsupported(format!(\
                     \"{sql_name}: window extra arg #{j} shape not wired\")));",
                );
            }
        }
    }

    let call_extras_lit = if call_extras.is_empty() {
        String::new()
    } else {
        format!(", {}", call_extras.join(", "))
    };

    // Upstream call. All four pilot postgis cluster functions are
    // fallible (result<list<Y>, postgis-error>).
    let map_err = if shape.fallible {
        format!(".map_err(|e| types::Duckerror::Invalidargument(format!(\"{sql_name}: {{}}\", postgis_err_string(e))))?")
    } else {
        String::new()
    };
    s.push_str(&format!(
        "{i}let __geom_refs: Vec<&Geometry> = __geoms.iter().collect();\n\
         {i}let __labels = {module}::{func}(&__geom_refs{call_extras_lit}){map_err};\n",
    ));

    // Pick the current-row label from the frame. Default frame
    // (UNBOUNDED PRECEDING -> CURRENT ROW) makes `frame.end - 1` the
    // current row; for other frames the label is the same anyway
    // (partition-invariant). Clamp to labels.len() - 1 defensively.
    s.push_str(&format!(
        "{i}if __labels.is_empty() {{ return Ok(types::Duckvalue::Null); }}\n\
         {i}let __row_ix = if frame.end == 0 {{ 0usize }} else {{\n\
         {i}    let __e = (frame.end as usize).saturating_sub(1);\n\
         {i}    core::cmp::min(__e, __labels.len() - 1)\n\
         {i}}};\n",
    ));

    // Marshal the per-row label to a Duckvalue per WindowReturn.
    match &shape.returns {
        WindowReturn::OptionU32 => {
            s.push_str(&format!(
                "{i}match __labels[__row_ix] {{\n\
                 {i}    Some(__id) => Ok(types::Duckvalue::Int64(__id as i64)),\n\
                 {i}    None => Ok(types::Duckvalue::Null),\n\
                 {i}}}\n",
            ));
        }
        WindowReturn::U32 => {
            s.push_str(&format!(
                "{i}Ok(types::Duckvalue::Int64(__labels[__row_ix] as i64))\n",
            ));
        }
        WindowReturn::GeomBlob => {
            s.push_str(&format!(
                "{i}Ok(types::Duckvalue::Blob(__labels[__row_ix].as_wkb().into()))\n",
            ));
        }
    }
    s
}
