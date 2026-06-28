//! SQLite-target emitter of the shim-bridge codegen.
//!
//! Step 3 of PLAN-shim-codegen-datalink-migration lifts the
//! `emit_sqlite/` subtree of `sqlink-shim-codegen` into this crate
//! so the database-agnostic substrate (`datalink-shim-codegen-core`)
//! and the per-database emitter are clearly separated. The CLI in
//! `sqlink-shim-codegen` becomes a thin wrapper around this crate.
//!
//! Produces a Rust crate compilable for `wasm32-wasip2` as a
//! `cdylib`. The component imports the upstream shim's WIT
//! interfaces (the same ones the hand-written
//! `extensions/postgis-bridge` in sqlink consumes) and exports the
//! canonical `sqlite:extension/minimal`-shape contract (metadata +
//! scalar-function + aggregate-function + vtab).
//!
//! Layout produced under `out_dir`:
//!
//! ```text
//! Cargo.toml
//! README.md
//! src/lib.rs
//! wit/world.wit
//! wit/deps/postgis-wasm/...        (vendored from postgis-bridge)
//! wit/deps/sfcgal-component/...    (vendored from postgis-bridge)
//! wit/deps/sqlite-extension/...    (vendored from postgis-bridge)
//! ```
//!
//! The WIT `deps/` are vendored at codegen time from the
//! hand-written postgis-bridge crate — the source of truth for the
//! import surface. Phase 4+ moves to fetching WIT from the
//! interface DB or from upstream tags.

pub(crate) mod dispatch;
pub(crate) mod emit_cargo;
pub(crate) mod emit_lib;
pub(crate) mod emit_readme;
pub(crate) mod emit_wit;
pub(crate) mod vtab;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use shim_bridge_codegen_core::BridgePlan;

/// Entry point invoked from `sqlink-shim-codegen`'s
/// `generate_with_target`. Emits a complete bridge crate under
/// `out_dir` for the SQLite (wasm-component) target.
pub fn emit(plan: &BridgePlan, out_dir: &Path) -> Result<()> {
    let crate_name = crate_name_for(plan);

    fs::create_dir_all(out_dir.join("src"))?;
    fs::create_dir_all(out_dir.join("wit"))?;
    fs::create_dir_all(out_dir.join("wit/deps"))?;

    // Cargo.toml
    fs::write(out_dir.join("Cargo.toml"), emit_cargo::cargo_toml(plan, &crate_name))?;

    // WIT (world + vendored deps).
    emit_wit::write_world(plan, &out_dir.join("wit/world.wit"))?;
    emit_wit::write_deps(plan, &out_dir.join("wit/deps"))
        .context("emitting wit/deps/")?;

    // src/lib.rs
    let lib_rs_path = out_dir.join("src/lib.rs");
    fs::write(&lib_rs_path, emit_lib::lib_rs(plan, &crate_name)?)?;

    // compose.wac — #563 auto-emit. Skipped when no stub-plug
    // is present (postgis bridge today: plug → socket alone
    // satisfies the composition via `wac plug`).
    let primary = emit_wit::primary_extension_name(plan).to_string();
    let shim_deps = emit_wit::source_shim_deps_dir(&primary)?;
    let shim_packages = emit_wit::discover_shim_packages(&shim_deps)?;
    let has_compose_wac = datalink_shim_codegen_core::compose_emit::write_compose_wac(out_dir, &primary, &shim_packages)
        .context("emitting compose.wac")?;

    // README.md
    fs::write(
        out_dir.join("README.md"),
        emit_readme::readme(plan, &crate_name, has_compose_wac),
    )?;

    // rustfmt the emitted Rust source. Best-effort.
    let to_fmt: Vec<PathBuf> = vec![lib_rs_path];
    rustfmt_files(&to_fmt);

    Ok(())
}

/// Compose the crate name from the primary extension. PostGIS
/// becomes `postgis-sqlink-bridge`; the `-sqlink-` segment
/// disambiguates the wasm-component bridges from the existing
/// native-dylib `postgis-sqlite-bridge` crate.
pub(crate) fn crate_name_for(plan: &BridgePlan) -> String {
    let primary = plan
        .extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or("shim");
    format!("{}-sqlink-bridge", sanitize(primary))
}

pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Run `rustfmt --edition 2021` against each file. Best-effort: a
/// missing or failing rustfmt logs to stderr and continues, so the
/// codegen still produces output usable as-is.
pub(crate) fn rustfmt_files(paths: &[PathBuf]) {
    for path in paths {
        let status = Command::new("rustfmt")
            .arg("--edition")
            .arg("2021")
            .arg(path)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("[codegen] rustfmt {} exited with {s}", path.display()),
            Err(e) => {
                eprintln!("[codegen] rustfmt invocation failed for {}: {e}", path.display());
            }
        }
    }
}
