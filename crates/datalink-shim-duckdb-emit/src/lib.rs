//! DuckDB-target emitter of the shim-bridge codegen.
//!
//! Step 4 of PLAN-shim-codegen-datalink-migration creates this
//! crate as the DuckDB-targeted sibling of
//! `datalink-shim-sqlite-emit`. It consumes the same
//! database-agnostic substrate
//! (`datalink-shim-codegen-core` + `shim-bridge-codegen-core`)
//! and emits a Rust crate compilable for `wasm32-wasip2` as a
//! `cdylib` whose component:
//!
//!   * imports the upstream shim's WIT interfaces (the same
//!     ones the sqlink wasm-component target consumes — postgis
//!     scalars / mobilitydb temporal / sfcgal / etc.) so any
//!     bridge generated here composes against the same
//!     `<primary>-composed.wasm` artifacts;
//!   * imports the `duckdb:extension@2.2.0` runtime + types
//!     interfaces (`runtime`, `config`, `logging`, `catalog`,
//!     `files`);
//!   * exports `duckdb:extension/guest` (the lifecycle
//!     load / reconfigure / shutdown surface) and
//!     `duckdb:extension/callback-dispatch` (the six call-* arms
//!     a DuckDB extension's bridge dispatches on).
//!
//! ## Scalar-first cut (Step 4 scope)
//!
//! The first cut wires SCALARS end-to-end. `call_table`,
//! `call_aggregate`, `call_pragma`, `call_cast` stub out as
//! `Duckerror::Unsupported`. The verification gate for this
//! step is "the generated bridge crate COMPILES against the
//! duckdb:extension contract" — full smoke-tests against a
//! ducklink runtime are deferred until ducklink-loader gains
//! the equivalent of sqlink-loader's #559 wit-value lift.
//!
//! ## Layout produced under `out_dir`:
//!
//! ```text
//! Cargo.toml
//! README.md
//! src/lib.rs
//! wit/world.wit
//! wit/deps/duckdb-extension/...   (vendored from ducklink)
//! wit/deps/postgis-wasm/...       (vendored from upstream shim)
//! wit/deps/sfcgal-component/...   (vendored from upstream shim)
//! ```

pub(crate) mod dispatch;
pub(crate) mod emit_cargo;
pub(crate) mod emit_lib;
pub(crate) mod emit_readme;
pub(crate) mod emit_wit;
pub(crate) mod handle_table;
pub(crate) mod lifecycle;
pub(crate) mod register;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use shim_bridge_codegen_core::BridgePlan;

/// Entry point invoked from `sqlink-shim-codegen`'s
/// `generate_with_target` (or a future `ducklink-shim-codegen`).
/// Emits a complete bridge crate under `out_dir` for the DuckDB
/// (wasm-component) target.
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

    // README.md
    fs::write(
        out_dir.join("README.md"),
        emit_readme::readme(plan, &crate_name),
    )?;

    // rustfmt the emitted Rust source. Best-effort.
    let to_fmt: Vec<PathBuf> = vec![lib_rs_path];
    rustfmt_files(&to_fmt);

    Ok(())
}

/// Compose the crate name from the primary extension. PostGIS
/// becomes `postgis-ducklink-bridge`; the `-ducklink-` segment
/// disambiguates the wasm-component bridges targeting DuckDB
/// from the SQLite-targeted `postgis-sqlink-bridge`.
pub(crate) fn crate_name_for(plan: &BridgePlan) -> String {
    let primary = plan
        .extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or("shim");
    format!("{}-ducklink-bridge", sanitize(primary))
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
