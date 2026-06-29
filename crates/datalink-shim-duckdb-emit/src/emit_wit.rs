//! Emit the WIT world + vendored deps for the wasm-component
//! bridge (DuckDB target).
//!
//! The world imports the upstream shim's interfaces (the same
//! `postgis:wasm/*` / `mobilitydb:temporal/*` set the SQLite
//! target consumes) AND the `duckdb:extension@2.2.0` runtime
//! interfaces, and EXPORTS `duckdb:extension/guest` +
//! `duckdb:extension/callback-dispatch`. The vendored `deps/`
//! holds both the upstream shim WIT packages (sourced from the
//! same per-primary path as sqlite-emit) and the
//! `duckdb-extension` package (sourced from ducklink's
//! `wit/duckdb-extension/`).
//!
//! ## Source locations
//!
//! Per-primary upstream-shim WIT (same as sqlite-emit):
//!   * `postgis`     → `~/git/sqlink/extensions/postgis-bridge/wit/deps`
//!   * `mobilitydb`  → `~/git/mobilitydb-sqlink-bridge/wit/deps`
//!                 (or `~/git/mobilitydb-wasm/wit/deps`)
//!
//! DuckDB extension WIT:
//!   * `$DUCKLINK_EXTENSION_WIT=...`
//!   * `$HOME/git/ducklink/wit/duckdb-extension`
//!   * `../ducklink/wit/duckdb-extension`

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use shim_bridge_codegen_core::BridgePlan;
use datalink_shim_codegen_core::wit_parse::{self, WitPackage};

/// Write `wit/world.wit`.
pub fn write_world(plan: &BridgePlan, dest: &Path) -> Result<()> {
    let primary = primary_extension_name(plan);
    let shim_deps = source_shim_deps_dir(primary)?;
    let shim_packages = discover_shim_packages(&shim_deps)?;
    let duckdb_pkg = discover_duckdb_extension_package()?;
    let world = render_world(primary, &shim_packages, &duckdb_pkg);
    fs::write(dest, world).with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

/// Copy the dependency WIT tree into `wit/deps/`.
///
/// Every subdir of the source shim deps tree that holds a
/// well-formed package is copied as-is, EXCLUDING the SQLite
/// contract package (`sqlite-extension`) — the DuckDB target
/// imports `duckdb:extension`, not `sqlite:extension`. The
/// `duckdb-extension` package is always sourced from ducklink's
/// canonical `wit/duckdb-extension/` directly.
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
        if name == "sqlite-extension" {
            // SQLite contract isn't part of the DuckDB world.
            continue;
        }
        let to = deps_dir.join(&name);
        copy_tree(&from, &to)
            .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
    }
    let duckdb_wit = source_duckdb_extension_wit_dir()?;
    let duckdb_dst = deps_dir.join("duckdb-extension");
    copy_tree(&duckdb_wit, &duckdb_dst).with_context(|| {
        format!(
            "copying ducklink wit/duckdb-extension {} -> {}",
            duckdb_wit.display(),
            duckdb_dst.display()
        )
    })?;
    Ok(())
}

/// Locate the source `wit/deps/` directory for the upstream shim
/// WIT packages. Same lookup tree as sqlite-emit — the wasm
/// component imports are identical between the SQLite and DuckDB
/// targets; only the contract (sqlite:extension vs
/// duckdb:extension) differs at the export surface.
pub fn source_shim_deps_dir(primary: &str) -> Result<PathBuf> {
    if let Ok(p) = std::env::var("SQLINK_SHIM_WIT_DEPS") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Ok(p);
        }
        return Err(anyhow!(
            "SQLINK_SHIM_WIT_DEPS={} does not exist",
            p.display()
        ));
    }
    let env_per_primary = match primary {
        "postgis" => Some("SQLINK_POSTGIS_BRIDGE_WIT_DEPS"),
        "mobilitydb" => Some("SQLINK_MOBILITYDB_BRIDGE_WIT_DEPS"),
        _ => None,
    };
    if let Some(var) = env_per_primary {
        if let Ok(p) = std::env::var(var) {
            let p = PathBuf::from(p);
            if p.is_dir() {
                return Ok(p);
            }
            return Err(anyhow!("{}={} does not exist", var, p.display()));
        }
    }
    let candidates: Vec<PathBuf> = match primary {
        "postgis" => vec![
            home_path("git/sqlink/extensions/postgis-bridge/wit/deps"),
            Some(PathBuf::from("../sqlink/extensions/postgis-bridge/wit/deps")),
        ],
        "mobilitydb" => vec![
            home_path("git/mobilitydb-sqlink-bridge/wit/deps"),
            home_path("git/mobilitydb-wasm/wit/deps"),
            Some(PathBuf::from("../mobilitydb-wasm/wit/deps")),
        ],
        _ => vec![home_path(&format!(
            "git/{}-sqlink-bridge/wit/deps",
            primary
        ))],
    }
    .into_iter()
    .flatten()
    .collect();
    for c in &candidates {
        if c.is_dir() {
            return Ok(c.clone());
        }
    }
    Err(anyhow!(
        "cannot locate shim wit/deps for primary '{primary}'. Set \
         SQLINK_SHIM_WIT_DEPS=/path/to/wit/deps"
    ))
}

/// Locate ducklink's `wit/duckdb-extension/` directory.
///
/// Resolution order:
///   1. `$DUCKLINK_EXTENSION_WIT` (explicit override)
///   2. `$HOME/git/ducklink/wit/duckdb-extension`
///   3. `../ducklink/wit/duckdb-extension`
fn source_duckdb_extension_wit_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("DUCKLINK_EXTENSION_WIT") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Ok(p);
        }
        return Err(anyhow!(
            "DUCKLINK_EXTENSION_WIT={} does not exist",
            p.display()
        ));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join("git/ducklink/wit/duckdb-extension");
        if p.is_dir() {
            return Ok(p);
        }
    }
    let rel = PathBuf::from("../ducklink/wit/duckdb-extension");
    if rel.is_dir() {
        return Ok(rel);
    }
    Err(anyhow!(
        "cannot locate ducklink/wit/duckdb-extension. Set \
         DUCKLINK_EXTENSION_WIT=/path/to/ducklink/wit/duckdb-extension"
    ))
}

/// Walk the shim deps tree and parse every package subdir into a
/// `WitPackage`. The DuckDB-extension package is loaded separately.
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
        if name == "sqlite-extension" || name == "duckdb-extension" {
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

/// Parse ducklink's `duckdb-extension` package.
pub fn discover_duckdb_extension_package() -> Result<WitPackage> {
    let dir = source_duckdb_extension_wit_dir()?;
    let pkg = wit_parse::parse_package_dir(&dir)?.ok_or_else(|| {
        anyhow!(
            "duckdb-extension wit dir {} has no parseable package declaration",
            dir.display()
        )
    })?;
    Ok(pkg)
}

/// Render `world.wit` from the parsed packages.
///
/// Step 4 scalar-first cut: the world omits the bridge-side
/// `serde-ops` interface that the SQLite target emits — without
/// ducklink-loader wit-value lift, the bridge has no consumer for
/// per-record codecs. The export surface is the two
/// `duckdb:extension/` interfaces a DuckDB extension MUST export
/// (`guest` + `callback-dispatch`) and no more.
pub fn render_world(
    primary: &str,
    shim_packages: &[WitPackage],
    duckdb_pkg: &WitPackage,
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "package ducklink-bridge:{primary}@0.1.0;\n\n",
    ));
    s.push_str("/// Generated by sqlink-shim-codegen (target=duckdb).\n");
    s.push_str("/// Bridges the shim's WIT-exposed surface onto the\n");
    s.push_str("/// canonical `duckdb:extension@2.2.0` contract. Imports\n");
    s.push_str("/// are derived from the shim's vendored WIT packages +\n");
    s.push_str("/// the DuckDB runtime surface; exports are the fixed\n");
    s.push_str("/// (guest + callback-dispatch) pair every DuckDB\n");
    s.push_str("/// extension component declares.\n");
    s.push_str("world bridge {\n");

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
    s.push('\n');

    // DuckDB runtime imports.
    let duckdb_ns = &duckdb_pkg.ns_name;
    let duckdb_ver = &duckdb_pkg.version;
    for iface in DUCKDB_IMPORTS {
        s.push_str(&format!(
            "    import {duckdb_ns}/{iface}@{duckdb_ver};\n"
        ));
    }
    s.push('\n');

    // DuckDB contract exports.
    for iface in DUCKDB_EXPORTS {
        s.push_str(&format!(
            "    export {duckdb_ns}/{iface}@{duckdb_ver};\n"
        ));
    }
    s.push_str("}\n");
    s
}

/// DuckDB runtime interfaces the bridge imports. These are the
/// hand-written ducklink extension precedent set
/// (aba-component/wit/duckdb-extension.wit) minus the optional
/// arrow-ext / collation / index / storage / runtime-ext arms the
/// scalar-first cut doesn't exercise. A follow-up that wires
/// runtime-ext for `null-handling: special` (Called) functions
/// adds `runtime-ext` here.
pub const DUCKDB_IMPORTS: &[&str] = &[
    "runtime",
    "config",
    "logging",
    "catalog",
    "files",
];

/// DuckDB contract exports the host expects. Every extension
/// component declares both. `guest` is the lifecycle surface
/// (load / reconfigure / shutdown); `callback-dispatch` is the
/// six-arm call_* surface (call_scalar / call_scalar_batch /
/// call_table / call_aggregate / call_pragma / call_cast). Step 4
/// scalar-first cut wires `call_scalar` + `call_scalar_batch`;
/// the other four return `Duckerror::Unsupported`.
pub const DUCKDB_EXPORTS: &[&str] = &["guest", "callback-dispatch"];

pub(crate) fn primary_extension_name(plan: &BridgePlan) -> &str {
    plan.extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or("shim")
}

/// Returns true when `package` is the primary shim's own package
/// (mirrors the helper in sqlite-emit's emit_wit).
pub fn package_belongs_to_primary(package: &str, primary: &str) -> bool {
    package.split(':').next().map(|ns| ns == primary).unwrap_or(false)
}

fn home_path(rel: &str) -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rel))
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Err(anyhow!("source {} is not a directory", src.display()));
    }
    fs::create_dir_all(dst)?;
    // #642: when the upstream shim splits an umbrella `world.wit` into
    // per-interface .wit files, a stale `dst/world.wit` left over from
    // a previous regen still declares the same interfaces inline,
    // triggering a "duplicate item" parse error. Drop the stale file
    // before copying — if the source still owns a `world.wit`, the
    // loop below copies it right back; if not, it stays gone.
    let stale_world = dst.join("world.wit");
    if stale_world.exists() {
        fs::remove_file(&stale_world)
            .with_context(|| format!("removing stale {}", stale_world.display()))?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
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
                fs::copy(&resolved, &to)
                    .with_context(|| format!("copy {} -> {}", resolved.display(), to.display()))?;
            }
        } else if file_type.is_file() {
            if same_file(&from, &to) {
                continue;
            }
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}
