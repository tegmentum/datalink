//! Dynlink-mode duckdb bridge emitter (Phase A, §A.4 Option 1).
//!
//! Emits a bridge crate that dispatches every SQL arm through
//! `compose:dynlink/linker` — CBOR envelope in / CBOR envelope out
//! against a resident provider identified by `opts.provider_id` —
//! instead of the wac-plug-linked WIT interfaces the sibling
//! `datalink-shim-duckdb-emit` produces.
//!
//! Following §A.4 Option 1, every scalar arm is opaque: the
//! `callback-dispatch::call-scalar` row-major singleton path
//! marshals its `duckvalue` args into CBOR (blob-preferred, all
//! primitives faithfully preserved), forwards the request through
//! `linker.resolve-by-id + invoke`, and rewraps the response into
//! a `duckvalue`. The columnar hot paths and every other export
//! (aggregate / cast / table / pragma / index / files / …) are
//! stubbed with `duckerror::unsupported` at Phase A scope.
//!
//! Wire discipline mirrors
//! `postgis-wasm/crates/provider/src/envelope.rs`:
//!
//! ```ignore
//! Request  { v: 1, args: Vec<CborValue> }
//! Response { ok:  Option<CborValue>, err: Option<String> }
//! ```
//!
//! The `CborValue` variants are serialised at their bare CBOR
//! type via a manual `Serialize` — matching the provider-side
//! envelope. See the deep note on `Response::ok` null-collapse
//! rehydration in the emitted `call` function.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::spatial_catalog::{Catalog, FnKind, LeavesOverlay};
use crate::DynlinkOptions;

/// Emit a Dynlink-mode duckdb bridge crate under `out_dir`.
///
/// Produced layout:
///
/// ```text
/// Cargo.toml
/// README.md
/// src/lib.rs
/// wit/world.wit
/// wit/deps/compose-dynlink/*.wit   (copied from datalink-dynlink)
/// wit/deps/sys-compose/*.wit
/// wit/deps/duckdb/*.wit            (copied from ~/git/ducklink/wit/duckdb-extension)
/// ```
pub fn emit_dynlink(
    catalog: &Catalog,
    _leaves_overlay: Option<&LeavesOverlay>,
    out_dir: &Path,
    opts: &DynlinkOptions,
) -> Result<()> {
    fs::create_dir_all(out_dir.join("src"))?;
    fs::create_dir_all(out_dir.join("wit/deps"))?;

    let leaves = catalog
        .resolve(&opts.target)
        .with_context(|| format!("resolving target '{}'", opts.target))?;
    let functions = catalog.functions_for(&leaves);

    let crate_name = crate_name_for(opts);
    let version = if catalog.meta.version.is_empty() {
        "0.1.0".to_string()
    } else {
        catalog.meta.version.clone()
    };

    fs::write(out_dir.join("Cargo.toml"), cargo_toml(&crate_name, &version))?;
    fs::write(out_dir.join("wit/world.wit"), world_wit(&opts.sub_ext))?;
    populate_deps(&out_dir.join("wit/deps"))?;

    let lib_src = lib_rs(
        &opts.provider_id,
        &opts.extension_root,
        &catalog.meta.extension,
        &version,
        functions.iter().collect::<Vec<_>>().as_slice(),
    );
    fs::write(out_dir.join("src/lib.rs"), lib_src)?;

    fs::write(
        out_dir.join("README.md"),
        readme(&crate_name, &opts.provider_id, &opts.sub_ext, &opts.target),
    )?;

    Ok(())
}

fn crate_name_for(opts: &DynlinkOptions) -> String {
    let sub: String = opts
        .sub_ext
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{sub}-duckdb-bridge-dynlink")
}

fn cargo_toml(crate_name: &str, version: &str) -> String {
    format!(
        r#"[package]
name = "{crate_name}"
version = "{version}"
edition = "2021"
description = "Phase A dynlink-mode duckdb bridge — routes SQL dispatch through compose:dynlink/linker against a resident provider."
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
serde_json = {{ version = "1", default-features = false, features = ["alloc"] }}

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
strip = true
"#,
    )
}

fn world_wit(sub_ext: &str) -> String {
    let pkg = sub_ext
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    format!(
        r#"package duckdb-bridge:{pkg}@0.1.0;

/// Phase A dynlink-mode duckdb bridge.
///
/// The bridge imports `compose:dynlink/linker` for outbound
/// dispatch to a resident provider and exports the canonical
/// duckdb:extension@4.0.0 guest + callback-dispatch pair (the
/// same shape every DuckDB extension component declares). Only
/// `callback-dispatch::call-scalar` is wired to the provider at
/// Phase A scope; every other method returns
/// `duckerror::unsupported` so the composite world instantiates
/// against `duckdb-loader` without a missing-export failure.
world bridge {{
    import compose:dynlink/linker@0.1.0;

    // Minimal contract-side imports — the guest needs `runtime`
    // to register its scalars during `load`.
    import duckdb:extension/runtime@4.0.0;
    import duckdb:extension/logging@4.0.0;

    export duckdb:extension/guest@4.0.0;
    export duckdb:extension/callback-dispatch@4.0.0;
}}
"#,
    )
}

fn populate_deps(deps_dir: &Path) -> Result<()> {
    // compose-dynlink + sys-compose from datalink-dynlink.
    let dynlink_root = std::env::var("DATALINK_DYNLINK_WIT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("git/datalink/crates/datalink-dynlink/wit")
        });
    let compose_dynlink_from = dynlink_root.join("compose-dynlink");
    let sys_compose_from = dynlink_root.join("compose-dynlink/deps/sys-compose");
    if !compose_dynlink_from.is_dir() {
        return Err(anyhow!(
            "compose:dynlink WIT source missing: {} (set DATALINK_DYNLINK_WIT)",
            compose_dynlink_from.display()
        ));
    }
    if !sys_compose_from.is_dir() {
        return Err(anyhow!(
            "sys:compose WIT source missing: {}",
            sys_compose_from.display()
        ));
    }
    let compose_dst = deps_dir.join("compose-dynlink");
    fs::create_dir_all(&compose_dst)?;
    for name in ["package.wit", "linker.wit", "endpoint.wit"] {
        let f = compose_dynlink_from.join(name);
        if f.is_file() {
            copy_kebab_fixed(&f, &compose_dst.join(name))?;
        }
    }
    copy_tree_kebab_fixed(&sys_compose_from, &deps_dir.join("sys-compose"))?;

    // duckdb:extension package. Every .wit file under
    // ~/git/ducklink/wit/duckdb-extension/ carries the same
    // `package duckdb:extension@4.0.0;` header; copying the
    // interface files preserves that. The `worlds/` subdirectory
    // is skipped — the bridge synthesises its own world.
    let duckdb_from = std::env::var("DUCKLINK_WIT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("git/ducklink/wit/duckdb-extension")
        });
    if !duckdb_from.is_dir() {
        return Err(anyhow!(
            "duckdb:extension WIT source missing: {} (set DUCKLINK_WIT)",
            duckdb_from.display()
        ));
    }
    let duckdb_dst = deps_dir.join("duckdb");
    fs::create_dir_all(&duckdb_dst)?;
    for entry in fs::read_dir(&duckdb_from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src = entry.path();
        if ty.is_file() {
            let dst = duckdb_dst.join(entry.file_name());
            copy_kebab_fixed(&src, &dst)?;
        }
        // Skip the `worlds/` subdirectory; the bridge world lives
        // at wit/world.wit and is synthesised above.
    }
    Ok(())
}

fn copy_kebab_fixed(src: &Path, dst: &Path) -> Result<()> {
    let bytes = fs::read(src)?;
    if src.extension().and_then(|e| e.to_str()) == Some("wit") {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let fixed = datalink_shim_codegen_core::kebab_fix::kebab_fix_wit(&text);
        fs::write(dst, fixed)?;
    } else {
        fs::write(dst, bytes)?;
    }
    Ok(())
}

fn copy_tree_kebab_fixed(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if ty.is_dir() {
            copy_tree_kebab_fixed(&src, &dst)?;
        } else if ty.is_file() {
            copy_kebab_fixed(&src, &dst)?;
        }
    }
    Ok(())
}

fn readme(crate_name: &str, provider_id: &str, sub_ext: &str, target: &str) -> String {
    format!(
        "# {crate_name}\n\
         \n\
         Phase A dynlink-mode duckdb bridge for `{sub_ext}` (target `{target}`).\n\
         \n\
         The bridge imports only `compose:dynlink/linker` and dispatches SQL\n\
         scalar arms as CBOR envelopes through the resident provider\n\
         `{provider_id}`. Aggregate / cast / table / pragma / column paths\n\
         return `duckerror::unsupported` at Phase A scope.\n"
    )
}

// ============================================================
// src/lib.rs generation
// ============================================================

fn lib_rs(
    provider_id: &str,
    extension_root: &str,
    catalog_extension: &str,
    version: &str,
    functions: &[&(FnKind, String)],
) -> String {
    let mut scalar_names: Vec<&str> = functions
        .iter()
        .filter(|(k, _)| *k == FnKind::Scalar)
        .map(|(_, n)| n.as_str())
        .collect();
    scalar_names.sort();
    scalar_names.dedup();

    // Build id ↔ name lookups. Handles start at 1; 0 is reserved.
    let mut scalar_name_arms = String::new();
    let mut scalar_id_arms = String::new();
    let mut scalar_register_calls = String::new();
    for (idx, name) in scalar_names.iter().enumerate() {
        let handle = (idx + 1) as u32;
        let escaped = name.replace('"', "\\\"");
        scalar_name_arms.push_str(&format!(
            "        {handle} => Some(\"{escaped}\"),\n"
        ));
        scalar_id_arms.push_str(&format!(
            "        \"{escaped}\" => Some({handle}),\n"
        ));
        scalar_register_calls.push_str(&format!(
            "        // scalar: {escaped}  (handle {handle})\n"
        ));
    }

    let extension_root = extension_root.to_string();
    let catalog_extension = catalog_extension.to_string();

    format!(
        r##"//! Auto-generated by `datalink_shim_duckdb_dynlink_emit::emit_dynlink`
//! (Phase A, opaque-blob scalar dispatch). Do NOT edit by hand — regenerate.
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

use bindings::duckdb::extension::types::{{Duckerror, Duckvalue, Invokeinfo, Resultset}};
use bindings::exports::duckdb::extension::guest::{{self as guest_export, Guest as GuestGuest, Loadresult}};
use bindings::exports::duckdb::extension::callback_dispatch::{{
    self as cb_export, Guest as CallbackGuest,
}};

use bindings::compose::dynlink::linker;

const PROVIDER_ID: &str = "{provider_id}";
const EXTENSION_ROOT: &str = "{extension_root}";
const CATALOG_EXTENSION: &str = "{catalog_extension}";
const CATALOG_VERSION: &str = "{version}";

fn resolve() -> Result<linker::Instance, Duckerror> {{
    linker::resolve_by_id(&PROVIDER_ID.to_string())
        .map_err(|e| Duckerror::Internal(format!("dynlink resolve('{{}}'): {{:?}}", PROVIDER_ID, e)))
}}

// -----------------------------------------------------------
// CBOR envelope (mirrors provider crate's Request/Response).
// -----------------------------------------------------------

#[derive(Debug, Clone)]
enum CborValue {{
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    List(Vec<CborValue>),
}}

impl serde::Serialize for CborValue {{
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {{
        match self {{
            CborValue::Null => s.serialize_unit(),
            CborValue::Bool(b) => s.serialize_bool(*b),
            CborValue::Int(i) => s.serialize_i64(*i),
            CborValue::Uint(u) => s.serialize_u64(*u),
            CborValue::Float(f) => s.serialize_f64(*f),
            CborValue::Text(t) => s.serialize_str(t),
            CborValue::Bytes(b) => s.serialize_bytes(b),
            CborValue::List(items) => {{
                use serde::ser::SerializeSeq;
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {{
                    seq.serialize_element(item)?;
                }}
                seq.end()
            }}
        }}
    }}
}}

#[derive(Debug, Clone, serde::Serialize)]
struct Request {{
    #[serde(rename = "v")]
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

#[derive(Debug, Clone)]
enum ResponseValue {{
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    List(Vec<ResponseValue>),
}}

impl<'de> serde::Deserialize<'de> for ResponseValue {{
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {{
        use serde::de::{{Error, MapAccess, SeqAccess, Visitor}};
        struct V;
        impl<'de> Visitor<'de> for V {{
            type Value = ResponseValue;
            fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {{
                f.write_str("a CBOR value")
            }}
            fn visit_unit<E: Error>(self) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Null) }}
            fn visit_none<E: Error>(self) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Null) }}
            fn visit_bool<E: Error>(self, v: bool) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Bool(v)) }}
            fn visit_i64<E: Error>(self, v: i64) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Int(v)) }}
            fn visit_u64<E: Error>(self, v: u64) -> Result<ResponseValue, E> {{
                if v <= i64::MAX as u64 {{ Ok(ResponseValue::Int(v as i64)) }}
                else {{ Ok(ResponseValue::Uint(v)) }}
            }}
            fn visit_f64<E: Error>(self, v: f64) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Float(v)) }}
            fn visit_str<E: Error>(self, v: &str) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Text(v.to_string())) }}
            fn visit_string<E: Error>(self, v: String) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Text(v)) }}
            fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Bytes(v.to_vec())) }}
            fn visit_byte_buf<E: Error>(self, v: Vec<u8>) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Bytes(v)) }}
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<ResponseValue, A::Error> {{
                let mut items = Vec::new();
                while let Some(v) = s.next_element::<ResponseValue>()? {{ items.push(v); }}
                Ok(ResponseValue::List(items))
            }}
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<ResponseValue, A::Error> {{
                let k: Option<String> = m.next_key()?;
                let k = k.ok_or_else(|| A::Error::custom("empty map"))?;
                let v = match k.as_str() {{
                    "Null" => {{ let _: serde::de::IgnoredAny = m.next_value()?; ResponseValue::Null }}
                    "Bool" => ResponseValue::Bool(m.next_value()?),
                    "Int"  => ResponseValue::Int(m.next_value()?),
                    "Uint" => ResponseValue::Uint(m.next_value()?),
                    "Float" => ResponseValue::Float(m.next_value()?),
                    "Text" => ResponseValue::Text(m.next_value()?),
                    "Bytes" => {{
                        let b: serde_bytes::ByteBuf = m.next_value()?;
                        ResponseValue::Bytes(b.into_vec())
                    }}
                    "List" => ResponseValue::List(m.next_value()?),
                    other => return Err(A::Error::custom(alloc::format!("unknown tag: {{}}", other))),
                }};
                Ok(v)
            }}
        }}
        d.deserialize_any(V)
    }}
}}

fn encode_request(args: Vec<CborValue>) -> Result<Vec<u8>, Duckerror> {{
    let mut out = Vec::new();
    ciborium::into_writer(&Request {{ version: 1, args }}, &mut out)
        .map_err(|e| Duckerror::Internal(alloc::format!("cbor encode: {{}}", e)))?;
    Ok(out)
}}

fn decode_response(bytes: &[u8]) -> Result<Response, Duckerror> {{
    ciborium::from_reader(bytes)
        .map_err(|e| Duckerror::Internal(alloc::format!("cbor decode: {{}}", e)))
}}

fn call(method: &str, args: Vec<CborValue>) -> Result<ResponseValue, Duckerror> {{
    let inst = resolve()?;
    let payload = encode_request(args)?;
    let bytes = inst
        .invoke(&method.to_string(), &payload)
        .map_err(|e| Duckerror::Internal(alloc::format!("{{}}: invoke: {{:?}}", method, e)))?;
    let resp = decode_response(&bytes)?;
    if let Some(err) = resp.err {{
        return Err(Duckerror::Internal(alloc::format!("{{}}: {{}}", method, err)));
    }}
    Ok(resp.ok.unwrap_or(ResponseValue::Null))
}}

// -----------------------------------------------------------
// duckvalue / ResponseValue marshalling.
// -----------------------------------------------------------

fn duckv_to_cbor(v: &Duckvalue) -> CborValue {{
    match v {{
        Duckvalue::Null => CborValue::Null,
        Duckvalue::Boolean(b) => CborValue::Bool(*b),
        Duckvalue::Tinyint(i) => CborValue::Int(*i as i64),
        Duckvalue::Smallint(i) => CborValue::Int(*i as i64),
        Duckvalue::Integer(i) => CborValue::Int(*i as i64),
        Duckvalue::Bigint(i) => CborValue::Int(*i),
        Duckvalue::Utinyint(u) => CborValue::Int(*u as i64),
        Duckvalue::Usmallint(u) => CborValue::Int(*u as i64),
        Duckvalue::Uinteger(u) => CborValue::Int(*u as i64),
        Duckvalue::Ubigint(u) => CborValue::Uint(*u),
        Duckvalue::Float(f) => CborValue::Float(*f as f64),
        Duckvalue::Double(f) => CborValue::Float(*f),
        Duckvalue::Varchar(s) => CborValue::Text(s.clone()),
        Duckvalue::Blob(b) => CborValue::Bytes(b.clone()),
        _ => CborValue::Null,
    }}
}}

fn response_to_duckv(v: ResponseValue) -> Duckvalue {{
    match v {{
        ResponseValue::Null => Duckvalue::Null,
        ResponseValue::Bool(b) => Duckvalue::Boolean(b),
        ResponseValue::Int(i) => Duckvalue::Bigint(i),
        ResponseValue::Uint(u) => Duckvalue::Ubigint(u),
        ResponseValue::Float(f) => Duckvalue::Double(f),
        ResponseValue::Text(t) => Duckvalue::Varchar(t),
        ResponseValue::Bytes(b) => Duckvalue::Blob(b),
        ResponseValue::List(_) => Duckvalue::Null,
    }}
}}

fn scalar_name_by_handle(h: u32) -> Option<&'static str> {{
    match h {{
{scalar_name_arms}        _ => None,
    }}
}}

fn scalar_handle_by_name(name: &str) -> Option<u32> {{
    match name {{
{scalar_id_arms}        _ => None,
    }}
}}

// -----------------------------------------------------------
// Guest impls.
// -----------------------------------------------------------

struct Component;

impl GuestGuest for Component {{
    fn load() -> Result<Loadresult, Duckerror> {{
        // Registration is host-orchestrated in the dynlink flow —
        // the composed provider owns the SQL surface, the bridge
        // only ferries calls. Returning Loadresult empty keeps the
        // host's LOAD path happy.
{scalar_register_calls}        Ok(Loadresult {{
            name: EXTENSION_ROOT.to_string(),
            version: CATALOG_VERSION.to_string(),
            extras: Vec::new(),
        }})
    }}
    fn reconfigure(_keys: Vec<String>) -> Result<bool, Duckerror> {{ Ok(false) }}
    fn shutdown() -> Result<bool, Duckerror> {{ Ok(true) }}
}}

impl CallbackGuest for Component {{
    fn call_scalar(
        handle: u32,
        args: Vec<Duckvalue>,
        _ctx: Invokeinfo,
    ) -> Result<Duckvalue, Duckerror> {{
        let name = scalar_name_by_handle(handle)
            .ok_or_else(|| Duckerror::Internal(alloc::format!("unknown scalar handle {{}}", handle)))?;
        // Null-propagation: any NULL argument short-circuits to NULL.
        if args.iter().any(|v| matches!(v, Duckvalue::Null)) {{
            return Ok(Duckvalue::Null);
        }}
        let method = name.replace('_', "-");
        let cbor_args: Vec<CborValue> = args.iter().map(duckv_to_cbor).collect();
        let resp = call(&method, cbor_args)?;
        Ok(response_to_duckv(resp))
    }}

    fn call_scalar_batch_col(
        _handle: u32,
        _args: Vec<bindings::duckdb::extension::column_types::Colvec>,
        _ctx: Invokeinfo,
    ) -> Result<bindings::duckdb::extension::column_types::Colvec, Duckerror> {{
        Err(Duckerror::Internal(
            "call_scalar_batch_col: not wired at Phase A (row-major cold path only)".to_string(),
        ))
    }}

    fn call_aggregate_col(
        _handle: u32,
        _args: Vec<bindings::duckdb::extension::column_types::Colvec>,
    ) -> Result<Duckvalue, Duckerror> {{
        Err(Duckerror::Internal("call_aggregate_col: unsupported (Phase A)".to_string()))
    }}

    fn call_cast_col(
        _handle: u32,
        _arg: bindings::duckdb::extension::column_types::Colvec,
    ) -> Result<bindings::duckdb::extension::column_types::Colvec, Duckerror> {{
        Err(Duckerror::Internal("call_cast_col: unsupported (Phase A)".to_string()))
    }}

    fn call_table(_handle: u32, _args: Vec<Duckvalue>) -> Result<Resultset, Duckerror> {{
        Err(Duckerror::Internal("call_table: unsupported (Phase A)".to_string()))
    }}

    fn call_pragma(_handle: u32, _args: Vec<Duckvalue>) -> Result<Option<Duckvalue>, Duckerror> {{
        Err(Duckerror::Internal("call_pragma: unsupported (Phase A)".to_string()))
    }}

    fn call_cast(_handle: u32, _value: Duckvalue) -> Result<Duckvalue, Duckerror> {{
        Err(Duckerror::Internal("call_cast: unsupported (Phase A)".to_string()))
    }}
}}

bindings::export!(Component with_types_in bindings);
"##,
    )
}
