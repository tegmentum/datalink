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
/// Resource methods are skipped: wit-bindgen emits them as `impl
/// Resource { fn method(&self, ...) }` rather than free functions, so
/// they don't have a stable fn-pointer form. Postgis's force-link
/// block is intentionally non-exhaustive for resources today; the
/// observation is that the postgis composition was already clean
/// (only the W4a vendoring tripped the structural-equivalence wire),
/// so resource-method coverage is a follow-up.
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
    let mut fn_refs: Vec<(String, String, String, String)> = Vec::new();
    for f in &wit_fns {
        if !package_belongs_to_primary(&f.package, primary) {
            continue;
        }
        if f.resource.is_some() {
            continue;
        }
        let (pkg_ns, pkg_name) = split_pkg(&f.package);
        let pkg_ns_s = sanitize_module(&pkg_ns);
        let pkg_name_s = sanitize_module(&pkg_name);
        let iface = sanitize_module(&f.interface);
        let func = wit_parse::kebab_to_snake(&f.kebab_name);
        fn_refs.push((pkg_ns_s, pkg_name_s, iface, func));
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

    if fn_refs.is_empty() && type_refs.is_empty() {
        return Ok(String::new());
    }

    // Dedupe (interface may declare a record under multiple imports if
    // it's vendored more than once — shouldn't happen, but stay tidy).
    fn_refs.sort();
    fn_refs.dedup();
    type_refs.sort();
    type_refs.dedup();

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
    let type_lines: Vec<String> = type_refs
        .iter()
        .map(|(ns, name, iface, ty)| {
            format!(
                "    __ForceLinkPtr(\n        core::ptr::null::<bindings::{ns}::{name}::{iface}::{ty}>()\n            as *const (),\n    ),"
            )
        })
        .collect();
    let fn_count = fn_lines.len();
    let type_count = type_lines.len();
    let fn_block = fn_lines.join("\n");
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
