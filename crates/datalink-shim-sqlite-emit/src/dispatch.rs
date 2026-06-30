//! `sqlite:extension`-shape dispatch-arm emitter.
//!
//! Step 1 of PLAN-shim-codegen-datalink-migration carved the
//! database-agnostic dispatch IR + classifiers out of this module
//! into `datalink_shim_codegen_core::interface_db`. What stays here is the
//! `SqlValue`-aware emit half: rendering the Rust source of each
//! match-arm body (scalar / aggregate step / aggregate finalize)
//! consumed by the bridge crate's `sqlite:extension/minimal`
//! dispatch loop.
//!
//! For every `(DispatchEntry, fallible)` pair produced by
//! `core::interface_db::build_full`, `emit_arm_body` writes a Rust
//! expression that:
//!   1. unpacks the SqlValue `args` slice into the upstream WIT
//!      function's parameter shape (one `arg_text` / `arg_blob` /
//!      `arg_i64` / wit-value decode per `ParamShape`);
//!   2. invokes the WIT function (with `.map_err(... err_string)?`
//!      threaded around fallible calls);
//!   3. wraps the result back into `SqlValue::<variant>` per
//!      `RetShape`.
//!
//! Aggregate step + finalize bodies use a parallel pattern over
//! `AggregateShape` (and its inner `AccKind` state) and live in
//! `emit_aggregate_step_body` / `emit_aggregate_finalize_body`.
//!
//! The `datalink_shim_codegen_core::interface_db` symbols are re-exported under
//! `dispatch::` so existing call sites in `emit_lib.rs` continue
//! to compile without churn.

use shim_bridge_codegen_core::ScalarFn;
use datalink_shim_codegen_core::record_registry::RecordType;
use datalink_shim_codegen_core::wit_parse::WitType;

// Re-export the dispatch IR + classifiers so callers can still
// write `dispatch::ParamShape` / `dispatch::build_full(...)` etc.
// without learning the new `core::interface_db::*` path.
pub use datalink_shim_codegen_core::interface_db::*;

/// Emit the body of one match arm — the code that runs inside
/// `match func_id { N => { ... } }`. The `fallible` flag is the
/// WIT-side `result<T, E>` marker; when true the call result is
/// threaded through `.map_err(... postgis_err_string)?` first.
pub fn emit_arm_body(
    shape: &DispatchShape,
    fallible: bool,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let mut s = String::new();
    let mut call_args = Vec::with_capacity(shape.params.len());
    for (idx, p) in shape.params.iter().enumerate() {
        match p {
            ParamShape::Text => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_text(&args, {idx}, \"{sql_name}\")?;\n"
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::F64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_f64(&args, {idx}, \"{sql_name}\")?;\n"
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::S32 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_i64(&args, {idx}, \"{sql_name}\")? as i32;\n"
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::S64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_i64(&args, {idx}, \"{sql_name}\")?;\n"
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::U32 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_i64(&args, {idx}, \"{sql_name}\")? as u32;\n"
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::U64 => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_i64(&args, {idx}, \"{sql_name}\")? as u64;\n"
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::Bool => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_i64(&args, {idx}, \"{sql_name}\")? != 0;\n"
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::Blob => {
                s.push_str(&format!(
                    "{i}let arg{idx} = arg_blob(&args, {idx}, \"{sql_name}\")?;\n"
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::Geom => {
                s.push_str(&format!(
                    "{i}let arg{idx} = from_wkb(arg_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n"
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Geog => {
                s.push_str(&format!(
                    "{i}let arg{idx} = geog_from_wkb(arg_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n"
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Raster => {
                // Round-490: decode the BLOB via
                // `postgis-raster-types::from-binary`. The emitted
                // helper `from_raster_binary` lives next to
                // `from_wkb` in the bridge's lib.rs prelude.
                s.push_str(&format!(
                    "{i}let arg{idx} = from_raster_binary(arg_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n"
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::Topology => {
                // Round-490: decode the BLOB via
                // `postgis-topology-types::from-bytes`. Helper
                // `from_topology_bytes` lives in the lib.rs prelude.
                s.push_str(&format!(
                    "{i}let arg{idx} = from_topology_bytes(arg_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n"
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::OptionNone => {
                // Pass None for option<T>. Phase 3 doesn't try to
                // promote a SQL value into Some(...) — the
                // interface DB doesn't surface optionality at the
                // SQL layer; functions that need overrides need
                // a hand-curated entry in operator_function_overrides.
                call_args.push("None".to_string());
            }
            ParamShape::ListGeom => {
                // Round 2: list<borrow<geometry>> param.
                //
                // Two flavors based on position:
                // - If this is the LAST param, it's variadic: every
                //   SqlValue::Blob from position `idx` to the end is
                //   decoded into one Geometry and the list is the
                //   collection (covers `st_collect(g1, g2, ...)`).
                // - If there are subsequent params, the SQL surface
                //   only provides ONE blob at this position; we wrap
                //   it as a single-element list (covers
                //   `st_asmvt(geom, layer_name, extent)`).
                let is_variadic = idx + 1 == shape.params.len();
                if is_variadic {
                    s.push_str(&format!(
                        "{i}let arg{idx}_owned: Vec<Geometry> = args[{idx}..]\n\
                         {i}    .iter()\n\
                         {i}    .enumerate()\n\
                         {i}    .map(|(j, v)| match v {{\n\
                         {i}        SqlValue::Blob(b) => Geometry::from_wkb(b.as_slice())\n\
                         {i}            .map_err(|e| format!(\"{sql_name}: arg {{}}: {{}}\", {idx} + j, postgis_err_string(e))),\n\
                         {i}        SqlValue::Text(t) => Geometry::from_wkb(t.as_bytes())\n\
                         {i}            .map_err(|e| format!(\"{sql_name}: arg {{}}: {{}}\", {idx} + j, postgis_err_string(e))),\n\
                         {i}        _ => Err(format!(\"{sql_name}: arg {{}} must be BLOB\", {idx} + j)),\n\
                         {i}    }})\n\
                         {i}    .collect::<Result<Vec<_>, _>>()?;\n\
                         {i}let arg{idx}: Vec<&Geometry> = arg{idx}_owned.iter().collect();\n",
                    ));
                } else {
                    s.push_str(&format!(
                        "{i}let arg{idx}_one = from_wkb(arg_blob(&args, {idx}, \"{sql_name}\")?, \"{sql_name}\")?;\n\
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
                // W3.3 (#543): WIT enums marshal from SqlValue::Integer.
                // The discriminant N (0..len) maps to the Nth case in
                // declaration order. wit-bindgen converts kebab case
                // names to PascalCase; we replicate that conversion
                // here so the match arms reference the same idents.
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
                    "{i}let arg{idx} = match arg_i64(&args, {idx}, \"{sql_name}\")? {{\n{arms}{i}    other => return Err(format!(\n\
                     {i}        \"{sql_name}: arg {idx} ({kebab_name}) out of range: {{}} (valid 0..{max})\",\n\
                     {i}        other,\n\
                     {i}    )),\n\
                     {i}}};\n"
                ));
                call_args.push(format!("arg{idx}"));
            }
            ParamShape::ListRecord { kebab_name, .. } => {
                // W2 Phase 2 (#553): record-element `list<X>` param
                // via JSON-as-TEXT marshaling.
                //
                // SQL passes a JSON-array of record-shaped objects
                // (e.g. `'[{"lower":1,"upper":10,"lower-inc":true,
                // "upper-inc":false}, ...]'`); the codegen-emitted
                // `parse_json_list_record_<snake>` helper in the
                // bridge lib.rs `emit_wit_value_helpers` block calls
                // `serde_json::from_str::<Vec<UPSTREAM>>` and the
                // dispatch arm passes `&arg{idx}` to the WIT call.
                //
                // Wit-bindgen's `additional_derives:
                // [serde::Deserialize]` derives Deserialize on the
                // UPSTREAM record so the parse is direct; no
                // LOCAL→UPSTREAM ciborium round-trip is needed
                // (dispatch is by func_id, not type_id).
                let snake = kebab_name.replace('-', "_");
                s.push_str(&format!(
                    "{i}let arg{idx} = parse_json_list_record_{snake}(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::ListPrim(elem) => {
                // W2 (#542): primitive `list<X>` param via
                // JSON-as-TEXT marshaling.
                //
                // SQL passes a JSON-array literal as the TEXT arg
                // (e.g. `'[1.0, 2.0, 3.0]'`); the codegen-emitted
                // `parse_json_list_<suffix>` helper (in the bridge
                // lib.rs prelude) decodes it into a `Vec<elem>`.
                // wit-bindgen takes `list<X>` non-resource params as
                // `&[X]`, so we pass `&arg<idx>` (Vec deref to slice).
                //
                // Complex elements (records, spans, geometry) are
                // handled by classify_param's non-primitive
                // diagnostic — they need the wit-value codec path
                // (deferred; see plan doc W2.6).
                let suffix = elem.helper_suffix();
                let rust_ty = elem.rust_elem();
                s.push_str(&format!(
                    "{i}let arg{idx}: Vec<{rust_ty}> = parse_json_list_{suffix}(&args, {idx}, \"{sql_name}\")?;\n",
                ));
                call_args.push(format!("&arg{idx}"));
            }
            ParamShape::ListTuple { elements } => {
                // W2 Phase 2 mop-up (#555): `list<tuple<T1, T2,
                // ...>>` param via JSON-as-TEXT marshaling.
                //
                // SQL passes a JSON-array of arrays as the TEXT
                // arg (e.g. `'[[1, 10], [20, 30]]'` for
                // `list<tuple<s32, s32>>`). The codegen-emitted
                // helper `parse_json_list_tuple_<sig>` (e.g.
                // `parse_json_list_tuple_i32_i32`) calls
                // `serde_json::from_str::<Vec<(T1, T2, ...)>>` —
                // serde_json renders Rust tuples as fixed-length
                // JSON arrays so this round-trips cleanly against
                // wit-bindgen's `Vec<(i32, i32)>` binding for the
                // WIT shape.
                let suffix = list_tuple_sig_suffix(elements);
                let elems_joined = elements
                    .iter()
                    .map(|e| e.rust_elem())
                    .collect::<Vec<_>>()
                    .join(", ");
                // Rust 1-tuple needs a trailing comma: `(X,)` ≠ `(X)`.
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
                // Phase E wit-value param marshaling.
                //
                // Each record-typed param routes through a
                // codegen-emitted helper `arg_witvalue_<snake>(...)`
                // which:
                //   1. Unwraps the SqlValue::WitValue payload.
                //   2. Calls the bridge's LOCAL serde-ops decoder
                //      (`<record>_from_canon_cbor`) to recover the
                //      LOCAL Rust struct (proof the codec ran).
                //   3. Ciborium-round-trips LOCAL → UPSTREAM (same
                //      field shape by construction; the bytes
                //      match).
                // The helper is emitted once per record in lib.rs
                // top scope; see emit_lib::emit_wit_value_helpers.
                //
                // wit-bindgen passes imported-record-typed params by
                // value if the upstream type derives Copy
                // (all-primitive records), by `&Record` reference
                // otherwise. The record registry's `is_copy`
                // analysis drives the pass-mode selection here.
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
    // #547 (W3.1): for resource methods, arg 0 is the owned receiver
    // and the rest are the actual method args. The dispatch arm's
    // ParamShape::Topology / Raster decode at idx 0 emits `&argN`
    // into call_args, but methods are invoked on the OWNED receiver
    // (`argN.method(...)`) — strip the leading `&` and split off the
    // receiver expression. Resource-method receivers are never
    // passed by value because `from_*` produces an owned resource.
    // #556 (W3.1 mop-up): constructors look method-shaped in the WIT
    // but compile to `<Pascal>::new(args)` — there's no receiver
    // in the arg list and the upstream Rust type ident is the
    // Pascal-case resource name. The Pascal ident is already in
    // scope at lib.rs top via the `use bindings::...::{Topology}`
    // import that the bridge emits whenever the resource is
    // referenced (topology, raster, geometry — see emit_lib's
    // use-list derivation).
    let call_expr = if let Some(mc) = shape.method_call.as_ref() {
        if mc.is_constructor {
            let pascal = kebab_to_pascal(&mc.resource_kebab);
            format!("{pascal}::new({call_args_str})")
        } else {
            // call_args[0] is `&arg0`; method-call form drops the `&`
            // and reuses the same arg ident.
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
        // Phase E: use the generic `shim_err_string<E: Debug>`
        // helper rather than postgis_err_string so mobilitydb's
        // (and any future shim's) variant errors format cleanly
        // without per-shim pretty-printers.  Postgis still keeps
        // postgis_err_string for the from_wkb-style helpers below.
        format!(
            ".map_err(|e| format!(\"{sql_name}: {{}}\", shim_err_string(e)))?"
        )
    } else {
        String::new()
    };

    let return_expr = match &shape.ret {
        RetShape::Text => format!("Ok(SqlValue::Text({call_expr}{unwrap_chain}))"),
        RetShape::Real => format!("Ok(SqlValue::Real({call_expr}{unwrap_chain}))"),
        RetShape::Int => format!(
            "Ok(SqlValue::Integer({call_expr}{unwrap_chain} as i64))"
        ),
        RetShape::BoolInt => format!(
            "Ok(SqlValue::Integer({call_expr}{unwrap_chain} as i64))"
        ),
        RetShape::GeomBlob => {
            if fallible {
                format!(
                    "let __r = {call_expr}{unwrap_chain};\n{i}Ok(SqlValue::Blob(__r.as_wkb()))"
                )
            } else {
                format!("Ok(SqlValue::Blob({call_expr}.as_wkb()))")
            }
        }
        // Round-490: raster result — encode via the resource's own
        // `as-binary` method (parallel to `Geometry::as_wkb`).
        RetShape::RasterBlob => {
            if fallible {
                format!(
                    "let __r = {call_expr}{unwrap_chain};\n{i}Ok(SqlValue::Blob(__r.as_binary()))"
                )
            } else {
                format!("Ok(SqlValue::Blob({call_expr}.as_binary()))")
            }
        }
        // Round-490: topology result — encode via the resource's own
        // `to-bytes` method.
        RetShape::TopologyBlob => {
            if fallible {
                format!(
                    "let __r = {call_expr}{unwrap_chain};\n{i}Ok(SqlValue::Blob(__r.to_bytes()))"
                )
            } else {
                format!("Ok(SqlValue::Blob({call_expr}.to_bytes()))")
            }
        }
        RetShape::Blob => format!("Ok(SqlValue::Blob({call_expr}{unwrap_chain}))"),
        // Round 2: option<T> returns — Some(v) → SqlValue::T(v), None → Null.
        RetShape::OptionText => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => SqlValue::Text(v),\n\
             {i}    None => SqlValue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionReal => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => SqlValue::Real(v as f64),\n\
             {i}    None => SqlValue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionInt => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => SqlValue::Integer(v as i64),\n\
             {i}    None => SqlValue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => SqlValue::Blob(v),\n\
             {i}    None => SqlValue::Null,\n\
             {i}}})"
        ),
        RetShape::OptionGeomBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => SqlValue::Blob(v.as_wkb()),\n\
             {i}    None => SqlValue::Null,\n\
             {i}}})"
        ),
        // Round-490: option<raster>.
        RetShape::OptionRasterBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => SqlValue::Blob(v.as_binary()),\n\
             {i}    None => SqlValue::Null,\n\
             {i}}})"
        ),
        // Round-490: option<topology>.
        RetShape::OptionTopologyBlob => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => SqlValue::Blob(v.to_bytes()),\n\
             {i}    None => SqlValue::Null,\n\
             {i}}})"
        ),
        RetShape::FirstGeomBlob => format!(
            "{{\n\
             {i}    let __r: Vec<Geometry> = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(g) => Ok(SqlValue::Blob(g.as_wkb())),\n\
             {i}        None => Ok(SqlValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        // Round-490: list<raster> projected to its first element
        // in scalar context (Null if empty).
        RetShape::FirstRasterBlob => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.into_iter().next() {{\n\
             {i}        Some(r) => Ok(SqlValue::Blob(r.as_binary())),\n\
             {i}        None => Ok(SqlValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        // Round-490: list<topology> projected to its first element
        // in scalar context (Null if empty).
        RetShape::FirstTopologyBlob => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.into_iter().next() {{\n\
             {i}        Some(t) => Ok(SqlValue::Blob(t.to_bytes())),\n\
             {i}        None => Ok(SqlValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstOptionU32Int => format!(
            "{{\n\
             {i}    let __r: Vec<Option<u32>> = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(Some(v)) => Ok(SqlValue::Integer(*v as i64)),\n\
             {i}        Some(None) | None => Ok(SqlValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        // Round 3: bbox record → WKB POLYGON envelope blob. We
        // compose `pg_ctor::st_make_envelope(min_x, min_y, max_x,
        // max_y).as_wkb()` so the interface DB's `binary` return
        // type is honoured. Both `st-make-box2d` and
        // `st-box-from-geohash` live in postgis-constructors, so
        // `pg_ctor` is always already in scope when this shape
        // fires.
        RetShape::BboxBlob => format!(
            "{{\n\
             {i}    let __bb = {call_expr}{unwrap_chain};\n\
             {i}    let __env = pg_ctor::st_make_envelope(__bb.min_x, __bb.min_y, __bb.max_x, __bb.max_y);\n\
             {i}    Ok(SqlValue::Blob(__env.as_wkb()))\n\
             {i}}}"
        ),
        // Round 3: `tuple<bool, option<string>, option<geometry>>`
        // returned by `st-is-valid-detail`. Rendered as a
        // PostgreSQL composite-type text representation; the
        // location geometry is converted to WKT via
        // `pg_out::st_as_text` when present. The interface DB
        // says the return is `text`.
        RetShape::IsValidDetailText => format!(
            "{{\n\
             {i}    let (__valid, __reason, __loc) = {call_expr}{unwrap_chain};\n\
             {i}    let __reason_s = __reason.unwrap_or_default();\n\
             {i}    let __loc_s = match __loc {{\n\
             {i}        Some(g) => pg_out::st_as_text(&g),\n\
             {i}        None => alloc::string::String::new(),\n\
             {i}    }};\n\
             {i}    Ok(SqlValue::Text(format!(\n\
             {i}        \"({{}},\\\"{{}}\\\",\\\"{{}}\\\")\",\n\
             {i}        __valid, __reason_s, __loc_s\n\
             {i}    )))\n\
             {i}}}"
        ),
        // Gap G3 (#668): bbox3d return — ISO-WKB `LINESTRING Z`
        // blob whose two vertices are the bbox's min and max
        // corners `(xmin, ymin, zmin) -> (xmax, ymax, zmax)`. The
        // diagonal preserves all six coordinates and is parseable
        // by downstream `st_astext` (and other scalar consumers
        // that decode WKB on entry). Bytes are composed inline
        // because no upstream WIT constructor builds a 3D-envelope
        // geometry today (parallels `BboxBlob` which uses
        // `pg_ctor::st_make_envelope`). The bbox3d record's
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
             {i}    Ok(SqlValue::Blob(__wkb))\n\
             {i}}}"
        ),
        // Phase E: record-typed return — wrap as WitValue. Helper
        // `ret_to_witvalue_<snake>` is emitted once per record at
        // the lib.rs top scope; see `emit_lib::emit_wit_value_helpers`.
        RetShape::WitValueRecord { kebab_name, .. } => {
            let snake = kebab_name.replace('-', "_");
            format!("ret_to_witvalue_{snake}({call_expr}{unwrap_chain})")
        }
        // Phase F (#522): option<bool> — Some(true|false) →
        // SqlValue::Integer; None → SqlValue::Null.
        RetShape::OptionBoolInt => format!(
            "Ok(match {call_expr}{unwrap_chain} {{\n\
             {i}    Some(v) => SqlValue::Integer(v as i64),\n\
             {i}    None => SqlValue::Null,\n\
             {i}}})"
        ),
        // Phase F (#522): option<record> — Some(rec) → encoded via
        // the per-record helper; None → SqlValue::Null. Mirrors the
        // bare `WitValueRecord` shape but unwraps the Option first.
        RetShape::OptionWitValueRecord { kebab_name, .. } => {
            let snake = kebab_name.replace('-', "_");
            format!(
                "match {call_expr}{unwrap_chain} {{\n\
                 {i}    Some(__rec) => ret_to_witvalue_{snake}(__rec),\n\
                 {i}    None => Ok(SqlValue::Null),\n\
                 {i}}}"
            )
        }
        // Phase F (#522): list<record> projected to first element
        // in scalar context (Null if empty). The contract carries
        // no native list variant on `sql-value`, so scalar
        // semantics require collapsing to one element. Multi-row
        // exposure stays on the table-function path.
        RetShape::FirstWitValueRecord { kebab_name, .. } => {
            let snake = kebab_name.replace('-', "_");
            format!(
                "{{\n\
                 {i}    let __r = {call_expr}{unwrap_chain};\n\
                 {i}    let mut __it = __r.into_iter();\n\
                 {i}    match __it.next() {{\n\
                 {i}        Some(__rec) => ret_to_witvalue_{snake}(__rec),\n\
                 {i}        None => Ok(SqlValue::Null),\n\
                 {i}    }}\n\
                 {i}}}"
            )
        }
        // Phase F (#522): list<primitive> projections in scalar
        // context — collapse to first element.
        RetShape::FirstInt => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(v) => Ok(SqlValue::Integer(*v as i64)),\n\
             {i}        None => Ok(SqlValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstReal => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.first() {{\n\
             {i}        Some(v) => Ok(SqlValue::Real(*v as f64)),\n\
             {i}        None => Ok(SqlValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        RetShape::FirstText => format!(
            "{{\n\
             {i}    let __r = {call_expr}{unwrap_chain};\n\
             {i}    match __r.into_iter().next() {{\n\
             {i}        Some(v) => Ok(SqlValue::Text(v)),\n\
             {i}        None => Ok(SqlValue::Null),\n\
             {i}    }}\n\
             {i}}}"
        ),
        // W3.3 (#543): WIT enum return — match the variant against
        // the declared case list and emit the discriminant index as
        // an i64. Mirrors the param-side decode in reverse.
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
                 {i}    Ok(SqlValue::Integer(__disc))\n\
                 {i}}}"
            )
        }
        // W3.4 (#550) + W2 Phase 2 mop-up (#555): nested compound
        // return serialised to JSON TEXT. SQL callers unpack via
        // SQLite's `json_each(...)` / JSON1 ops.
        //
        // Three sub-variants:
        //   - `list<list<X>>` for prim X — serde_json directly on
        //     the upstream `Vec<Vec<X>>`.
        //   - `list<tuple<X1, X2, ...>>` for prim Xi — serde_json
        //     directly on `Vec<(X1, X2, ...)>` (serde renders Rust
        //     tuples as JSON arrays).
        //   - `list<tuple<geometry, f64>>` — hand-built JSON
        //     `[[<wkb-hex>, <value>], ...]` because `Geometry` is
        //     a resource and can't derive `serde::Serialize`. The
        //     WKB-hex projection matches the existing `GeomBlob`
        //     ret shape (same `as_wkb` bytes).
        RetShape::JsonText { kind } => match kind {
            JsonRetKind::ListListPrim(_)
            | JsonRetKind::ListTuplePrim(_)
            | JsonRetKind::TuplePrim(_) => format!(
                "{{\n\
                 {i}    let __r = {call_expr}{unwrap_chain};\n\
                 {i}    let __json = serde_json::to_string(&__r)\n\
                 {i}        .map_err(|e| format!(\"{sql_name}: encode JSON: {{}}\", e))?;\n\
                 {i}    Ok(SqlValue::Text(__json))\n\
                 {i}}}"
            ),
            // W3.5 (#551): `option<tuple<X1, X2, ...>>` — Some →
            // JSON text of the inner tuple; None → SQL NULL. The
            // inner tuple is serde-rendered as a fixed-length JSON
            // array, matching the bare `TuplePrim` path.
            JsonRetKind::OptionTuplePrim(_) => format!(
                "{{\n\
                 {i}    match {call_expr}{unwrap_chain} {{\n\
                 {i}        Some(__t) => {{\n\
                 {i}            let __json = serde_json::to_string(&__t)\n\
                 {i}                .map_err(|e| format!(\"{sql_name}: encode JSON: {{}}\", e))?;\n\
                 {i}            Ok(SqlValue::Text(__json))\n\
                 {i}        }}\n\
                 {i}        None => Ok(SqlValue::Null),\n\
                 {i}    }}\n\
                 {i}}}"
            ),
            // #630: `option<list<R>>` for an all-primitive record R
            // — Some(vec) → JSON array of objects via serde
            // (`wit-bindgen`'s `additional_derives` supplies the
            // `Serialize` impl); None → SQL NULL. Today's surface:
            // mobilitydb `<date|float|int|tstz>-spanset-from-text`.
            JsonRetKind::OptionListPrimRecord(_) => format!(
                "{{\n\
                 {i}    match {call_expr}{unwrap_chain} {{\n\
                 {i}        Some(__v) => {{\n\
                 {i}            let __json = serde_json::to_string(&__v)\n\
                 {i}                .map_err(|e| format!(\"{sql_name}: encode JSON: {{}}\", e))?;\n\
                 {i}            Ok(SqlValue::Text(__json))\n\
                 {i}        }}\n\
                 {i}        None => Ok(SqlValue::Null),\n\
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
                 {i}            .map_err(|e| format!(\"{sql_name}: encode JSON: {{}}\", e))?;\n\
                 {i}        __out.push_str(&__vj);\n\
                 {i}        __out.push(']');\n\
                 {i}    }}\n\
                 {i}    __out.push(']');\n\
                 {i}    Ok(SqlValue::Text(__out))\n\
                 {i}}}"
            ),
        },
        // #564: tuple-pick — call the underlying tuple-returning
        // function and surface element `index` as the matching
        // SqlValue primitive variant. Wired by `tuple_pick_overrides()`
        // (the only entries today are `st_worldtorastercoordcol/row`
        // routing to `st-world-to-raster-coord -> tuple<s32, s32>`).
        //
        // The variant + cast suffix come from the tuple element type
        // (`ListPrimElem`); today's surface only uses `S32` but the
        // arm covers every primitive shape so a future override entry
        // can pick an f64 / bool / string element without another
        // RetShape variant.
        RetShape::TuplePick { index, elem } => {
            let (variant, expr_suffix) = match elem {
                ListPrimElem::S32
                | ListPrimElem::S64
                | ListPrimElem::U32
                | ListPrimElem::U64
                | ListPrimElem::U8
                | ListPrimElem::Bool => ("Integer", format!("__r.{index} as i64")),
                ListPrimElem::F64 | ListPrimElem::F32 => {
                    ("Real", format!("__r.{index} as f64"))
                }
                ListPrimElem::String => ("Text", format!("__r.{index}")),
            };
            format!(
                "{{\n\
                 {i}    let __r = {call_expr}{unwrap_chain};\n\
                 {i}    Ok(SqlValue::{variant}({expr_suffix}))\n\
                 {i}}}"
            )
        }
    };
    s.push_str(i);
    s.push_str(&return_expr);
    s
}

/// Emit the body of an aggregate `step` call. The aggregate state
/// machine maintains a per-context Vec<Vec<u8>> (WKB blobs); each
/// `step` pushes one row's contribution; `finalize` invokes the
/// WIT aggregate function with the full list and returns the result.
///
/// Round 2: for aggregates with extra args (e.g.
/// `st_clusterwithin(geom, distance)`), the step also latches the
/// constant args into a per-context "extras" slot on the first
/// call. The constants are validated against subsequent calls'
/// extras (PostgreSQL semantics: SQL aggregate constant args MUST
/// be uniform across all rows of a single aggregate invocation).
pub fn emit_aggregate_step_body(
    shape: &AggregateShape,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    // #607 Phase 1 + #614 + #640: AccKind::Record / RecordToScalar
    // / RecordToTuple use a different per-row shape (WitValuePayload
    // extracted from SqlValue::WitValue rather than a raw blob). The
    // step body is structurally identical for all three — only
    // finalize diverges — so reuse the existing record step emitter.
    if matches!(
        &shape.accumulator_kind,
        AccKind::Record { .. }
            | AccKind::RecordToScalar { .. }
            | AccKind::RecordToTuple { .. }
    ) {
        return emit_aggregate_step_body_record(shape, sql_name, arm_indent);
    }
    // #548 (W3.2): the push helper varies by accumulator kind.
    // Both per-kind state maps live in the bridge's lib.rs prelude.
    let push = match &shape.accumulator_kind {
        AccKind::Geom => "push_geom_state",
        AccKind::Raster => "push_raster_state",
        AccKind::Record { .. }
        | AccKind::RecordToScalar { .. }
        | AccKind::RecordToTuple { .. } => {
            unreachable!("handled above")
        }
    };
    if shape.extra_args.is_empty() {
        // Simple shape: just push the resource blob.
        format!(
            "{i}let bytes = arg_blob(&args, 0, \"{sql_name}\")?;\n\
             {i}{push}(context_id, bytes.to_vec());\n\
             {i}Ok(())"
        )
    } else {
        // Round 2 shape: push the resource blob AND latch the
        // extras (we just clone the input args[1..] into the state
        // map; finalize will re-decode them).
        format!(
            "{i}let bytes = arg_blob(&args, 0, \"{sql_name}\")?;\n\
             {i}{push}(context_id, bytes.to_vec());\n\
             {i}// Round 2: latch extra constant args (1..) on first step.\n\
             {i}let extras: Vec<SqlValue> = args[1..].to_vec();\n\
             {i}set_or_validate_extras(context_id, extras, \"{sql_name}\")?;\n\
             {i}Ok(())"
        )
    }
}

/// #607 Phase 1: aggregate step body for `AccKind::Record` —
/// mobilitydb temporal-type aggregators. The per-row arg is a
/// `SqlValue::WitValue(payload)`; we extract the `WitValuePayload`
/// and push it onto the per-context witvalue state.  Finalize
/// decodes each via the per-record codec helper.
///
/// Extra args (constants beyond the streaming list) are latched
/// using the same `set_or_validate_extras` shape as the
/// Geom/Raster paths so per-target divergence stays contained.
fn emit_aggregate_step_body_record(
    shape: &AggregateShape,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let extract = format!(
        "{i}let payload = match args.get(0) {{\n\
         {i}    Some(SqlValue::WitValue(p)) => p.clone(),\n\
         {i}    _ => return Err(format!(\"{sql_name}: arg 0 must be WIT-VALUE\")),\n\
         {i}}};\n\
         {i}push_witvalue_state(context_id, payload);\n",
    );
    if shape.extra_args.is_empty() {
        format!("{extract}{i}Ok(())")
    } else {
        format!(
            "{extract}\
             {i}// Round 2: latch extra constant args (1..) on first step.\n\
             {i}let extras: Vec<SqlValue> = args[1..].to_vec();\n\
             {i}set_or_validate_extras(context_id, extras, \"{sql_name}\")?;\n\
             {i}Ok(())"
        )
    }
}

/// Emit the body of an aggregate `finalize` call. Materialises
/// the accumulated geometries, calls the WIT aggregate function,
/// returns the WKB-encoded result.
///
/// Round 2: for aggregates with extra args, also re-decode the
/// latched extras and pass them to the WIT function.
pub fn emit_aggregate_finalize_body(
    shape: &AggregateShape,
    sql_name: &str,
    arm_indent: &str,
) -> String {
    let i = arm_indent;
    let module = &shape.wit_module;
    let func = &shape.wit_func;
    // #607 Phase 1: AccKind::Record finalize is structurally
    // different (lazy decode via per-record codec, no `refs` slice
    // of borrows) so we route to a dedicated emitter and short-
    // circuit before the Geom/Raster scaffolding.
    if let AccKind::Record { .. } = &shape.accumulator_kind {
        return emit_aggregate_finalize_body_record(shape, sql_name, arm_indent);
    }
    // #614: RecordToScalar mirrors the record decode path but
    // emits a primitive-scalar finalize encoder instead of routing
    // through `ret_to_witvalue_<snake>`. Dedicated emitter keeps
    // the Geom/Raster scaffolding below clean.
    if let AccKind::RecordToScalar { .. } = &shape.accumulator_kind {
        return emit_aggregate_finalize_body_record_to_scalar(
            shape, sql_name, arm_indent,
        );
    }
    // #640: RecordToTuple — same record-decode input side; output
    // side serialises the upstream Rust tuple to JSON-array text
    // and wraps it in SqlValue::Text (None → SqlValue::Null when
    // optional). Today's surface: mobilitydb `tint-range-aggregate`.
    if let AccKind::RecordToTuple { .. } = &shape.accumulator_kind {
        return emit_aggregate_finalize_body_record_to_tuple(
            shape, sql_name, arm_indent,
        );
    }
    let mut s = String::new();
    // #548 (W3.2): per-kind accumulator take + decode. Both paths
    // materialise `refs: Vec<&Resource>` so the downstream
    // call-args composition is uniform.
    match &shape.accumulator_kind {
        AccKind::Geom => {
            s.push_str(&format!(
                "{i}let blobs = take_geom_state(context_id);\n\
                 {i}let geoms: Vec<Geometry> = blobs.iter()\n\
                 {i}    .map(|b| Geometry::from_wkb(b))\n\
                 {i}    .collect::<Result<Vec<_>, _>>()\n\
                 {i}    .map_err(|e| format!(\"{sql_name}: {{}}\", postgis_err_string(e)))?;\n\
                 {i}let refs: Vec<&Geometry> = geoms.iter().collect();\n",
            ));
        }
        AccKind::Raster => {
            s.push_str(&format!(
                "{i}let blobs = take_raster_state(context_id);\n\
                 {i}let rasters: Vec<Raster> = blobs.iter()\n\
                 {i}    .map(|b| from_raster_binary(b.as_slice(), \"{sql_name}\"))\n\
                 {i}    .collect::<Result<Vec<_>, _>>()?;\n\
                 {i}let refs: Vec<&Raster> = rasters.iter().collect();\n",
            ));
        }
        AccKind::Record { .. }
        | AccKind::RecordToScalar { .. }
        | AccKind::RecordToTuple { .. } => {
            unreachable!("handled above")
        }
    }

    // Build the extras as Rust-typed bindings if any.
    let mut call_extras: Vec<String> = Vec::new();
    if !shape.extra_args.is_empty() {
        s.push_str(&format!(
            "{i}let extras = take_extras_state(context_id);\n",
        ));
        for (j, p) in shape.extra_args.iter().enumerate() {
            match p {
                ParamShape::Text => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_text(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::F64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_f64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? as i32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? as u32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? as u64;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Bool => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? != 0;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Blob => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_blob(&extras, {j}, \"{sql_name}\")?;\n",
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
                | ParamShape::ListTuple { .. } => {
                    // Extra args that are themselves geometries or
                    // record-typed don't appear in the postgis
                    // aggregate surface, and Phase E doesn't try to
                    // wire record extras for mobilitydb aggregates
                    // either. W3.3 (#543) likewise defers enum-typed
                    // aggregate extras (no known caller). W2 (#542)
                    // primitive `list<X>` extras also have no known
                    // aggregate caller (an aggregate with a
                    // list-typed constant arg would be unusual).
                    // Bail clearly so the dispatcher's unwired-symbol
                    // diagnostic surfaces it.
                    return format!(
                        "{i}Err(format!(\"{sql_name}: aggregate extra arg #{j} shape not wired\"))",
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
                 {i}    .map_err(|e| format!(\"{sql_name}: {{}}\", postgis_err_string(e)))?;\n\
                 {i}Ok(SqlValue::Blob(r.as_wkb()))",
            ));
        }
        // #548 (W3.2): raster mosaic aggregates return a single
        // raster — encode via the resource's `as-binary` method
        // (same path the scalar dispatcher uses for raster returns).
        // raster-error has its own pretty-printer; using
        // `shim_err_string` keeps the wrap generic and works for
        // both postgis raster errors and any future shim.
        RetShape::RasterBlob => {
            s.push_str(&format!(
                "{i}let r = {module}::{func}({call_args})\n\
                 {i}    .map_err(|e| format!(\"{sql_name}: {{}}\", shim_err_string(e)))?;\n\
                 {i}Ok(SqlValue::Blob(r.as_binary()))",
            ));
        }
        RetShape::FirstGeomBlob => {
            s.push_str(&format!(
                "{i}let r: Vec<Geometry> = {module}::{func}({call_args})\n\
                 {i}    .map_err(|e| format!(\"{sql_name}: {{}}\", postgis_err_string(e)))?;\n\
                 {i}match r.first() {{\n\
                 {i}    Some(g) => Ok(SqlValue::Blob(g.as_wkb())),\n\
                 {i}    None => Ok(SqlValue::Null),\n\
                 {i}}}",
            ));
        }
        RetShape::FirstOptionU32Int => {
            s.push_str(&format!(
                "{i}let r: Vec<Option<u32>> = {module}::{func}({call_args})\n\
                 {i}    .map_err(|e| format!(\"{sql_name}: {{}}\", postgis_err_string(e)))?;\n\
                 {i}match r.first() {{\n\
                 {i}    Some(Some(v)) => Ok(SqlValue::Integer(*v as i64)),\n\
                 {i}    Some(None) | None => Ok(SqlValue::Null),\n\
                 {i}}}",
            ));
        }
        // Gap G3 (#668): bbox3d return — ISO-WKB `LINESTRING Z`
        // blob whose two vertices are the bbox's min and max
        // corners `(xmin, ymin, zmin) -> (xmax, ymax, zmax)`. The
        // diagonal preserves all six coordinates and lets the
        // downstream `st_astext` (and other scalar consumers that
        // decode WKB on entry) parse the aggregate result as a
        // standard WKB geometry. Today's only producer is the
        // `st_3dextent` aggregate (`postgis-aggregates::st-extent-threed`),
        // which is non-fallible (returns `bbox3d` directly, no
        // `result<...>`), so no `.map_err(...)` thread.
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
                 {i}Ok(SqlValue::Blob(__wkb))",
            ));
        }
        _ => {
            // Other ret shapes for aggregates aren't yet wired;
            // emit a stub error so the surface stays loadable.
            s.push_str(&format!(
                "{i}Err(format!(\"{sql_name}: aggregate return shape not wired\"))",
            ));
        }
    }
    s
}

/// #607 Phase 1: aggregate finalize body for `AccKind::Record` —
/// mobilitydb temporal-type aggregators.
///
/// Pattern (lazy decode per DD1):
///   1. `take_witvalue_state` recovers the per-row payload list.
///   2. For each payload, the existing `arg_witvalue_<snake>`
///      decoder runs (slice trick: synthesise a one-element
///      `[SqlValue::WitValue(p)]` so the helper's args/idx
///      signature matches). This produces an UPSTREAM record.
///   3. The upstream aggregator is called with the `&Vec<R>`
///      (wit-bindgen lowers `list<R>` as `&[R]`).
///   4. The Option<R> result is encoded back via the existing
///      `ret_to_witvalue_<snake>` helper — the same encoder the
///      `OptionWitValueRecord` scalar return shape uses.
///
/// Extras (constant args beyond the streaming list) are
/// supported by the same `take_extras_state` flow as the
/// Geom/Raster path. Phase 1 pilot has no temporal aggregate
/// with extras; this branch errors clearly if one shows up.
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
    let in_snake = input.kebab_name.replace('-', "_");
    let out_snake = output.kebab_name.replace('-', "_");

    let mut s = String::new();
    // Drain the per-context witvalue accumulator, then decode each
    // payload to UPSTREAM via the per-input-record helper.
    s.push_str(&format!(
        "{i}let payloads = take_witvalue_state(context_id);\n\
         {i}let mut upstream_vec = Vec::with_capacity(payloads.len());\n\
         {i}for pw in payloads {{\n\
         {i}    // Slice trick: arg_witvalue_<snake> takes the standard\n\
         {i}    // `&[SqlValue]` / idx signature. Synthesise a one-element\n\
         {i}    // slice so we can reuse the helper from the aggregate\n\
         {i}    // finalize site.\n\
         {i}    let __args = [SqlValue::WitValue(pw)];\n\
         {i}    upstream_vec.push(arg_witvalue_{in_snake}(&__args, 0, \"{sql_name}\")?);\n\
         {i}}}\n",
    ));

    // Phase 1 pilot doesn't support record-typed aggregates with
    // extras (no known surface today). Bail with a clear error
    // when one materialises — better than emitting a half-wired
    // body the compiler still accepts.
    if !shape.extra_args.is_empty() {
        s.push_str(&format!(
            "{i}return Err(format!(\"{sql_name}: AccKind::Record aggregate with extra args not yet wired\"));\n",
        ));
        return s;
    }

    // Upstream takes `list<R>` which wit-bindgen lowers to `&[R]`;
    // `&upstream_vec` coerces to a slice cleanly. The Option<R'>
    // result encodes back via the output-record's ret helper, which
    // may differ from the input record (#612 OQ1: e.g. decode via
    // `arg_witvalue_tgeompoint_sequence`, encode via
    // `ret_to_witvalue_stbox`).
    s.push_str(&format!(
        "{i}let __r = {module}::{func}(&upstream_vec);\n\
         {i}match __r {{\n\
         {i}    Some(__rec) => ret_to_witvalue_{out_snake}(__rec),\n\
         {i}    None => Ok(SqlValue::Null),\n\
         {i}}}",
    ));
    s
}

/// #614: aggregate finalize body for `AccKind::RecordToScalar` —
/// mobilitydb trajectory-pattern counters. The input side mirrors
/// `emit_aggregate_finalize_body_record` (drain the witvalue
/// accumulator, decode each payload to UPSTREAM via the per-input-
/// record helper); the output side wraps the primitive return in
/// the matching `SqlValue` variant rather than routing through
/// `ret_to_witvalue_<snake>`.
///
/// Extras (constants like `distance-threshold`, `min-duration-us`,
/// `min-size`) are supported: the step body latches `args[1..]`
/// via `set_or_validate_extras` on first invocation, and finalize
/// re-decodes them through the same per-`ParamShape` arms the
/// Geom/Raster aggregate path uses.
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
    let in_snake = input.kebab_name.replace('-', "_");

    let mut s = String::new();
    // Drain the per-context witvalue accumulator + decode each
    // payload to UPSTREAM. Identical to the Record path.
    s.push_str(&format!(
        "{i}let payloads = take_witvalue_state(context_id);\n\
         {i}let mut upstream_vec = Vec::with_capacity(payloads.len());\n\
         {i}for pw in payloads {{\n\
         {i}    let __args = [SqlValue::WitValue(pw)];\n\
         {i}    upstream_vec.push(arg_witvalue_{in_snake}(&__args, 0, \"{sql_name}\")?);\n\
         {i}}}\n",
    ));

    // Re-decode the extras into Rust-typed bindings; mirrors the
    // Geom/Raster aggregate finalize extras-decode block. Only the
    // primitive ParamShape arms are reachable here (the only known
    // RecordToScalar callers — mobilitydb's three trajectory
    // counters — take f64/s64/u32 extras).
    let mut call_extras: Vec<String> = Vec::new();
    if !shape.extra_args.is_empty() {
        s.push_str(&format!(
            "{i}let extras = take_extras_state(context_id);\n",
        ));
        for (j, p) in shape.extra_args.iter().enumerate() {
            match p {
                ParamShape::Text => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_text(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::F64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_f64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? as i32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? as u32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? as u64;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Bool => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? != 0;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Blob => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_blob(&extras, {j}, \"{sql_name}\")?;\n",
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
                | ParamShape::ListTuple { .. } => {
                    return format!(
                        "{i}Err(format!(\"{sql_name}: aggregate extra arg #{j} shape not wired\"))",
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

    // Wrap the primitive return in the matching SqlValue variant.
    // SQLite has no native bool — Bool collapses to Integer 0/1
    // (mirrors the scalar `RetShape::BoolInt` arm). Float widths
    // collapse to f64; integer widths collapse to i64.
    //
    // #637: `optional = true` wraps the upstream return in `match __r
    // { Some(v) => <native scalar>, None => SqlValue::Null }`. The
    // inner Some-branch wrap mirrors the bare-primitive arm.
    let some_wrap = match output {
        ScalarReturnKind::F64 | ScalarReturnKind::F32 => {
            "SqlValue::Real(v as f64)".to_string()
        }
        ScalarReturnKind::Bool => {
            "SqlValue::Integer(if v { 1 } else { 0 })".to_string()
        }
        ScalarReturnKind::U32
        | ScalarReturnKind::S32
        | ScalarReturnKind::U64
        | ScalarReturnKind::S64
        | ScalarReturnKind::U8 => "SqlValue::Integer(v as i64)".to_string(),
    };
    let wrap = if optional {
        format!(
            "match __r {{ Some(v) => Ok({some_wrap}), None => Ok(SqlValue::Null) }}",
        )
    } else {
        match output {
            ScalarReturnKind::F64 | ScalarReturnKind::F32 => {
                format!("Ok(SqlValue::Real(__r as f64))")
            }
            ScalarReturnKind::Bool => {
                format!("Ok(SqlValue::Integer(if __r {{ 1 }} else {{ 0 }}))")
            }
            ScalarReturnKind::U32
            | ScalarReturnKind::S32
            | ScalarReturnKind::U64
            | ScalarReturnKind::S64
            | ScalarReturnKind::U8 => format!("Ok(SqlValue::Integer(__r as i64))"),
        }
    };
    s.push_str(&format!(
        "{i}let __r = {module}::{func}({call_args});\n\
         {i}{wrap}",
    ));
    s
}

/// #640: aggregate finalize body for `AccKind::RecordToTuple` —
/// mobilitydb `tint-range-aggregate` (and any future record-input
/// aggregate returning a primitive tuple). Input side mirrors
/// `emit_aggregate_finalize_body_record_to_scalar` (drain the
/// witvalue accumulator, decode each payload to UPSTREAM via the
/// per-input-record helper); output side serialises the upstream
/// Rust tuple to a JSON-array text via `serde_json::to_string` and
/// wraps the result in `SqlValue::Text`. The `optional = true`
/// path emits `None → SqlValue::Null` / `Some(t) → JSON text`.
///
/// `output` (the Vec<ScalarReturnKind>) is informational only at
/// this layer: serde-derives auto-implement `Serialize` for tuples
/// of `Serialize` types, so the upstream Rust tuple (e.g.
/// `(i64, i64)` for `option<tuple<s64, s64>>`) serialises directly
/// without per-element matching. The element kinds are captured by
/// the classifier so a future emit could pick a typed render
/// (e.g. boolean → JSON `true`/`false`) without revisiting the
/// classifier.
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
    let in_snake = input.kebab_name.replace('-', "_");

    let mut s = String::new();
    // Drain the per-context witvalue accumulator + decode each
    // payload to UPSTREAM. Identical to the RecordToScalar path.
    s.push_str(&format!(
        "{i}let payloads = take_witvalue_state(context_id);\n\
         {i}let mut upstream_vec = Vec::with_capacity(payloads.len());\n\
         {i}for pw in payloads {{\n\
         {i}    let __args = [SqlValue::WitValue(pw)];\n\
         {i}    upstream_vec.push(arg_witvalue_{in_snake}(&__args, 0, \"{sql_name}\")?);\n\
         {i}}}\n",
    ));

    // Re-decode the extras into Rust-typed bindings; mirrors the
    // RecordToScalar finalize extras-decode block. Only the
    // primitive ParamShape arms are reachable here (today's surface
    // — mobilitydb `tint-range-aggregate` — takes no extras).
    let mut call_extras: Vec<String> = Vec::new();
    if !shape.extra_args.is_empty() {
        s.push_str(&format!(
            "{i}let extras = take_extras_state(context_id);\n",
        ));
        for (j, p) in shape.extra_args.iter().enumerate() {
            match p {
                ParamShape::Text => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_text(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::F64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_f64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? as i32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::S64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")?;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U32 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? as u32;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::U64 => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? as u64;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Bool => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_i64(&extras, {j}, \"{sql_name}\")? != 0;\n",
                    ));
                    call_extras.push(format!("extra{j}"));
                }
                ParamShape::Blob => {
                    s.push_str(&format!(
                        "{i}let extra{j} = arg_blob(&extras, {j}, \"{sql_name}\")?;\n",
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
                | ParamShape::ListTuple { .. } => {
                    return format!(
                        "{i}Err(format!(\"{sql_name}: aggregate extra arg #{j} shape not wired\"))",
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
    // SQL NULL on `None`.
    let body = if optional {
        format!(
            "match __r {{\n\
             {i}    Some(__t) => {{\n\
             {i}        let __json = serde_json::to_string(&__t)\n\
             {i}            .map_err(|e| format!(\"{sql_name}: encode JSON: {{}}\", e))?;\n\
             {i}        Ok(SqlValue::Text(__json))\n\
             {i}    }}\n\
             {i}    None => Ok(SqlValue::Null),\n\
             {i}}}",
        )
    } else {
        format!(
            "{{\n\
             {i}    let __json = serde_json::to_string(&__r)\n\
             {i}        .map_err(|e| format!(\"{sql_name}: encode JSON: {{}}\", e))?;\n\
             {i}    Ok(SqlValue::Text(__json))\n\
             {i}}}",
        )
    };
    s.push_str(&format!(
        "{i}let __r = {module}::{func}({call_args});\n\
         {i}{body}",
    ));
    s
}

/// W3.3 (#543): kebab-case → PascalCase for enum-type and -case
/// names. wit-bindgen's generator does the same conversion when
/// emitting Rust enum idents, so the dispatch arm references
/// `<module>::PixelType::Bool1` etc. consistently with the
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

