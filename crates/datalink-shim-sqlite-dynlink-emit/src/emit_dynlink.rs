//! Dynlink-mode sqlite bridge emitter (Phase A, §A.4 Option 1).
//!
//! Emits a bridge crate that dispatches every SQL scalar through
//! `compose:dynlink/linker` — CBOR envelope in / CBOR envelope out
//! against a resident provider identified by `opts.provider_id` —
//! instead of the wac-plug-linked WIT interfaces the sibling
//! `datalink-shim-sqlite-emit` produces.
//!
//! The bridge maps onto the declarative `sqlite:extension@1.0.0`
//! contract (fresh recon, `/Users/zacharywhitley/git/sqlink/sqlite-wit/
//! wit/sqlite-extension/*.wit`):
//!
//!   * `metadata.describe() -> manifest` — the guest advertises
//!     every scalar it wants registered; the host installs the
//!     sqlite3 trampolines against its own connection.
//!   * `scalar-function.call(func-id, args) -> result<sql-value,
//!     string>` — per-row dispatch keyed by the manifest-assigned
//!     `func-id`.
//!
//! There is **no** imperative `register-*` call on the extension
//! side (the pre-1.0.0 contract had an `extension` interface with
//! `register-scalar-function`; that has been retired). This crate's
//! previous emit forked against the stale contract; the rewrite
//! matches the shape shipping in `postgis-sqlink-bridge`.
//!
//! Wire discipline mirrors
//! `postgis-wasm/crates/provider/src/envelope.rs`:
//!
//! ```ignore
//! Request  { v: 1, args: Vec<CborValue> }
//! Response { ok:  Option<CborValue>, err: Option<String> }
//! ```
//!
//! Aggregate / vtab / collation / hook exports are OMITTED at
//! Phase A: the `minimal` world exports only `metadata` +
//! `scalar-function`. Follow-up phases can add
//! `aggregate-function` / `vtab` / hook exports as their catalog
//! metadata lands.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::sql_extension_catalog::{Catalog, FnKind, LeavesOverlay};
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
/// wit/deps/sqlite-extension/    (copied from ~/git/sqlink/sqlite-wit/wit/…)
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

    // Build the scalar alias→canonical map from `catalog.aliases`.
    // The catalog carries name-mangling aliases (e.g.
    // `st_geomfromtext` → `st_geom_from_text`) as first-class
    // `[[aliases]]` entries so both SQL spellings resolve to the
    // same provider WIT method. The bridge exposes both forms via
    // `metadata.describe()` (they're both in `leaf.scalars`), but
    // the provider only matches the WIT-canonical (long) form.
    // Without translation, a call to `st_geomfromtext(...)` reaches
    // the provider as method `st-geomfromtext` and fails with
    // `unknown method`.
    let scalar_aliases: Vec<(String, String)> = catalog
        .aliases
        .iter()
        .filter(|a| a.kind == "scalar")
        .map(|a| (a.alias.clone(), a.canonical.clone()))
        .collect();

    let lib_src = lib_rs(
        &opts.provider_id,
        &opts.extension_root,
        &catalog.meta.extension,
        &version,
        functions.iter().collect::<Vec<_>>().as_slice(),
        &scalar_aliases,
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
wit-bindgen = {{ version = "0.41", features = ["macros"] }}
wit-bindgen-rt = {{ version = "0.41", features = ["bitflags"] }}
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
"#,
    )
}

/// Convert a snake_case sub-ext name into a kebab-case WIT package
/// segment that satisfies the component-model rule "each `-`-separated
/// segment must start with `[a-z]`". Underscores become dashes; digit-
/// starting segments (`3d`, `2d`, `4d`) get the same word-form
/// treatment as `scripts/fix-postgis-kebab.sh` (`3d` → `threed`, etc.)
/// so a `postgis_3d` sub-ext produces `postgis-threed`, not
/// `postgis-3d` (which wit-bindgen rejects).
fn kebab_safe_pkg_name(sub_ext: &str) -> String {
    let raw: String = sub_ext
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    raw.split('-')
        .map(|seg| match seg {
            "2" => "two".to_string(),
            "3" => "three".to_string(),
            "4" => "four".to_string(),
            "2d" => "twod".to_string(),
            "3d" => "threed".to_string(),
            "4d" => "fourd".to_string(),
            _ => seg.to_string(),
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn world_wit(sub_ext: &str) -> String {
    let pkg = kebab_safe_pkg_name(sub_ext);
    format!(
        r#"package sqlite-bridge:{pkg}@0.1.0;

/// Phase A dynlink-mode sqlite bridge.
///
/// The bridge imports `compose:dynlink/linker` for outbound
/// dispatch to a resident provider and exports the declarative
/// `sqlite:extension@1.0.0` metadata + scalar-function pair.
/// The host reads `metadata.describe()` at load, installs
/// sqlite3 trampolines against every advertised scalar, and
/// routes per-row calls back through `scalar-function.call`.
world bridge {{
    import compose:dynlink/linker@0.1.0;

    // sqlite:extension imports needed by the exports' types.
    import sqlite:extension/types@1.0.0;
    import sqlite:extension/policy@1.0.0;

    export sqlite:extension/metadata@1.0.0;
    export sqlite:extension/scalar-function@1.0.0;
}}
"#,
    )
}

/// Copy the vendored WIT trees the emitted bridge needs into
/// `deps_dir`. Sources:
///
///   * `compose:dynlink` + `sys:compose` — from `datalink-dynlink`'s
///     WIT tree (the definitive copy for this repo).
///   * `sqlite:extension` — from `~/git/sqlink/sqlite-wit/wit/…`
///     (the fresh @1.0.0 contract).
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

    // sqlite:extension contract package. `SQLINK_WIT` should point
    // at the sqlite-wit tree (defaults to
    // `~/git/sqlink/sqlite-wit/wit/sqlite-extension/`); we copy the
    // whole tree since `policy` uses `http.method` from `host-spi`
    // and `metadata` uses `types + policy`, so trimming is fragile.
    // The `worlds/` subdirectory (if any) is skipped — the bridge
    // synthesises its own world at `wit/world.wit`.
    let sqlite_from = std::env::var("SQLINK_WIT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("git/sqlink/sqlite-wit/wit/sqlite-extension")
        });
    if !sqlite_from.is_dir() {
        return Err(anyhow!(
            "sqlite:extension WIT source missing: {} (set SQLINK_WIT)",
            sqlite_from.display()
        ));
    }
    let sqlite_dst = deps_dir.join("sqlite-extension");
    fs::create_dir_all(&sqlite_dst)?;
    for entry in fs::read_dir(&sqlite_from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src = entry.path();
        if ty.is_file() {
            // Skip the upstream world file — the bridge world
            // lives at wit/world.wit and is synthesised above.
            if src.file_name().and_then(|s| s.to_str()) == Some("world.wit") {
                continue;
            }
            let dst = sqlite_dst.join(entry.file_name());
            copy_kebab_fixed(&src, &dst)?;
        }
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
         Phase A dynlink-mode sqlite bridge for `{sub_ext}` (target `{target}`).\n\
         \n\
         Exports the declarative `sqlite:extension@1.0.0` metadata + scalar-\n\
         function contract. `metadata.describe()` advertises every scalar the\n\
         catalog names; `scalar-function.call(func-id, args)` routes per-row\n\
         invocations through `compose:dynlink/linker` against the resident\n\
         provider `{provider_id}`. Aggregate / vtab / hook exports are\n\
         deferred to a follow-up.\n"
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
    scalar_aliases: &[(String, String)],
) -> String {
    let mut scalar_names: Vec<&str> = functions
        .iter()
        .filter(|(k, _)| *k == FnKind::Scalar)
        .map(|(_, n)| n.as_str())
        .collect();
    scalar_names.sort();
    scalar_names.dedup();

    // Emit the compact→canonical alias table as a `match` arm body
    // consumed by the `canonical_for` helper below. Only aliases
    // that appear as scalars in this bridge's dispatch set contribute
    // an arm — if the alias isn't a name we register with sqlite,
    // there's no dispatch site that could reach it. Aliases whose
    // canonical form is missing from the dispatch set are dropped
    // too (no arm to route to).
    let scalar_name_set: std::collections::BTreeSet<&str> =
        scalar_names.iter().copied().collect();
    let mut alias_arms = String::new();
    for (alias, canonical) in scalar_aliases {
        if scalar_name_set.contains(alias.as_str())
            && scalar_name_set.contains(canonical.as_str())
        {
            let a = alias.replace('"', "\\\"");
            let c = canonical.replace('"', "\\\"");
            alias_arms.push_str(&format!("            \"{a}\" => \"{c}\",\n"));
        }
    }

    // Build the func-id ↔ name lookup and the ScalarFunctionSpec
    // list body for `metadata.describe()`. Ids start at 1 (id 0 is
    // reserved as a manifest sentinel).
    let mut scalar_id_arms = String::new();
    let mut scalar_specs = String::new();
    for (idx, name) in scalar_names.iter().enumerate() {
        let id = (idx + 1) as u64;
        let escaped = name.replace('"', "\\\"");
        scalar_id_arms.push_str(&format!(
            "        {id} => Some(\"{escaped}\"),\n"
        ));
        // Phase A dynlink advertises every scalar with num_args=-1
        // (variadic). The catalog carries no arity info; declaring
        // -1 lets sqlite route calls of any arity through
        // `scalar-function.call`, where the provider can inspect
        // args.len() and reject if needed. TODO(phase-B): thread
        // arity from `datalink-shim-codegen-core::interface_db`
        // once the catalog carries the shape.
        scalar_specs.push_str(&format!(
            r#"            ScalarFunctionSpec {{
                id: {id},
                name: "{escaped}".to_string(),
                num_args: -1,
                func_flags: FunctionFlags::DETERMINISTIC,
            }},
"#,
        ));
    }

    let extension_root = extension_root.to_string();
    let catalog_extension = catalog_extension.to_string();

    format!(
        r##"//! Auto-generated by `datalink_shim_sqlite_dynlink_emit::emit_dynlink`
//! (Phase A, opaque-blob scalar dispatch). Do NOT edit by hand — regenerate.
#![allow(unused_imports, dead_code)]
#![allow(unsafe_op_in_unsafe_fn)]

mod bindings {{
    wit_bindgen::generate!({{
        path: "wit",
        world: "bridge",
        generate_all,
    }});
}}

use bindings::exports::sqlite::extension::metadata::{{
    Guest as MetadataGuest, Manifest, ScalarFunctionSpec,
}};
use bindings::exports::sqlite::extension::scalar_function::Guest as ScalarFunctionGuest;
use bindings::sqlite::extension::types::{{FunctionFlags, SqlValue}};

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
                    other => return Err(A::Error::custom(format!("unknown tag: {{}}", other))),
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
        .map_err(|e| format!("cbor encode: {{}}", e))?;
    Ok(out)
}}

fn decode_response(bytes: &[u8]) -> Result<Response, String> {{
    ciborium::from_reader(bytes).map_err(|e| format!("cbor decode: {{}}", e))
}}

fn call(method: &str, args: Vec<CborValue>) -> Result<ResponseValue, String> {{
    let inst = resolve()?;
    let payload = encode_request(args)?;
    let bytes = inst
        .invoke(&method.to_string(), &payload)
        .map_err(|e| format!("{{}}: invoke: {{:?}}", method, e))?;
    let resp = decode_response(&bytes)?;
    if let Some(err) = resp.err {{
        return Err(format!("{{}}: {{}}", method, err));
    }}
    Ok(resp.ok.unwrap_or(ResponseValue::Null))
}}

// -----------------------------------------------------------
// SqlValue marshalling — variant discipline per the @1.0.0
// contract. The `wit-value` arm is Phase-A-out-of-scope; the
// bridge treats it as null in both directions.
// -----------------------------------------------------------

fn sqlv_to_cbor(v: &SqlValue) -> Result<CborValue, String> {{
    Ok(match v {{
        SqlValue::Null => CborValue::Null,
        SqlValue::Integer(i) => CborValue::Int(*i),
        SqlValue::Real(f) => CborValue::Float(*f),
        SqlValue::Text(t) => CborValue::Text(t.clone()),
        SqlValue::Blob(b) => CborValue::Bytes(b.clone()),
        // WitValue is Phase-A-out-of-scope. Prior code silently
        // downgraded it to CBOR null, which lied to the provider
        // (an explicit-typed argument became indistinguishable
        // from a real NULL). Surface an explicit error instead so
        // callers see a diagnostic on the SQL side.
        SqlValue::WitValue(_) => {{
            return Err("bridge: WitValue arg not supported in dynlink Phase A".to_string());
        }}
    }})
}}

fn response_to_sqlv(v: ResponseValue) -> Result<SqlValue, String> {{
    Ok(match v {{
        ResponseValue::Null => SqlValue::Null,
        ResponseValue::Bool(b) => SqlValue::Integer(if b {{ 1 }} else {{ 0 }}),
        ResponseValue::Int(i) => SqlValue::Integer(i),
        ResponseValue::Float(f) => SqlValue::Real(f),
        ResponseValue::Text(t) => SqlValue::Text(t),
        ResponseValue::Bytes(b) => SqlValue::Blob(b),
        // A list-shaped response has no SQLite scalar sqlvalue
        // arm. Prior code silently returned SqlValue::Null, hiding
        // a shape mismatch behind an implicit NULL. Return an
        // explicit error so the SQL side sees a diagnostic.
        ResponseValue::List(_) => {{
            return Err("bridge: list-shaped response not supported".to_string());
        }}
    }})
}}

fn scalar_name_by_id(id: u64) -> Option<&'static str> {{
    match id {{
{scalar_id_arms}        _ => None,
    }}
}}

/// SQL-name aliasing: translate a compact-form SQL name (e.g.
/// `st_geomfromtext`) to its WIT-canonical (long) form
/// (`st_geom_from_text`) before kebab-casing to the provider
/// method name. The catalog carries both spellings as scalars in
/// `leaf.scalars` (both are advertised via `metadata.describe()`
/// so callers can use either), and as first-class `[[aliases]]`
/// entries. The provider dispatch only matches the WIT-canonical
/// form; the bridge's per-row `call` calls `canonical_for(name)`
/// so both spellings route to the same provider arm.
fn canonical_for(name: &str) -> &str {{
    match name {{
{alias_arms}        other => other,
    }}
}}

// -----------------------------------------------------------
// Guest impls.
// -----------------------------------------------------------

struct Component;

impl MetadataGuest for Component {{
    fn describe() -> Manifest {{
        let scalar_functions: Vec<ScalarFunctionSpec> = vec![
{scalar_specs}        ];
        Manifest {{
            name: EXTENSION_ROOT.to_string(),
            version: CATALOG_VERSION.to_string(),
            scalar_functions,
            aggregate_functions: vec![],
            collations: vec![],
            vtabs: vec![],
            dot_commands: vec![],
            has_authorizer: false,
            has_update_hook: false,
            has_commit_hook: false,
            has_wal_hook: false,
            wal_hook_id: 0,
            declared_capabilities: vec![],
            optional_capabilities: vec![],
            preferred_prefix: None,
            prefix_expansion: None,
            typed_values: vec![],
        }}
    }}
}}

impl ScalarFunctionGuest for Component {{
    fn call(func_id: u64, args: Vec<SqlValue>) -> Result<SqlValue, String> {{
        let name = scalar_name_by_id(func_id)
            .ok_or_else(|| format!("unknown function id {{}}", func_id))?;
        // SQL-style null propagation. Providers that need to
        // observe explicit NULL arguments will need a follow-up
        // Phase to opt in per-arm.
        if args.iter().any(|v| matches!(v, SqlValue::Null)) {{
            return Ok(SqlValue::Null);
        }}
        // Alias translation: SQL name → WIT-canonical → provider
        // method (kebab-case). Compact-form SQL names (e.g.
        // `st_geomfromtext`) route through `canonical_for` to their
        // long form (`st_geom_from_text`) before kebab-casing;
        // non-alias names pass through unchanged. See
        // `canonical_for` above.
        let canonical = canonical_for(name);
        let method = canonical.replace('_', "-");
        let cbor_args: Vec<CborValue> = args
            .iter()
            .map(sqlv_to_cbor)
            .collect::<Result<Vec<_>, String>>()?;
        let resp = call(&method, cbor_args)?;
        response_to_sqlv(resp)
    }}
}}

bindings::export!(Component with_types_in bindings);
"##,
    )
}
