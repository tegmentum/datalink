//! Datafission-target emitter of the shim-bridge codegen.
//!
//! The datafission-targeted sibling of `datalink-shim-sqlite-emit`
//! and `datalink-shim-duckdb-emit`. Consumes the same
//! database-agnostic substrate
//! (`datalink-shim-codegen-core` + `shim-bridge-codegen-core`) and
//! emits a Rust crate compilable for `wasm32-wasip2` as a `cdylib`
//! whose component:
//!
//!   * imports the upstream shim's WIT interfaces (the same
//!     `postgis:wasm/*` / `mobilitydb:temporal/*` set the SQLite
//!     and DuckDB targets consume) so any bridge generated here
//!     composes against the same `<primary>-composed.wasm`
//!     artifacts;
//!   * `include`s the canonical composite world
//!     `datafission:extension/extension@1.0.0`, which pulls in
//!     identity, sql-extension-plugin/metadata, spatial-index-plugin,
//!     system-catalog-plugin, function-plugin (scalar / aggregate /
//!     window / table registries), type-plugin (custom-type +
//!     multi-custom-type), and index-plugin as exports.
//!
//! ## Scalar-first cut
//!
//! The first cut wires `scalar-function-registry` end-to-end. All
//! other capability interfaces are stubbed:
//!
//!   * `identity` returns the primary extension name + version.
//!   * `sql-extension-plugin/metadata` returns empty cast /
//!     operator / preprocessor lists.
//!   * `function-plugin/scalar-function-registry` — WIRED. Per-
//!     scalar arms dispatch by `name` (the registry pattern).
//!   * `function-plugin/aggregate-function-registry` — empty
//!     `list-functions`; per-call methods return
//!     `FunctionError::UnknownFunction`.
//!   * `function-plugin/window-function-registry` — empty
//!     `list-functions`; `compute-partition` returns
//!     `FunctionError::UnknownFunction`.
//!   * `function-plugin/table-function-registry` — empty
//!     `list-functions`; `output-schema` / `begin` return
//!     `FunctionError::UnknownFunction`.
//!   * `type-plugin/custom-type` + `multi-custom-type` — empty
//!     `list-types`; per-type ops return `TypeError::Internal`.
//!   * `spatial-index-plugin/spatial-index` — when the shim
//!     advertises `postgis:wasm/postgis-spatial-index`, wired as a
//!     pass-through to `pg_strtree::*` (GEOS STRtree:
//!     name=`"strtree"`, aliases `spatial`/`gist`/`rtree`, full
//!     envelope+KNN+within-distance capabilities). Bridges without
//!     that interface fall back to the original stub (advertises
//!     nothing; `build` returns `SpatialError::UnsupportedOperation`).
//!   * `system-catalog-plugin/system-catalog` — `catalog-name`
//!     returns the primary; `list-tables` returns empty.
//!   * `index-plugin/index` — honest no-op (#621). The BridgePlan
//!     substrate has no generic-index field today (only
//!     `spatial_indexes`, handled by the sibling
//!     `spatial-index-plugin` export) and no current upstream shim
//!     advertises a 1-D / R-tree / GIN / HNSW plugin, so the trait
//!     surface is wired structurally: `name` returns
//!     `<primary>-stub-index`, `type_id` returns 0, `capabilities`
//!     are all false, `supported_types` is empty, and per-op
//!     methods return an `IndexError::Internal` whose message names
//!     both the primary shim and the WIT method that was called
//!     (`"<primary> index-plugin: <method> not implemented ..."`)
//!     so a probing host sees exactly where the call landed.
//!
//! The verification gate at this step is "the generated bridge
//! crate COMPILES against the `datafission:extension@1.0.0`
//! contract" — runtime smoke against df-plugin-loader is deferred.
//!
//! ## Layout produced under `out_dir`:
//!
//! ```text
//! Cargo.toml
//! README.md
//! src/lib.rs
//! wit/world.wit
//! wit/deps/datafission-extension/...   (vendored from datafission)
//! wit/deps/datafission-function-plugin/...
//! wit/deps/datafission-sql-extension-plugin/...
//! wit/deps/datafission-type-plugin/...
//! wit/deps/datafission-spatial-index-plugin/...
//! wit/deps/datafission-system-catalog-plugin/...
//! wit/deps/datafission-index-plugin/...
//! wit/deps/postgis-wasm/...            (vendored from upstream shim)
//! wit/deps/sfcgal-component/...        (vendored from upstream shim)
//! ```

pub(crate) mod dispatch;
pub(crate) mod emit_cargo;
pub(crate) mod emit_lib;
pub(crate) mod emit_readme;
pub(crate) mod emit_wit;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use shim_bridge_codegen_core::BridgePlan;

/// Entry point invoked from `sqlink-shim-codegen`'s
/// `generate_with_target` (or a future `datafission-shim-codegen`).
/// Emits a complete bridge crate under `out_dir` for the
/// datafission (wasm-component) target.
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

    // compose.wac — #645 auto-emit, mirroring the sqlite target's
    // #563 emission. Emitted when the bridge needs explicit
    // `wac compose` plug → plug wiring; skipped for the postgis
    // case where postgis-composed.wasm packs the upstream
    // namespaces and plain `wac plug` is sufficient.
    let primary = emit_wit::primary_extension_name(plan).to_string();
    let shim_deps = emit_wit::source_shim_deps_dir(&primary)?;
    let shim_packages = emit_wit::discover_shim_packages(&shim_deps)?;
    let bridge_pkg_name = format!("datafission-bridge:{primary}");
    let has_compose_wac = datalink_shim_codegen_core::compose_emit::write_compose_wac(
        out_dir,
        &primary,
        &bridge_pkg_name,
        &shim_packages,
    )
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
/// becomes `postgis-datafission-bridge`; the `-datafission-`
/// segment disambiguates the wasm-component bridges targeting
/// datafission from the SQLite-targeted `postgis-sqlink-bridge`
/// and the DuckDB-targeted `postgis-ducklink-bridge`.
pub(crate) fn crate_name_for(plan: &BridgePlan) -> String {
    let primary = plan
        .extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or("shim");
    format!("{}-datafission-bridge", sanitize(primary))
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
