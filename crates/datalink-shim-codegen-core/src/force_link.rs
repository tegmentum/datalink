//! #557fix.2 / #565 force-link block emission.
//!
//! `render_force_link_upstream_imports` writes the Rust source of
//! the `__FORCE_LINK_UPSTREAM_FNS` + `__FORCE_LINK_UPSTREAM_TYPES`
//! `#[used]` statics injected into every bridge's `lib.rs`. The
//! block holds `*const ()` references to every upstream free
//! function + `core::ptr::null::<T>()` references to every reachable
//! upstream record/variant/enum/flags type, so wit-component keeps
//! the full import shape live for `wac plug` to satisfy from the
//! upstream's export shape.
//!
//! The emitted code is sqlite-agnostic — it never mentions
//! `SqlValue` — so the block lives in `core/`, shared by every
//! database emit target.
//!
//! Inlines `split_pkg` / `sanitize_module` / `pascal_case` /
//! `package_belongs_to_primary` as private helpers to keep this
//! module self-contained; `emit_sqlite::emit_lib` carries its own
//! copies for its much larger surface, and the two have no
//! visibility coupling.

use anyhow::Result;

use crate::wit_parse::{self, WitPackage};

/// #557fix W4a composition fix: render a `__force_link_upstream_imports`
/// block that holds a `*const ()` reference to every upstream free
/// function in every primary-shim package + a `PhantomData<T>` reference
/// to every upstream record/variant/enum/flags type. The block is held
/// alive by a `#[used]` static so LLVM keeps it past DCE; the function-
/// pointer casts in turn keep each wit-bindgen wrapper alive; the
/// wrapper's `extern "C"` canonical-ABI lowering is what wit-component
/// uses to decide which imported functions/types make it into the
/// component's import shape.
///
/// Without this, wit-component prunes the import shape down to "only
/// what the bridge actually calls" (~10 ttext functions instead of
/// upstream's 28). `wac plug` 0.10's encoder then can't satisfy the
/// trimmed import from upstream's full export, so the composition
/// leaves the socket's import slot open — and the composed loadable
/// fails to instantiate inside sqlink-host.
///
/// Restricted to PRIMARY-shim packages (`mobilitydb:temporal`,
/// `postgis:wasm`, ...). Helper-component packages (sfcgal-component,
/// raster-component, ...) keep their existing trim-on-demand behaviour
/// because they're not the failure surface — they aren't directly
/// imported by the bridge's vendored WIT, only transitively.
///
/// #811 (Fix B): resource methods (both static and instance) are
/// force-linked too. wit-bindgen emits every resource method as
/// `impl Resource { pub fn method(...) }` — the `Resource::method`
/// path is a Rust fn item that coerces to a fn pointer, which then
/// casts to `*const ()` just like a free-function path. Without
/// these references, wit-component prunes the trimmed methods off
/// the imported resource's method set, and wac plug 0.10's
/// structural-match check on the resource (plug's `postgis-types.geometry`
/// exports 16 methods; bridge's trimmed import declares only the
/// 2 it actually calls) fails with `resource types are not the
/// same on postgis:wasm/postgis-accessors@0.1.0`. Emitting every
/// method reference keeps the imported resource's method set
/// structurally equal to the plug's export set so the composition
/// closes.
pub fn render_force_link_upstream_imports(
    primary: &str,
    shim_packages: &[WitPackage],
    wit_deps_root: &std::path::Path,
) -> Result<String> {
    let wit_fns = wit_parse::parse_dir(wit_deps_root)?;

    // #565 (#557fix.2): compute the set of primary types wit-bindgen
    // will actually emit Rust definitions for. wit-bindgen DCEs any
    // import-side record/variant/enum/flags not referenced (transitively)
    // by some imported function signature. The pre-#565 emitter walked
    // EVERY type in the primary's packages, which broke the postgis
    // build on records that are declared but unreferenced
    // (`box3d`, `coord-z`, `buffer-params`, `extremes`, ...). Filter
    // by membership in the reachable closure so the force-link block
    // never references a type wit-bindgen has trimmed.
    let reachable_types =
        wit_parse::reachable_primary_types(&wit_fns, shim_packages, primary);

    // Collect free-function references: (pkg_ns, pkg_name, iface, func_snake).
    // #811: resource method references are collected as a separate list —
    // they need the extra `ResourcePascal::` path segment inserted between
    // the interface module and the fn name so the fn item resolves through
    // the inherent-impl namespace wit-bindgen emits.
    let mut fn_refs: Vec<(String, String, String, String)> = Vec::new();
    let mut method_refs: Vec<(String, String, String, String, String)> = Vec::new();
    for f in &wit_fns {
        if !package_belongs_to_primary(&f.package, primary) {
            continue;
        }
        let (pkg_ns, pkg_name) = split_pkg(&f.package);
        let pkg_ns_s = sanitize_module(&pkg_ns);
        let pkg_name_s = sanitize_module(&pkg_name);
        let iface = sanitize_module(&f.interface);
        if let Some(resource_kebab) = &f.resource {
            // #811: resource method / constructor / static-func.
            // wit-bindgen emits the resource as `struct <Pascal>` under
            // the interface module; methods live in an inherent `impl`
            // block. The fn item path is therefore
            // `bindings::<ns>::<name>::<iface>::<Pascal>::<fn_snake>`.
            let resource_pascal = pascal_case(resource_kebab);
            // Constructors are emitted as `<Pascal>::new(...)` by
            // wit-bindgen even though the WIT spells it
            // `constructor(...)`. The kebab_name we synthesise
            // upstream is `create-<resource_kebab>`, but the Rust
            // path segment is always `new`.
            let method_snake = if f.is_constructor {
                "new".to_string()
            } else {
                wit_parse::kebab_to_snake(&f.kebab_name)
            };
            method_refs.push((
                pkg_ns_s,
                pkg_name_s,
                iface,
                resource_pascal,
                method_snake,
            ));
        } else {
            let func = wit_parse::kebab_to_snake(&f.kebab_name);
            fn_refs.push((pkg_ns_s, pkg_name_s, iface, func));
        }
    }

    // Collect type references: (pkg_ns, pkg_name, iface, type_pascal).
    // wit-bindgen converts kebab record names to PascalCase Rust idents,
    // so `tbool-sequence` → `TboolSequence`, `temporal-error` →
    // `TemporalError`, `interpolation` → `Interpolation`. The
    // `pascal_case` helper already implements this convention.
    //
    // #565: each candidate is gated against `reachable_types`. The
    // reachable set is keyed by the original (package, interface,
    // kebab_name) triple — the same key shape we iterate on here
    // before sanitising for Rust path emission.
    let mut type_refs: Vec<(String, String, String, String)> = Vec::new();
    for pkg in shim_packages {
        if !package_belongs_to_primary(&pkg.ns_name, primary) {
            continue;
        }
        let (pkg_ns, pkg_name) = split_pkg(&pkg.ns_name);
        let pkg_ns_s = sanitize_module(&pkg_ns);
        let pkg_name_s = sanitize_module(&pkg_name);
        let push_if_reachable =
            |iface_kebab: &str, type_kebab: &str, sink: &mut Vec<_>| {
                if !reachable_types.contains(&(
                    pkg.ns_name.clone(),
                    iface_kebab.to_string(),
                    type_kebab.to_string(),
                )) {
                    return;
                }
                sink.push((
                    pkg_ns_s.clone(),
                    pkg_name_s.clone(),
                    sanitize_module(iface_kebab),
                    pascal_case(type_kebab),
                ));
            };
        for r in &pkg.records {
            push_if_reachable(&r.interface, &r.kebab_name, &mut type_refs);
        }
        for v in &pkg.variants {
            push_if_reachable(&v.interface, &v.kebab_name, &mut type_refs);
        }
        for e in &pkg.enums {
            push_if_reachable(&e.interface, &e.kebab_name, &mut type_refs);
        }
        for f in &pkg.flags {
            push_if_reachable(&f.interface, &f.kebab_name, &mut type_refs);
        }
    }

    if fn_refs.is_empty() && type_refs.is_empty() && method_refs.is_empty() {
        return Ok(String::new());
    }

    // Dedupe (interface may declare a record under multiple imports if
    // it's vendored more than once — shouldn't happen, but stay tidy).
    fn_refs.sort();
    fn_refs.dedup();
    type_refs.sort();
    type_refs.dedup();
    method_refs.sort();
    method_refs.dedup();

    // Emit a `#[used]` static slice of function-pointer addresses.
    // Initialising a static REQUIRES each address at compile time, so
    // the linker has to keep every referenced fn symbol alive — exactly
    // what we need for wit-component to include them in the import
    // shape. `#[used]` keeps the static itself past LLVM DCE.
    //
    // Type pointers are emitted as a separate `#[used]` static via
    // `core::ptr::null::<T>()` wrapped in the Sync newtype. That
    // counts T as referenced for wit-component's
    // `process_live_type_imports` even when no function in the
    // upstream interface mentions T (the case for the records-only
    // `mobilitydb:temporal/types` interface).
    let fn_lines: Vec<String> = fn_refs
        .iter()
        .map(|(ns, name, iface, func)| {
            format!(
                "    __ForceLinkPtr(bindings::{ns}::{name}::{iface}::{func} as *const ()),"
            )
        })
        .collect();
    // #811: resource-method references share the same `#[used]` static
    // as free functions since both are fn-item casts to `*const ()`.
    // Emit them alongside the free-function lines under a section
    // header so the diff surface stays legible.
    let method_lines: Vec<String> = method_refs
        .iter()
        .map(|(ns, name, iface, resource, method)| {
            format!(
                "    __ForceLinkPtr(bindings::{ns}::{name}::{iface}::{resource}::{method} as *const ()),"
            )
        })
        .collect();
    let type_lines: Vec<String> = type_refs
        .iter()
        .map(|(ns, name, iface, ty)| {
            format!(
                "    __ForceLinkPtr(\n        core::ptr::null::<bindings::{ns}::{name}::{iface}::{ty}>()\n            as *const (),\n    ),"
            )
        })
        .collect();
    let fn_count = fn_lines.len() + method_lines.len();
    let type_count = type_lines.len();
    let mut fn_block = fn_lines.join("\n");
    if !method_lines.is_empty() {
        if !fn_block.is_empty() {
            fn_block.push('\n');
        }
        fn_block.push_str("    // #811: resource methods (static + instance + constructors).\n");
        fn_block.push_str(&method_lines.join("\n"));
    }
    let type_block = type_lines.join("\n");

    Ok(format!(
        "\n\
        // ── #557fix W4a composition fix: force-link upstream imports ──\n\
        //\n\
        // wit-component (run as part of the wasm32-wasip2 lower step)\n\
        // walks the actually-lowered functions in the core wasm and\n\
        // encodes the bridge's component imports from that set.\n\
        // Imported functions/types the bridge never references end up\n\
        // trimmed out of the import shape — which then can't be plugged\n\
        // from upstream's full export shape because wac plug 0.10's\n\
        // encoder doesn't synthesise a trimming adapter.\n\
        //\n\
        // Two `#[used]` statics force-keep references to every upstream\n\
        // free function (as `*const ()` casts of fn-item addresses) and\n\
        // every upstream record/variant/enum/flags type (as `*const T`\n\
        // null pointers — the `as *const ()` coercion forces T to be\n\
        // considered live by wit-component's `process_live_type_imports`\n\
        // pass). The Sync newtype keeps the slice initialisable in static\n\
        // context.\n\
        #[cfg(target_arch = \"wasm32\")]\n\
        #[doc(hidden)]\n\
        #[repr(transparent)]\n\
        struct __ForceLinkPtr(#[allow(dead_code)] *const ());\n\
        \n\
        // SAFETY: addresses of static functions are immutable; the\n\
        // slice is read-only and never dereferenced.\n\
        #[cfg(target_arch = \"wasm32\")]\n\
        unsafe impl Sync for __ForceLinkPtr {{}}\n\
        \n\
        #[cfg(target_arch = \"wasm32\")]\n\
        #[doc(hidden)]\n\
        #[used]\n\
        static __FORCE_LINK_UPSTREAM_FNS: [__ForceLinkPtr; {fn_count}] = [\n\
        {fn_block}\n\
        ];\n\
        \n\
        #[cfg(target_arch = \"wasm32\")]\n\
        #[doc(hidden)]\n\
        #[used]\n\
        static __FORCE_LINK_UPSTREAM_TYPES: [__ForceLinkPtr; {type_count}] = [\n\
        {type_block}\n\
        ];\n\
        ",
        fn_count = fn_count,
        type_count = type_count,
        fn_block = fn_block,
        type_block = type_block,
    ))
}

// ── Local copies of pkg/name-shape helpers ────────────────────────
//
// These mirror the same-named helpers in `emit_sqlite::emit_lib`.
// Duplicated rather than shared because the dep direction is
// core ← emit_sqlite, not the other way around — emit_lib's
// versions keep their own private copies for the SqlValue-aware
// emit surface. Both copies stay byte-identical; a future shared
// `core::util` could consolidate them.

/// Split `"ns:name"` into `("ns", "name")`. Falls back to the whole
/// string as namespace + empty name if no colon.
fn split_pkg(pkg: &str) -> (String, String) {
    match pkg.find(':') {
        Some(i) => (pkg[..i].to_string(), pkg[i + 1..].to_string()),
        None => (pkg.to_string(), String::new()),
    }
}

/// Convert a WIT package namespace or name to its Rust module ident
/// as wit-bindgen would generate it (kebab → snake).
fn sanitize_module(s: &str) -> String {
    s.replace('-', "_")
}

fn pascal_case(s: &str) -> String {
    let mut out = String::new();
    let mut up = true;
    for c in s.chars() {
        if c == '-' || c == '_' || c.is_whitespace() {
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

/// True when `package` is owned by the same namespace as `primary`
/// (`mobilitydb:temporal` ↔ `mobilitydb`, `postgis:wasm` ↔ `postgis`).
/// Mirrors `emit_sqlite::emit_wit::package_belongs_to_primary`.
fn package_belongs_to_primary(package: &str, primary: &str) -> bool {
    package.split(':').next().map(|ns| ns == primary).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// #811: force-link block emits `Resource::method` references for
    /// every static + instance method + constructor on every imported
    /// primary-shim resource. Without these lines, wit-component prunes
    /// the trimmed methods off the bridge's imported resource, and
    /// `wac plug` 0.10's structural check fails because the plug
    /// exports all N methods while the bridge only imports the K
    /// methods it actually calls.
    #[test]
    fn force_link_emits_resource_methods_811() {
        let tmp = std::env::temp_dir().join(format!(
            "datalink-811-force-link-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir tmp");
        fs::write(
            tmp.join("types.wit"),
            "package postgis:wasm@0.1.0;\n\
             interface postgis-types {\n\
                 resource geometry {\n    \
                     from-wkb: static func(wkb: list<u8>) -> geometry;\n    \
                     from-wkt: static func(wkt: string) -> geometry;\n    \
                     as-wkb: func() -> list<u8>;\n    \
                     srid: func() -> option<s32>;\n    \
                     clone: func() -> geometry;\n\
                 }\n\
             }\n",
        )
        .expect("write types.wit");

        // The force-link renderer needs the shim_packages metadata to
        // walk records; supply a minimal one that surfaces the
        // resource decl.
        let pkg = crate::wit_parse::WitPackage {
            ns_name: "postgis:wasm".to_string(),
            version: "0.1.0".to_string(),
            interfaces: vec!["postgis-types".to_string()],
            records: vec![],
            resources: vec![crate::wit_parse::WitResourceDecl {
                interface: "postgis-types".to_string(),
                kebab_name: "geometry".to_string(),
            }],
            variants: vec![],
            enums: vec![],
            flags: vec![],
            type_aliases: vec![],
        };
        let src =
            render_force_link_upstream_imports("postgis", &[pkg], &tmp).expect("render");
        for m in &["from_wkb", "from_wkt", "as_wkb", "srid", "clone"] {
            assert!(
                src.contains(&format!(
                    "bindings::postgis::wasm::postgis_types::Geometry::{m} as *const ()"
                )),
                "force-link block missing Geometry::{m}; got:\n{src}"
            );
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    /// #811 corollary: constructors ride the same fn-item path as
    /// methods but wit-bindgen names them `<Pascal>::new` (the WIT
    /// spells the line `constructor(...)` and the parser synthesises
    /// kebab `create-<resource>`). The force-link block must reference
    /// `<Pascal>::new` for those, not `create_<resource>`.
    #[test]
    fn force_link_maps_constructor_to_new_811() {
        let tmp = std::env::temp_dir().join(format!(
            "datalink-811-ctor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir tmp");
        fs::write(
            tmp.join("topology.wit"),
            "package postgis:wasm@0.1.0;\n\
             interface postgis-topology-types {\n\
                 resource topology {\n    \
                     constructor(name: string, srid: s32);\n    \
                     node-count: func() -> u32;\n\
                 }\n\
             }\n",
        )
        .expect("write topology.wit");

        let pkg = crate::wit_parse::WitPackage {
            ns_name: "postgis:wasm".to_string(),
            version: "0.1.0".to_string(),
            interfaces: vec!["postgis-topology-types".to_string()],
            records: vec![],
            resources: vec![crate::wit_parse::WitResourceDecl {
                interface: "postgis-topology-types".to_string(),
                kebab_name: "topology".to_string(),
            }],
            variants: vec![],
            enums: vec![],
            flags: vec![],
            type_aliases: vec![],
        };
        let src =
            render_force_link_upstream_imports("postgis", &[pkg], &tmp).expect("render");
        assert!(
            src.contains(
                "bindings::postgis::wasm::postgis_topology_types::Topology::new as *const ()"
            ),
            "constructor should map to Topology::new; got:\n{src}"
        );
        assert!(
            src.contains(
                "bindings::postgis::wasm::postgis_topology_types::Topology::node_count as *const ()"
            ),
            "instance method should be captured; got:\n{src}"
        );
        // The synthesised `create-topology` kebab MUST NOT leak into
        // the emitted path — wit-bindgen doesn't emit `create_topology`.
        assert!(
            !src.contains("create_topology"),
            "synthesised create-topology kebab leaked into force-link; got:\n{src}"
        );
        let _ = fs::remove_dir_all(&tmp);
    }
}
