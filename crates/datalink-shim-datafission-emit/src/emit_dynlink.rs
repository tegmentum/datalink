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
//! Aggregates / UDTFs / window functions: emitted as empty
//! registries. They can be added in a follow-up.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use shim_bridge_codegen_core::BridgePlan;
use datalink_shim_codegen_core::interface_db::{self, DispatchEntry, ParamShape, RetShape};

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
/// Scalar-first cut: only identity + metadata + scalar-function-
/// registry are exported. Per `datafission:extension@1.0.0`'s doc
/// comment, a plugin that doesn't provide a given category simply
/// omits it from its world declaration; the loader detects what's
/// exported via component-model introspection at load time.
world bridge {{
    // Compose:dynlink linker — the only shim-side import.
    import compose:dynlink/linker@0.1.0;

    // Datafission composite exports (the minimum surface needed for
    // scalar-function dispatch).
    import datafission:extension/logging@1.0.0;
    export datafission:extension/identity@1.0.0;
    export datafission:sql-extension-plugin/metadata@1.2.0;
    export datafission:function-plugin/scalar-function-registry@1.0.0;
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
    let (scalar_entries, _unwired) =
        interface_db::build_full(plan, &shim_wit_dir, &[])?;

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
        for e in &skipped[..skipped.len().min(5)] {
            eprintln!("  - {}", e.sql_name);
        }
        if skipped.len() > 5 {
            eprintln!("  - ... ({} more)", skipped.len() - 5);
        }
    }

    // Extension package version for identity::api_version().
    let api_version = "1.0.0".to_string();
    let provider_id = &opts.provider_id;

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
        let body = emit_scalar_arm_body(sql, &entry.shape.params, &entry.shape.ret, *fallible);
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

use bindings::exports::datafission::extension::identity;
use bindings::exports::datafission::sql_extension_plugin::metadata as se_meta;
use bindings::exports::datafission::function_plugin::scalar_function_registry;

use bindings::compose::dynlink::linker;

const PROVIDER_ID: &str = "{provider_id}";

fn resolve() -> Result<linker::Instance, ftypes::FunctionError> {{
    linker::resolve_by_id(&PROVIDER_ID.to_string())
        .map_err(|e| ftypes::FunctionError::ExecutionError(format!("dynlink resolve('{{}}'): {{:?}}", PROVIDER_ID, e)))
}}

// -----------------------------------------------------------
// CBOR envelope (mirrors provider crate's Request/Response).
// -----------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
enum CborValue {{
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Text(String),
    Bytes(#[serde(with = "serde_bytes")] Vec<u8>),
}}

#[derive(Debug, Clone, serde::Serialize)]
struct Request {{
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

// Response `ok` uses the same single-key-map disambiguation the
// provider crate emits.
#[derive(Debug, Clone)]
enum ResponseValue {{
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}}

impl<'de> serde::Deserialize<'de> for ResponseValue {{
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {{
        use serde::de::{{Error, MapAccess, Visitor}};
        struct V;
        impl<'de> Visitor<'de> for V {{
            type Value = ResponseValue;
            fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {{
                f.write_str("a single-key CBOR map for a Response::Ok value")
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
                    other => return Err(A::Error::custom(alloc::format!("unknown ResponseValue tag: {{}}", other))),
                }};
                if let Some(extra) = m.next_key::<String>()? {{
                    return Err(A::Error::custom(alloc::format!("extra key {{}}", extra)));
                }}
                Ok(v)
            }}
        }}
        d.deserialize_map(V)
    }}
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
    ))
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
            | ParamShape::Geom | ParamShape::Raster | ParamShape::Topology
    )) && matches!(ret,
        RetShape::Text | RetShape::Real | RetShape::Int
            | RetShape::Blob | RetShape::BoolInt
            | RetShape::GeomBlob | RetShape::RasterBlob | RetShape::TopologyBlob
    )
}

fn param_to_logicaltype_lit(p: &ParamShape) -> String {
    match p {
        ParamShape::Blob
        | ParamShape::Geom
        | ParamShape::Raster
        | ParamShape::Topology => "ftypes::LogicalType::Binary".to_string(),
        ParamShape::F64 => "ftypes::LogicalType::Float64".to_string(),
        ParamShape::S32 | ParamShape::S64 | ParamShape::U32 | ParamShape::U64 => {
            "ftypes::LogicalType::Int64".to_string()
        }
        ParamShape::Bool => "ftypes::LogicalType::Boolean".to_string(),
        ParamShape::Text => "ftypes::LogicalType::Utf8".to_string(),
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
        RetShape::Text => "ftypes::LogicalType::Utf8".to_string(),
        RetShape::Real => "ftypes::LogicalType::Float64".to_string(),
        RetShape::Int => "ftypes::LogicalType::Int64".to_string(),
        RetShape::Blob
        | RetShape::GeomBlob
        | RetShape::RasterBlob
        | RetShape::TopologyBlob => "ftypes::LogicalType::Binary".to_string(),
        RetShape::BoolInt => "ftypes::LogicalType::Boolean".to_string(),
        _ => "ftypes::LogicalType::Binary".to_string(),
    }
}

fn ret_to_logicaltype_lit_stub(r: &RetShape) -> String {
    let _ = r;
    "ftypes::LogicalType::Binary".to_string()
}

fn emit_scalar_arm_body(
    sql: &str,
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
            | ParamShape::Raster
            | ParamShape::Topology => format!(
                "                let {ident} = CborValue::Bytes(dfv_blob(&args, {i}, \"{sql}\")?);\n"
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
        "                let resp = call(\"{sql}\", payload_args)?;\n"
    ));
    // Wrap response into ScalarValue.
    let wrap = match ret {
        RetShape::Text => "                match resp { ResponseValue::Text(s) => Ok(ftypes::ScalarValue::Utf8(s)), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        RetShape::Real => "                match resp { ResponseValue::Float(f) => Ok(ftypes::ScalarValue::Float64(f)), ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Float64(i as f64)), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        RetShape::Int => "                match resp { ResponseValue::Int(i) => Ok(ftypes::ScalarValue::Int64(i)), ResponseValue::Uint(u) => Ok(ftypes::ScalarValue::Int64(u as i64)), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        RetShape::Blob
        | RetShape::GeomBlob
        | RetShape::RasterBlob
        | RetShape::TopologyBlob => "                match resp { ResponseValue::Bytes(b) => Ok(ftypes::ScalarValue::Binary(b)), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        RetShape::BoolInt => "                match resp { ResponseValue::Bool(b) => Ok(ftypes::ScalarValue::Boolean(b)), other => Err(ftypes::FunctionError::ExecutionError(alloc::format!(\"unexpected: {:?}\", other))) }",
        _ => "                Err(ftypes::FunctionError::ExecutionError(\"unsupported return shape\".to_string()))",
    };
    lines.push_str(wrap);
    lines
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
