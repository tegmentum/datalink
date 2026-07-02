//! Emit the WIT world + vendored deps for the wasm-component
//! bridge (datafission target).
//!
//! The world `include`s the canonical composite world
//! `datafission:extension/extension@1.0.0` (which transitively
//! exports identity, sql-extension-plugin/metadata, scalar /
//! aggregate / window / table function registries, custom-type,
//! multi-custom-type, spatial-index, system-catalog, and index)
//! AND adds `import` declarations for every upstream shim
//! interface the bridge delegates scalar work to (the same
//! `postgis:wasm/*` / `mobilitydb:temporal/*` set the SQLite and
//! DuckDB targets consume).
//!
//! The vendored `wit/deps/` holds the seven datafission packages
//! (extension + function-plugin + sql-extension-plugin + type-plugin
//! + spatial-index-plugin + system-catalog-plugin + index-plugin)
//! plus the upstream shim packages.
//!
//! ## Source locations
//!
//! Per-primary upstream-shim WIT resolution is shared with the
//! SQLite + DuckDB targets via
//! `datalink-shim-codegen-core::wit_paths::source_shim_deps_dir`
//! (#654). The default path is the upstream-synthesised tree
//! (#651) — `~/git/postgis-wasm/wit/` or
//! `~/git/mobilitydb-wasm/crates/mdb-temporal-wasm/wit/` —
//! falling back to the bridge's vendored `wit/deps/` when
//! upstream isn't checked out.
//!
//! Datafission extension WIT:
//!   * `$DATAFISSION_EXTENSION_WIT_DEPS=...` (overrides the search;
//!     should point at a `wit/deps/` directory containing the seven
//!     datafission packages already laid out, e.g.
//!     `~/git/datafission/extensions/postgis/wit/deps`)
//!   * `$DATAFISSION_WIT=...` (point at the canonical
//!     `~/git/datafission/wit` and let the codegen vendor each
//!     plugin package itself)
//!   * `$HOME/git/datafission/extensions/<primary>/wit/deps`
//!     (use the per-extension pre-laid-out deps tree)
//!   * `$HOME/git/datafission/wit` (vendor from the canonical
//!     source-of-truth dir)

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use shim_bridge_codegen_core::BridgePlan;
use datalink_shim_codegen_core::kebab_fix::kebab_fix_wit;
use datalink_shim_codegen_core::wit_parse::{self, WitPackage};

// #654: WIT-deps resolution (incl. #651 upstream synthesis) lifted
// to `datalink-shim-codegen-core::wit_paths` so every emit target
// shares one implementation. Re-exported so existing callers like
// `emit_wit::source_shim_deps_dir(...)` keep working unchanged.
pub use datalink_shim_codegen_core::wit_paths::source_shim_deps_dir;

/// Write `wit/world.wit`.
pub fn write_world(plan: &BridgePlan, dest: &Path) -> Result<()> {
    let primary = primary_extension_name(plan);
    let shim_deps = source_shim_deps_dir(primary)?;
    let shim_packages = discover_shim_packages(&shim_deps)?;
    let datafission_packages = discover_datafission_packages()?;
    let world = render_world(primary, &shim_packages, &datafission_packages)?;
    fs::write(dest, world).with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

/// Copy the dependency WIT tree into `wit/deps/`.
///
/// Every subdir of the source shim deps tree that holds a
/// well-formed package is copied as-is, EXCLUDING:
///   - `sqlite-extension` — not part of the datafission world.
///   - `duckdb-extension` — not part of the datafission world.
///   - The seven `datafission:*` packages — those are sourced
///     separately from the canonical datafission WIT location so
///     a shim-side stale copy can't drift the contract.
pub fn write_deps(plan: &BridgePlan, deps_dir: &Path) -> Result<()> {
    let primary = primary_extension_name(plan);
    let shim_src = source_shim_deps_dir(primary)?;
    for entry in fs::read_dir(&shim_src)? {
        let entry = entry?;
        let from = entry.path();
        if !from.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if matches!(
            name_str.as_ref(),
            "sqlite-extension"
                | "duckdb-extension"
                | "extension"
                | "function-plugin"
                | "sql-extension-plugin"
                | "type-plugin"
                | "spatial-index-plugin"
                | "system-catalog-plugin"
                | "index-plugin"
        ) {
            // SQLite + DuckDB contracts aren't part of the
            // datafission world. The seven datafission packages
            // ride through the canonical source-of-truth copy
            // below.
            continue;
        }
        let to = deps_dir.join(&name);
        copy_tree(&from, &to)
            .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
    }
    let datafission_wit_root = source_datafission_wit_root()?;
    for pkg_dir_name in DATAFISSION_PACKAGE_DIRS {
        let from = datafission_wit_root.join(pkg_dir_name);
        if !from.is_dir() {
            return Err(anyhow!(
                "datafission WIT package directory missing: {}",
                from.display()
            ));
        }
        let to = deps_dir.join(pkg_dir_name);
        copy_tree(&from, &to).with_context(|| {
            format!(
                "copying datafission wit/{} -> {}",
                pkg_dir_name,
                to.display()
            )
        })?;
    }
    Ok(())
}

/// Locate the canonical datafission WIT root.
///
/// Resolution order:
///   1. `$DATAFISSION_EXTENSION_WIT_DEPS` — point at an
///      already-laid-out `wit/deps/` (e.g. the postgis extension's
///      own `wit/deps/`). When set, this dir is used VERBATIM as the
///      package source.
///   2. `$DATAFISSION_WIT` — explicit override pointing at
///      `~/git/datafission/wit` (or wherever the seven plugin
///      packages live).
///   3. `$HOME/git/datafission/extensions/<primary>/wit/deps`
///      (per-extension pre-laid-out deps tree).
///   4. `$HOME/git/datafission/wit` (canonical source-of-truth).
fn source_datafission_wit_root() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("DATAFISSION_EXTENSION_WIT_DEPS") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Ok(p);
        }
        return Err(anyhow!(
            "DATAFISSION_EXTENSION_WIT_DEPS={} does not exist",
            p.display()
        ));
    }
    if let Ok(p) = std::env::var("DATAFISSION_WIT") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Ok(p);
        }
        return Err(anyhow!(
            "DATAFISSION_WIT={} does not exist",
            p.display()
        ));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home_pb = PathBuf::from(&home);
        let per_ext = home_pb.join("git/datafission/extensions");
        if per_ext.is_dir() {
            for entry in fs::read_dir(&per_ext)?.flatten() {
                let pe_wit = entry.path().join("wit/deps");
                if pe_wit.is_dir() && pe_wit.join("extension/world.wit").is_file() {
                    return Ok(pe_wit);
                }
            }
        }
        let canonical = home_pb.join("git/datafission/wit");
        if canonical.is_dir() {
            return Ok(canonical);
        }
    }
    Err(anyhow!(
        "cannot locate datafission wit/. Set DATAFISSION_WIT=/path/to/datafission/wit \
         or DATAFISSION_EXTENSION_WIT_DEPS=/path/to/wit/deps"
    ))
}

/// Discover datafission's `extension` package, used to render the
/// world's `include` statement with the correct version pin.
pub fn discover_datafission_extension_package(_primary: &str) -> Result<WitPackage> {
    let root = source_datafission_wit_root()?;
    let pkg_dir = root.join("extension");
    let pkg = wit_parse::parse_package_dir(&pkg_dir)?.ok_or_else(|| {
        anyhow!(
            "datafission extension wit dir {} has no parseable package declaration",
            pkg_dir.display()
        )
    })?;
    Ok(pkg)
}

/// Discover all seven canonical datafission plugin packages by
/// walking the source-of-truth WIT tree and parsing each package
/// directory's declaration. Each returned `WitPackage` carries
/// `ns_name` ("datafission:type-plugin") + `version` ("1.0.0"), so
/// the world renderer can pin every per-interface export at the
/// version actually shipped by the vendored WIT — no hardcoded
/// `@1.0.0` strings on the bridge side.
pub fn discover_datafission_packages() -> Result<Vec<WitPackage>> {
    let root = source_datafission_wit_root()?;
    let mut out = Vec::with_capacity(DATAFISSION_PACKAGE_DIRS.len());
    for pkg_dir_name in DATAFISSION_PACKAGE_DIRS {
        let pkg_dir = root.join(pkg_dir_name);
        let pkg = wit_parse::parse_package_dir(&pkg_dir)?.ok_or_else(|| {
            anyhow!(
                "datafission wit dir {} has no parseable package declaration",
                pkg_dir.display()
            )
        })?;
        out.push(pkg);
    }
    Ok(out)
}

/// Look up a datafission plugin package by its `ns_name`
/// (e.g. `"datafission:type-plugin"`). Errors when the package
/// isn't present in the discovered set so a missing-vendoring
/// surface change is caught at codegen time rather than producing
/// a silently-mis-versioned world.
fn pkg_version<'a>(packages: &'a [WitPackage], ns_name: &str) -> Result<&'a str> {
    packages
        .iter()
        .find(|p| p.ns_name == ns_name)
        .map(|p| p.version.as_str())
        .ok_or_else(|| anyhow!("datafission package '{ns_name}' not discovered"))
}

/// Walk the shim deps tree and parse every package subdir into a
/// `WitPackage`. The datafission packages are loaded separately
/// (from the canonical datafission WIT location).
pub fn discover_shim_packages(deps_root: &Path) -> Result<Vec<WitPackage>> {
    let mut out = Vec::new();
    if !deps_root.is_dir() {
        return Ok(out);
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(deps_root)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    for d in entries {
        let name = d.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(
            name,
            "sqlite-extension"
                | "duckdb-extension"
                | "extension"
                | "function-plugin"
                | "sql-extension-plugin"
                | "type-plugin"
                | "spatial-index-plugin"
                | "system-catalog-plugin"
                | "index-plugin"
        ) {
            continue;
        }
        if let Some(pkg) = wit_parse::parse_package_dir(&d)
            .with_context(|| format!("parsing {}", d.display()))?
        {
            out.push(pkg);
        }
    }
    Ok(out)
}

/// Render `world.wit` from the parsed packages.
///
/// The world emits each canonical datafission per-capability
/// interface export explicitly (rather than `include`ing the
/// composite extension world). This lets the bridge drop the
/// single-type `type-plugin/custom-type` placeholder while still
/// advertising every other capability — components that need
/// multi-type registration target `multi-custom-type` instead,
/// which the composite world deliberately omits.
///
/// Per-interface version pins are auto-detected from the parsed
/// `WitPackage` set so a future contract bump (e.g. metadata
/// 1.1.0 → 1.2.0) flows through without a codegen edit.
///
/// Each shim-package import is added so the bridge can delegate
/// scalar work to the upstream component via `wac plug`.
pub fn render_world(
    primary: &str,
    shim_packages: &[WitPackage],
    datafission_packages: &[WitPackage],
) -> Result<String> {
    let ext_ver = pkg_version(datafission_packages, "datafission:extension")?;
    let sql_ext_ver = pkg_version(datafission_packages, "datafission:sql-extension-plugin")?;
    let spatial_idx_ver =
        pkg_version(datafission_packages, "datafission:spatial-index-plugin")?;
    let sys_cat_ver =
        pkg_version(datafission_packages, "datafission:system-catalog-plugin")?;
    let fn_ver = pkg_version(datafission_packages, "datafission:function-plugin")?;
    let type_ver = pkg_version(datafission_packages, "datafission:type-plugin")?;
    let idx_ver = pkg_version(datafission_packages, "datafission:index-plugin")?;

    let mut s = String::new();
    s.push_str(&format!(
        "package datafission-bridge:{primary}@0.1.0;\n\n",
    ));
    s.push_str("/// Generated by sqlink-shim-codegen (target=datafission).\n");
    s.push_str("/// Bridges the shim's WIT-exposed surface onto the\n");
    s.push_str("/// canonical datafission per-capability contract.\n");
    s.push_str("/// Imports are derived from the shim's vendored WIT\n");
    s.push_str("/// packages; exports list every datafission capability\n");
    s.push_str("/// EXCEPT type-plugin/custom-type (the single-type\n");
    s.push_str("/// placeholder is intentionally dropped — components\n");
    s.push_str("/// that register custom types target\n");
    s.push_str("/// type-plugin/multi-custom-type instead). Per-interface\n");
    s.push_str("/// version pins are auto-detected from the vendored\n");
    s.push_str("/// package declarations at codegen time.\n");
    s.push_str("world bridge {\n");

    // Host-provided logging — same package as `extension`.
    s.push_str(&format!(
        "    import datafission:extension/logging@{ext_ver};\n",
    ));
    s.push('\n');

    // Identity + per-capability exports. The composite
    // `extension` world groups identity / sql-extension-plugin
    // metadata / spatial-index / system-catalog / function
    // registries / type-plugin / index-plugin; we list each
    // export explicitly so type-plugin/custom-type can be skipped
    // without dragging the rest along.
    s.push_str(&format!(
        "    export datafission:extension/identity@{ext_ver};\n",
    ));
    s.push_str(&format!(
        "    export datafission:sql-extension-plugin/metadata@{sql_ext_ver};\n",
    ));
    s.push_str(&format!(
        "    export datafission:spatial-index-plugin/spatial-index@{spatial_idx_ver};\n",
    ));
    s.push_str(&format!(
        "    export datafission:system-catalog-plugin/system-catalog@{sys_cat_ver};\n",
    ));
    s.push_str(&format!(
        "    export datafission:function-plugin/scalar-function-registry@{fn_ver};\n",
    ));
    s.push_str(&format!(
        "    export datafission:function-plugin/aggregate-function-registry@{fn_ver};\n",
    ));
    s.push_str(&format!(
        "    export datafission:function-plugin/table-function-registry@{fn_ver};\n",
    ));
    s.push_str(&format!(
        "    export datafission:function-plugin/window-function-registry@{fn_ver};\n",
    ));
    // Skip datafission:type-plugin/custom-type — components target
    // multi-custom-type instead (see header doc-comment).
    s.push_str(&format!(
        "    export datafission:type-plugin/multi-custom-type@{type_ver};\n",
    ));
    s.push_str(&format!(
        "    export datafission:index-plugin/index@{idx_ver};\n",
    ));
    s.push('\n');

    // Shim imports — every interface in every shim package.
    for pkg in shim_packages {
        for iface in &pkg.interfaces {
            s.push_str(&format!(
                "    import {ns}/{iface}@{ver};\n",
                ns = pkg.ns_name,
                iface = iface,
                ver = pkg.version,
            ));
        }
    }
    s.push_str("}\n");
    Ok(s)
}

/// The seven datafission WIT packages that get vendored into the
/// bridge's `wit/deps/`. Directory names follow the canonical
/// `~/git/datafission/wit/<plugin>/` layout.
pub const DATAFISSION_PACKAGE_DIRS: &[&str] = &[
    "extension",
    "function-plugin",
    "sql-extension-plugin",
    "type-plugin",
    "spatial-index-plugin",
    "system-catalog-plugin",
    "index-plugin",
];

pub(crate) fn primary_extension_name(plan: &BridgePlan) -> &str {
    plan.extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or("shim")
}

/// Returns true when `package` is the primary shim's own package
/// (mirrors the helper in sqlite-emit / duckdb-emit's emit_wit).
pub fn package_belongs_to_primary(package: &str, primary: &str) -> bool {
    package.split(':').next().map(|ns| ns == primary).unwrap_or(false)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Err(anyhow!("source {} is not a directory", src.display()));
    }
    // #812: if `dst` is a symlink pointing at the SAME canonical
    // location as `src` (datafission extensions ship
    // `wit/deps/<pkg> -> ../../../../wit/<pkg>` symlinks whose target
    // IS the canonical source-of-truth), the copy is a no-op — the
    // symlink already ferries the source content through to the
    // destination. Preserving the symlink keeps the extension's
    // `wit/deps/` structure intact across regens (the pre-fix code
    // clobbered every symlinked subdir into a real directory on each
    // pass, silently drifting from the intended `wit/deps` layout).
    if let Ok(meta) = fs::symlink_metadata(dst) {
        if meta.file_type().is_symlink() {
            if same_file(src, dst) {
                return Ok(());
            }
            // #656: dst is a symlink pointing SOMEWHERE ELSE than
            // src's canonical (e.g. stale symlink from a previous
            // vendoring). Every subsequent path operation that joins
            // onto `dst` would traverse the symlink; the umbrella-prune
            // below (#642) would then `unlink` files at the wrong
            // location. Replace the symlink with a real directory
            // before continuing — the copy loop repopulates it from
            // `src`.
            fs::remove_file(dst).with_context(|| {
                format!("removing dst symlink {}", dst.display())
            })?;
        }
    }
    fs::create_dir_all(dst)?;
    // #642: when the upstream shim splits an umbrella `world.wit` into
    // per-interface .wit files, a stale `dst/world.wit` left over from
    // a previous regen still declares the same interfaces inline,
    // triggering a "duplicate item" parse error. Drop the stale file
    // before copying — if the source still owns a `world.wit`, the
    // loop below copies it right back; if not, it stays gone.
    // Use `symlink_metadata` (not `exists`) so we don't follow a
    // `world.wit` symlink and `unlink` the file at its target.
    let stale_world = dst.join("world.wit");
    match fs::symlink_metadata(&stale_world) {
        Ok(_) => {
            // Regular file or symlink — either way, `remove_file` only
            // unlinks the directory entry at this path (does not follow
            // a final-component symlink), which is what we want.
            fs::remove_file(&stale_world)
                .with_context(|| format!("removing stale {}", stale_world.display()))?;
        }
        Err(_) => {}
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // #812: if a target-tree entry is already a symlink pointing at
        // the corresponding source, skip it — the recursive `copy_tree`
        // below would otherwise clobber it into a real directory on
        // every regen. The top-level guard handles `dst` itself; this
        // guard covers per-subdir symlinks inside `dst`.
        if let Ok(to_meta) = fs::symlink_metadata(&to) {
            if to_meta.file_type().is_symlink() && same_file(&from, &to) {
                continue;
            }
        }
        if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else if file_type.is_symlink() {
            // #623: resolve symlinks so generated bridges are
            // standalone (datafission per-extension wit/deps are
            // symlink farms; without this, the codegen silently
            // skipped them).
            let resolved = fs::canonicalize(&from)
                .with_context(|| format!("resolve symlink {}", from.display()))?;
            if resolved.is_dir() {
                copy_tree(&resolved, &to)?;
            } else if resolved.is_file() {
                if same_file(&resolved, &to) { continue; }
                copy_file_with_kebab_fix(&resolved, &to)?;
            }
        } else if file_type.is_file() {
            if same_file(&from, &to) {
                continue;
            }
            copy_file_with_kebab_fix(&from, &to)?;
        }
    }
    Ok(())
}

/// Copy `from` to `to`, applying the WIT kebab-fix
/// (`-2d` / `-3d` trailing-segment rewrite, #655) when the source is
/// a `.wit` file. Non-WIT files pass through `fs::copy` unchanged so
/// binary artifacts in a WIT tree (rare, but the codegen treats deps
/// trees opaquely) aren't corrupted by a text-mode round-trip.
fn copy_file_with_kebab_fix(from: &Path, to: &Path) -> Result<()> {
    let is_wit = from.extension().and_then(|s| s.to_str()) == Some("wit");
    if is_wit {
        let text = fs::read_to_string(from)
            .with_context(|| format!("read {}", from.display()))?;
        let fixed = kebab_fix_wit(&text);
        fs::write(to, fixed)
            .with_context(|| format!("write kebab-fixed {}", to.display()))?;
    } else {
        fs::copy(from, to)
            .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// #656 + #812: when `dst` is a symlink pointing back at the source
    /// WIT tree, `copy_tree` must not delete source files through the
    /// symlink. The pre-#656 code path called `fs::remove_file` on
    /// `dst.join("world.wit")`, whose path traversal followed the dst
    /// symlink and unlinked the file at the source location. #656's
    /// original fix replaced the dst symlink with a real directory;
    /// #812 tightens that to PRESERVE the symlink when it already
    /// points at the source (the datafission extension wit/deps
    /// symlink layout is intentional and shouldn't be clobbered on
    /// every regen).
    #[test]
    fn copy_tree_preserves_dst_symlink_to_source() {
        let tmp = std::env::temp_dir().join(format!(
            "datafission-812-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let src = tmp.join("src/wit/extension");
        let out_deps = tmp.join("out/wit/deps");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&out_deps).unwrap();
        let source_world = src.join("world.wit");
        fs::write(&source_world, "source content").unwrap();

        let dst = out_deps.join("extension");
        symlink(&src, &dst).unwrap();

        // src has a single regular file (world.wit) which copy_tree
        // would iterate over — but since dst is a symlink pointing
        // back at src, the top-level short-circuit skips the copy.
        copy_tree(&src, &dst).unwrap();

        // Source file must still exist with original content (#656).
        assert!(
            source_world.is_file(),
            "source world.wit was deleted by copy_tree"
        );
        let after = fs::read_to_string(&source_world).unwrap();
        assert_eq!(after, "source content");

        // #812: dst stays a symlink (no clobbering).
        let meta = fs::symlink_metadata(&dst).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "#812 regression: dst symlink was replaced with real dir"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    /// #812: when a per-subdir target is a symlink pointing back at
    /// the corresponding source subdir, the subdir walk should skip
    /// it — otherwise the recursive `copy_tree` call re-enters and
    /// clobbers the symlink into a real dir even though the top-level
    /// dst wasn't a symlink.
    #[test]
    fn copy_tree_preserves_subdir_symlink_to_source() {
        let tmp = std::env::temp_dir().join(format!(
            "datafission-812-sub-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let src = tmp.join("src");
        let dst = tmp.join("dst");
        let src_pkg = src.join("postgis-wasm");
        fs::create_dir_all(&src_pkg).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src_pkg.join("world.wit"), "pkg content").unwrap();

        // dst has a subdir symlink at postgis-wasm -> src/postgis-wasm.
        symlink(&src_pkg, dst.join("postgis-wasm")).unwrap();

        copy_tree(&src, &dst).unwrap();

        let subdir_meta = fs::symlink_metadata(dst.join("postgis-wasm")).unwrap();
        assert!(
            subdir_meta.file_type().is_symlink(),
            "#812 regression: subdir symlink was replaced with real dir"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    /// #642 must keep working: a stale REAL `world.wit` file in dst
    /// (not a symlink) is still pruned before the copy loop runs.
    #[test]
    fn copy_tree_still_prunes_real_stale_world() {
        let tmp = std::env::temp_dir().join(format!(
            "datafission-642-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let src = tmp.join("src/wit/extension");
        let dst = tmp.join("out/wit/deps/extension");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        // Source has no world.wit (umbrella was split into pieces).
        fs::write(src.join("piece.wit"), "interface piece {}").unwrap();
        // dst has a stale REAL world.wit from a previous regen.
        fs::write(dst.join("world.wit"), "stale umbrella").unwrap();

        copy_tree(&src, &dst).unwrap();

        assert!(
            !dst.join("world.wit").exists(),
            "#642 regression: stale real world.wit was not pruned"
        );
        assert!(dst.join("piece.wit").is_file());

        let _ = fs::remove_dir_all(&tmp);
    }
}
