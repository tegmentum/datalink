//! Dynlink-mode sqlite bridge emitter (Phase A, §A.4 Option 1).
//!
//! Emits a bridge crate that dispatches every SQL arm through
//! `compose:dynlink/linker` — CBOR envelope in / CBOR envelope out
//! against a resident provider identified by `opts.provider_id` —
//! instead of the wac-plug-linked WIT interfaces the sibling
//! `datalink-shim-sqlite-emit` produces.
//!
//! Following §A.4 Option 1, every scalar arm is emitted with an
//! opaque BLOB payload discipline: each argument is marshalled as
//! a byte string sourced from the SQL value's blob (or the
//! UTF-8 bytes of a text value), and the return is unwrapped as
//! bytes rewrapped as `sql-value { blob }`. The provider owns the
//! per-arm type inference; the bridge is a pure CBOR tunnel.
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
//!
//! Aggregate / table / window / hook exports are stubbed at
//! Phase A: they surface an "unsupported" error variant on
//! invocation but keep the world surface structurally complete
//! so the host's bindgen-generated composite world instantiates
//! without a missing-export failure.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::spatial_catalog::{Catalog, FnKind, LeavesOverlay};
use crate::DynlinkOptions;

/// Emit a Dynlink-mode sqlite bridge crate under `out_dir`.
///
/// Produced layout:
///
/// ```text
/// Cargo.toml
/// README.md
/// src/lib.rs
/// wit/world.wit
/// wit/deps/compose-dynlink/     (copied from datalink-dynlink)
/// wit/deps/sys-compose/         (copied from datalink-dynlink)
/// wit/deps/sqlite-extension/    (copied from ~/git/sqlink/wit/…)
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

    // Cargo.toml
    fs::write(out_dir.join("Cargo.toml"), cargo_toml(&crate_name, &version))?;

    // wit/world.wit — imports compose:dynlink/linker + exports the
    // sqlite:extension contract surface.
    fs::write(out_dir.join("wit/world.wit"), world_wit(&opts.sub_ext))?;

    // wit/deps — copy compose-dynlink + sys-compose + sqlite-extension.
    populate_deps(&out_dir.join("wit/deps"))?;

    // src/lib.rs
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
    format!("{sub}-sqlite-bridge-dynlink")
}

fn cargo_toml(crate_name: &str, version: &str) -> String {
    format!(
        r#"[package]
name = "{crate_name}"
version = "{version}"
edition = "2021"
description = "Phase A dynlink-mode sqlite bridge — routes SQL dispatch through compose:dynlink/linker against a resident provider."
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
opt-level = "s"
lto = true
codegen-units = 1
strip = true
panic = "abort"
"#,
    )
}

fn world_wit(sub_ext: &str) -> String {
    let pkg = sub_ext
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    format!(
        r#"package sqlite-bridge:{pkg}@0.1.0;

/// Phase A dynlink-mode sqlite bridge.
///
/// The bridge imports `compose:dynlink/linker` for outbound
/// dispatch to a resident provider and exports the canonical
/// sqlite:extension contract surface. Scalar registration + the
/// on-scalar-function callback are the only paths wired to
/// dispatch; aggregate/collation/hook exports keep structural
/// parity with the sibling wac-plug bridge but return an
/// `extension-error` on invocation.
world bridge {{
    import compose:dynlink/linker@0.1.0;

    export sqlite:extension/extension;
    export sqlite:extension/extension-callbacks;
}}
"#,
    )
}

/// Copy the vendored WIT trees the emitted bridge needs into
/// `deps_dir`. Sources:
///
///   * `compose:dynlink` + `sys:compose` — from `datalink-dynlink`'s
///     WIT tree (the definitive copy for this repo).
///   * `sqlite:extension` — from `~/git/sqlink/wit/` (contract).
///
/// Every copied file passes through `kebab_fix_wit` so identifiers
/// like `-2d`/`-3d`/`-4d` become `-twod`/`-threed`/`-fourd` — a
/// wit-bindgen invariant.
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
    // Only copy the package + linker + endpoint files. The
    // upstream world.wit references sqlite:extension worlds that
    // aren't part of this bridge — the emit crate synthesises its
    // own world.wit at wit/world.wit.
    for name in ["package.wit", "linker.wit", "endpoint.wit"] {
        let f = compose_dynlink_from.join(name);
        if f.is_file() {
            copy_kebab_fixed(&f, &compose_dst.join(name))?;
        }
    }
    copy_tree_kebab_fixed(&sys_compose_from, &deps_dir.join("sys-compose"))?;

    // sqlite:extension contract package.
    let sqlite_from = std::env::var("SQLINK_WIT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("git/sqlink/wit")
        });
    // The sqlink repo keeps sqlite-extension.wit at the top level.
    // Copy it into deps/sqlite-extension/extension.wit — the
    // subdirectory name matches the `sqlite:extension` package the
    // world imports from.
    let sqlite_dst = deps_dir.join("sqlite-extension");
    fs::create_dir_all(&sqlite_dst)?;
    let src_extension = sqlite_from.join("sqlite-extension.wit");
    if !src_extension.is_file() {
        return Err(anyhow!(
            "sqlite:extension WIT source missing: {} (set SQLINK_WIT)",
            src_extension.display()
        ));
    }
    copy_kebab_fixed(&src_extension, &sqlite_dst.join("extension.wit"))?;
    // Prepend a package declaration if the source didn't carry
    // one — the sqlink top-level sqlite-extension.wit relies on
    // being included into a world file's package scope, but
    // wit-bindgen's dep-package resolver requires the dependency
    // package to declare its own name.
    fixup_sqlite_package_decl(&sqlite_dst.join("extension.wit"))?;
    Ok(())
}

/// Ensure the copied sqlite-extension.wit declares
/// `package sqlite:extension;` at the top. The upstream file at
/// `~/git/sqlink/wit/sqlite-extension.wit` has no package header
/// because it's spliced into world files that declare the
/// `sqlink:wasm` package; when the file is copied under
/// `wit/deps/sqlite-extension/` here, wit-bindgen resolves the
/// `sqlite:extension/…` import path against this file and needs
/// a matching package declaration to pick it up.
fn fixup_sqlite_package_decl(path: &Path) -> Result<()> {
    let text = fs::read_to_string(path)?;
    if text.trim_start().starts_with("package ") {
        return Ok(());
    }
    let mut out = String::with_capacity(text.len() + 40);
    out.push_str("package sqlite:extension;\n\n");
    out.push_str(&text);
    fs::write(path, out)?;
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
         Phase A dynlink-mode sqlite bridge for `{sub_ext}` (target `{target}`).\n\
         \n\
         The bridge imports only `compose:dynlink/linker` and dispatches SQL\n\
         arms as CBOR envelopes through the resident provider `{provider_id}`.\n\
         Scalar registration is wired end-to-end; aggregates / collations /\n\
         hooks return an `extension-error` variant at Phase A scope.\n"
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

    // Build the scalar-name → function-id assignment table. Ids
    // start at 1 (0 is reserved as a sentinel for
    // "not-yet-registered"). Every scalar advertised by the catalog
    // gets an id; the host's `on-scalar-function` callback carries
    // the id back so the bridge can look up the method to invoke.
    let mut scalar_id_arms = String::new();
    let mut scalar_name_arms = String::new();
    let mut scalar_register_calls = String::new();
    for (idx, name) in scalar_names.iter().enumerate() {
        let id = (idx + 1) as u64;
        let escaped = name.replace('"', "\\\"");
        scalar_id_arms.push_str(&format!(
            "        \"{escaped}\" => Some({id}),\n"
        ));
        scalar_name_arms.push_str(&format!(
            "        {id} => Some(\"{escaped}\"),\n"
        ));
        scalar_register_calls.push_str(&format!(
            "        register_scalar(db, \"{escaped}\", -1, {id})?;\n"
        ));
    }

    let extension_root = extension_root.to_string();
    let catalog_extension = catalog_extension.to_string();

    format!(
        r##"//! Auto-generated by `datalink_shim_sqlite_dynlink_emit::emit_dynlink`
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

use bindings::exports::sqlite::extension::extension::{{
    self as ext_export, Guest as ExtensionGuest, ExtensionError, SqlValue, ValueType,
    FunctionFlags,
}};
use bindings::exports::sqlite::extension::extension_callbacks::{{
    self as cb_export, Guest as CallbacksGuest, AuthAction, AuthResult, UpdateType,
}};

use bindings::compose::dynlink::linker;

const PROVIDER_ID: &str = "{provider_id}";
const EXTENSION_ROOT: &str = "{extension_root}";
const CATALOG_EXTENSION: &str = "{catalog_extension}";
const CATALOG_VERSION: &str = "{version}";

fn resolve() -> Result<linker::Instance, String> {{
    linker::resolve_by_id(&PROVIDER_ID.to_string())
        .map_err(|e| format!("dynlink resolve('{{}}'): {{:?}}", PROVIDER_ID, e))
}}

// -----------------------------------------------------------
// CBOR envelope (mirrors provider crate's Request/Response).
// -----------------------------------------------------------

#[derive(Debug, Clone)]
enum CborValue {{
    Null,
    Bool(bool),
    Int(i64),
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
                f.write_str("a CBOR value (null, bool, int, float, text, bytes, list)")
            }}
            fn visit_unit<E: Error>(self) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Null) }}
            fn visit_none<E: Error>(self) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Null) }}
            fn visit_bool<E: Error>(self, v: bool) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Bool(v)) }}
            fn visit_i64<E: Error>(self, v: i64) -> Result<ResponseValue, E> {{ Ok(ResponseValue::Int(v)) }}
            fn visit_u64<E: Error>(self, v: u64) -> Result<ResponseValue, E> {{
                if v <= i64::MAX as u64 {{ Ok(ResponseValue::Int(v as i64)) }}
                else {{ Err(E::custom("u64 overflow to i64")) }}
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

fn encode_request(args: Vec<CborValue>) -> Result<Vec<u8>, String> {{
    let mut out = Vec::new();
    ciborium::into_writer(&Request {{ version: 1, args }}, &mut out)
        .map_err(|e| alloc::format!("cbor encode: {{}}", e))?;
    Ok(out)
}}

fn decode_response(bytes: &[u8]) -> Result<Response, String> {{
    ciborium::from_reader(bytes).map_err(|e| alloc::format!("cbor decode: {{}}", e))
}}

fn call(method: &str, args: Vec<CborValue>) -> Result<ResponseValue, String> {{
    let inst = resolve()?;
    let payload = encode_request(args)?;
    let bytes = inst
        .invoke(&method.to_string(), &payload)
        .map_err(|e| alloc::format!("{{}}: invoke: {{:?}}", method, e))?;
    let resp = decode_response(&bytes)?;
    if let Some(err) = resp.err {{
        return Err(alloc::format!("{{}}: {{}}", method, err));
    }}
    Ok(resp.ok.unwrap_or(ResponseValue::Null))
}}

// -----------------------------------------------------------
// SQL value marshalling helpers — opaque-blob discipline.
// -----------------------------------------------------------

fn sqlv_to_cbor(v: &SqlValue) -> CborValue {{
    match v.value_type {{
        ValueType::Null => CborValue::Null,
        ValueType::Integer => v.int_value.map(CborValue::Int).unwrap_or(CborValue::Null),
        ValueType::Float => v.float_value.map(CborValue::Float).unwrap_or(CborValue::Null),
        ValueType::Text => v.text_value.clone().map(CborValue::Text).unwrap_or(CborValue::Null),
        ValueType::Blob => v.blob_value.clone().map(CborValue::Bytes).unwrap_or(CborValue::Null),
    }}
}}

fn response_to_sqlv(v: ResponseValue) -> SqlValue {{
    match v {{
        ResponseValue::Null => SqlValue {{
            value_type: ValueType::Null,
            int_value: None,
            float_value: None,
            text_value: None,
            blob_value: None,
        }},
        ResponseValue::Bool(b) => SqlValue {{
            value_type: ValueType::Integer,
            int_value: Some(if b {{ 1 }} else {{ 0 }}),
            float_value: None,
            text_value: None,
            blob_value: None,
        }},
        ResponseValue::Int(i) => SqlValue {{
            value_type: ValueType::Integer,
            int_value: Some(i),
            float_value: None,
            text_value: None,
            blob_value: None,
        }},
        ResponseValue::Float(f) => SqlValue {{
            value_type: ValueType::Float,
            int_value: None,
            float_value: Some(f),
            text_value: None,
            blob_value: None,
        }},
        ResponseValue::Text(t) => SqlValue {{
            value_type: ValueType::Text,
            int_value: None,
            float_value: None,
            text_value: Some(t),
            blob_value: None,
        }},
        ResponseValue::Bytes(b) => SqlValue {{
            value_type: ValueType::Blob,
            int_value: None,
            float_value: None,
            text_value: None,
            blob_value: Some(b),
        }},
        ResponseValue::List(_) => SqlValue {{
            value_type: ValueType::Null,
            int_value: None,
            float_value: None,
            text_value: None,
            blob_value: None,
        }},
    }}
}}

fn scalar_name_by_id(id: u64) -> Option<&'static str> {{
    match id {{
{scalar_name_arms}        _ => None,
    }}
}}

fn scalar_id_by_name(name: &str) -> Option<u64> {{
    match name {{
{scalar_id_arms}        _ => None,
    }}
}}

fn register_scalar(db: u64, name: &str, num_args: i32, function_id: u64)
    -> Result<u64, ExtensionError>
{{
    ext_export::register_scalar_function(
        db,
        &name.to_string(),
        num_args,
        FunctionFlags::DETERMINISTIC,
        function_id,
    )
}}

// -----------------------------------------------------------
// Guest impls.
// -----------------------------------------------------------

struct Component;

impl ExtensionGuest for Component {{
    fn register_scalar_function(
        _db: u64,
        _name: String,
        _num_args: i32,
        _func_flags: FunctionFlags,
        _function_id: u64,
    ) -> Result<u64, ExtensionError> {{
        // A dynlink bridge does not itself host the `extension`
        // interface — every method returns an error because
        // registration is orchestrated by the host during
        // `sqlite3_extension_init`. Kept as a stub so the world
        // exports a matching Guest impl.
        Err(ExtensionError {{
            code: 1,
            message: "sqlite dynlink bridge: extension registration is host-orchestrated"
                .to_string(),
        }})
    }}
    fn unregister_function(_handle: u64) -> Result<(), ExtensionError> {{ Ok(()) }}
    fn register_aggregate_function(
        _db: u64, _name: String, _num_args: i32,
        _func_flags: FunctionFlags, _function_id: u64,
    ) -> Result<u64, ExtensionError> {{
        Err(ExtensionError {{ code: 1, message: "aggregate: not supported (Phase A)".to_string() }})
    }}
    fn register_collation(_db: u64, _name: String, _collation_id: u64) -> Result<u64, ExtensionError> {{
        Err(ExtensionError {{ code: 1, message: "collation: not supported (Phase A)".to_string() }})
    }}
    fn unregister_collation(_handle: u64) -> Result<(), ExtensionError> {{ Ok(()) }}
    fn set_update_hook(_db: u64, _hook_id: u64) -> Result<u64, ExtensionError> {{
        Err(ExtensionError {{ code: 1, message: "update-hook: not supported (Phase A)".to_string() }})
    }}
    fn remove_update_hook(_handle: u64) -> Result<(), ExtensionError> {{ Ok(()) }}
    fn set_commit_hook(_db: u64, _hook_id: u64) -> Result<u64, ExtensionError> {{
        Err(ExtensionError {{ code: 1, message: "commit-hook: not supported (Phase A)".to_string() }})
    }}
    fn remove_commit_hook(_handle: u64) -> Result<(), ExtensionError> {{ Ok(()) }}
    fn set_rollback_hook(_db: u64, _hook_id: u64) -> Result<u64, ExtensionError> {{
        Err(ExtensionError {{ code: 1, message: "rollback-hook: not supported (Phase A)".to_string() }})
    }}
    fn remove_rollback_hook(_handle: u64) -> Result<(), ExtensionError> {{ Ok(()) }}
    fn set_busy_timeout(_db: u64, _ms: i32) -> Result<(), ExtensionError> {{ Ok(()) }}
    fn set_authorizer(_db: u64, _auth_id: u64) -> Result<u64, ExtensionError> {{
        Err(ExtensionError {{ code: 1, message: "authorizer: not supported (Phase A)".to_string() }})
    }}
    fn remove_authorizer(_handle: u64) -> Result<(), ExtensionError> {{ Ok(()) }}
}}

impl CallbacksGuest for Component {{
    fn on_scalar_function(function_id: u64, args: Vec<SqlValue>) -> Result<SqlValue, String> {{
        let name = scalar_name_by_id(function_id)
            .ok_or_else(|| alloc::format!("unknown function id {{}}", function_id))?;
        // If any argument is NULL, propagate NULL — matches the
        // datafission-emit sibling's default null-propagation
        // discipline. Providers that need to observe explicit NULL
        // arguments will need a follow-up Phase to opt in per-arm.
        if args.iter().any(|v| matches!(v.value_type, ValueType::Null)) {{
            return Ok(response_to_sqlv(ResponseValue::Null));
        }}
        // WIT SQL name → provider method: snake_case → kebab-case.
        let method = name.replace('_', "-");
        let cbor_args: Vec<CborValue> = args.iter().map(sqlv_to_cbor).collect();
        let resp = call(&method, cbor_args)?;
        Ok(response_to_sqlv(resp))
    }}

    fn on_aggregate_step(_function_id: u64, _context_id: u64, _args: Vec<SqlValue>) {{}}
    fn on_aggregate_finalize(_function_id: u64, _context_id: u64)
        -> Result<SqlValue, String> {{
        Err("aggregate: not supported (Phase A)".to_string())
    }}
    fn on_collation_compare(_collation_id: u64, _a: String, _b: String) -> i32 {{ 0 }}
    fn on_update(_hook_id: u64, _op: UpdateType, _database: String, _table: String, _rowid: i64) {{}}
    fn on_commit(_hook_id: u64) -> bool {{ false }}
    fn on_rollback(_hook_id: u64) {{}}
    fn on_authorize(
        _auth_id: u64, _action: AuthAction, _arg1: Option<String>,
        _arg2: Option<String>, _database: Option<String>, _trigger: Option<String>,
    ) -> AuthResult {{ AuthResult::Ok }}
}}

/// Convenience helper for a future `sqlite3_extension_init`
/// bootstrap: registers every scalar the catalog carries. Not
/// invoked by the emitted world (the host wires registration on
/// LOAD), but kept for symmetry with the wac-plug bridge's
/// helper.
fn register_all_scalars(db: u64) -> Result<(), ExtensionError> {{
{scalar_register_calls}    Ok(())
}}

bindings::export!(Component with_types_in bindings);
"##,
    )
}
