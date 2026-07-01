//! Source WIT-deps resolution shared across every emit target.
//!
//! Each per-target emit crate (`datalink-shim-sqlite-emit`,
//! `datalink-shim-datafission-emit`, `datalink-shim-duckdb-emit`)
//! has identical WIT-import surface needs at the upstream-shim
//! level — they all consume the same `postgis:wasm/*` /
//! `mobilitydb:temporal/*` set; only the contract export
//! (`sqlite:extension` / `duckdb:extension` /
//! `datafission:extension`) differs. The resolution logic for
//! locating that upstream tree was originally duplicated three
//! times; #654 lifts it here so the upstream-WIT synthesis
//! (#651) flows uniformly through every target.
//!
//! Resolution order (`source_shim_deps_dir`):
//!   1. `$SQLINK_SHIM_WIT_DEPS`                      (catch-all override)
//!   2. Per-primary env override
//!      (`SQLINK_{POSTGIS,MOBILITYDB}_BRIDGE_WIT_DEPS`)
//!   3. **Upstream WIT** — preferred default. Synthesizes a
//!      `wit/deps/`-shaped tree at
//!      `$TMPDIR/sqlink-codegen-upstream-<primary>/` from
//!      upstream sources whose layout doesn't match the flat
//!      `deps/<pkg>/` shape (e.g. `mobilitydb-wasm`'s
//!      `crates/mdb-temporal-wasm/wit/temporal.wit`).
//!   4. Bridge's own vendored `wit/deps/` (last-resort fallback,
//!      stale-by-definition during a regen).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Locate the source `wit/deps/` directory for the upstream shim
/// WIT packages. See module-level docs for resolution order.
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
    // (3) UPSTREAM (preferred) — synthesize a deps tree from upstream
    //     sources if the upstream repo is checked out at the expected
    //     paths. The bridge's own wit/deps/ is stale-by-definition
    //     during regen, so falling through to it silently drops any
    //     new functions added upstream (the #651 symptom).
    if let Some(p) = try_synthesize_upstream_deps(primary)? {
        return Ok(p);
    }
    // (4) Bridge's own vendored copy (last-resort fallback).
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

/// Synthesize a `wit/deps/`-shaped tree from upstream-shim sources.
/// Returns `None` when the upstream repo isn't checked out at the
/// expected paths.
///
/// Upstream layouts don't always match the flat `deps/<pkg>/` shape
/// the bridge expects:
///   - `mobilitydb`: the `mobilitydb:temporal` package lives at
///     `~/git/mobilitydb-wasm/crates/mdb-temporal-wasm/wit/temporal.wit`,
///     NOT at `~/git/mobilitydb-wasm/wit/deps/`. The helpers under
///     upstream `wit/deps/` are imported via wac plug at compose time
///     and aren't part of the bridge's `wit/deps/`.
///   - `postgis`: `~/git/postgis-wasm/wit/*.wit` holds the primary
///     `postgis:wasm` package as a multi-file dir; helper packages
///     `sfcgal:component`, `proj:wasm`, `mvt:vectortile`,
///     `flatgeobuf:format`, `kml:parser`, `geos:geometry`,
///     `geobuf:wasm`, `marc21:wasm`, `gml:parser`, `ttf:parser`,
///     `rustybuzz:shaper`, `geographiclib:geodesic`, `gdal:core`
///     are vendored at `~/git/postgis-wasm/wit/deps/<dir>/` (where
///     `<dir>` is the upstream repo's chosen name, not always
///     `<ns>-<name>`). #657 wires every helper into the synthesized
///     tree so the regenerated bridge's `wit/deps/` resolves all of
///     postgis-wasm's transitive imports.
///
/// The synthesized tree is rooted at
/// `$TMPDIR/sqlink-codegen-upstream-<primary>/` and is repopulated
/// from scratch on every call so the bridge always picks up
/// the latest upstream WIT.
pub fn try_synthesize_upstream_deps(primary: &str) -> Result<Option<PathBuf>> {
    let sources = upstream_pkg_sources(primary)?;
    if sources.is_empty() {
        return Ok(None);
    }
    let dest = std::env::temp_dir().join(format!("sqlink-codegen-upstream-{primary}"));
    if dest.exists() {
        fs::remove_dir_all(&dest).with_context(|| format!("clearing {}", dest.display()))?;
    }
    fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    for (subdir, src) in &sources {
        let to = dest.join(subdir);
        copy_top_level_wit_files(src, &to).with_context(|| {
            format!(
                "synthesizing upstream {} -> {}",
                src.display(),
                to.display()
            )
        })?;
    }
    Ok(Some(dest))
}

/// Postgis helper packages vendored under `~/git/postgis-wasm/wit/deps/`.
/// Each entry is `(deps_subdir_in_synthesized_tree, upstream_subdir_under_postgis_wit_deps)`.
///
/// The upstream subdir name often doesn't match the package's
/// `<ns>-<name>` form (e.g. `proj-wasm/` holds `proj:wasm`,
/// `mvt-wasm/` holds `mvt:vectortile`). The synthesized tree uses
/// the canonical `<ns>-<name>` naming to align with the bridge's
/// own `wit/deps/` convention.
const POSTGIS_HELPER_PKGS: &[(&str, &str)] = &[
    ("sfcgal-component", "sfcgal-wasm"),
    ("proj-wasm", "proj-wasm"),
    ("mvt-vectortile", "mvt-wasm"),
    ("flatgeobuf-format", "flatgeobuf-wasm"),
    ("kml-parser", "kml-wasm"),
    ("geos-geometry", "geos-wasm"),
    ("geobuf-wasm", "geobuf-wasm"),
    ("marc21-wasm", "marc21-wasm"),
    ("gml-parser", "gml-wasm"),
    ("ttf-parser", "ttf-parser-wasm"),
    ("rustybuzz-shaper", "rustybuzz-wasm"),
    ("geographiclib-geodesic", "geographiclib-wasm"),
    ("gdal-core", "gdal-wasm"),
    // Transitively required by `flatgeobuf-format/world.wit` (it
    // imports `geozero:convert/geozero-api`). Not imported by
    // postgis-wasm's top-level world directly, but the vendored
    // flatgeobuf world.wit pulls it in during WIT-deps parsing.
    ("geozero-convert", "geozero"),
];

/// Upstream-shim package sources. Each entry is
/// `(deps_subdir_name, source_dir_with_*.wit_files)`. Empty when the
/// upstream repo isn't checked out.
///
/// Returns an error (Option A from #657) when the primary upstream
/// repo IS checked out but a known-required helper package is
/// missing from its vendored `wit/deps/` — silent skipping would
/// surface later as a confusing "package <pkg> not found" during
/// `cargo build --target wasm32-wasip2` of the regenerated bridge.
pub fn upstream_pkg_sources(primary: &str) -> Result<Vec<(&'static str, PathBuf)>> {
    let mut out = Vec::<(&'static str, PathBuf)>::new();
    match primary {
        "mobilitydb" => {
            if let Some(p) = home_path("git/mobilitydb-wasm/crates/mdb-temporal-wasm/wit") {
                if p.is_dir() {
                    out.push(("mobilitydb-temporal", p));
                }
            }
        }
        "postgis" => {
            if let Some(p) = home_path("git/postgis-wasm/wit") {
                if p.is_dir() {
                    out.push(("postgis-wasm", p.clone()));
                    let deps_root = p.join("deps");
                    let mut missing = Vec::<String>::new();
                    for (dest_sub, src_sub) in POSTGIS_HELPER_PKGS {
                        let src = deps_root.join(src_sub);
                        if src.is_dir() {
                            out.push((dest_sub, src));
                        } else {
                            missing.push(format!(
                                "{} (expected at {})",
                                dest_sub,
                                src.display()
                            ));
                        }
                    }
                    if !missing.is_empty() {
                        return Err(anyhow!(
                            "postgis-wasm checkout at {} is missing vendored \
                             WIT helper package(s) required by postgis:wasm's \
                             imports: {}. Update the postgis-wasm checkout or \
                             extend POSTGIS_HELPER_PKGS in \
                             datalink-shim-codegen-core::wit_paths.",
                            p.display(),
                            missing.join(", ")
                        ));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Copy only top-level `*.wit` files from `src` to `dst`, ignoring
/// any subdirectories. Used when synthesizing the upstream deps tree
/// — e.g. `~/git/postgis-wasm/wit/` has a nested `deps/` subdir we
/// don't want to flatten into the bridge's `wit/deps/<pkg>/`.
///
/// #766: applies `kebab_fix_wit` to every copied `.wit` file so the
/// synthesized tree ALREADY carries the fixed identifier forms
/// (`cg-threed-alpha-wrapping` rather than `cg-3d-alpha-wrapping`).
/// The per-target `emit_wit::copy_tree` then copies the fixed text
/// verbatim into the bridge's `wit/deps/`, AND — the mop-up point
/// #766 exists to close — `wit_parse::parse_dir` reads the fixed
/// kebab into every `WitFunction.kebab_name`. Downstream
/// `classify_shape`/`classify_aggregate_shape`/`classify_udtf_shape`/
/// `classify_window_shape` sites derive `wit_func` via
/// `kebab_to_snake(&f.kebab_name)`, so they now emit
/// `cg_threed_alpha_wrapping` (matching the wit-bindgen-generated
/// Rust binding on the fixed WIT) instead of `cg_3d_alpha_wrapping`
/// (which resolves to nothing after the WIT-text rewrite lands).
pub fn copy_top_level_wit_files(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Err(anyhow!("source {} is not a directory", src.display()));
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() {
            continue;
        }
        if from.extension().and_then(|s| s.to_str()) != Some("wit") {
            continue;
        }
        let to = dst.join(entry.file_name());
        // #766: read + kebab_fix + write instead of raw `fs::copy` so
        // the synthesized tree holds fixed identifiers before
        // `parse_dir` populates `WitFunction.kebab_name`. See the
        // fn-level doc-comment for why the mop-up is needed here.
        let text = fs::read_to_string(&from)
            .with_context(|| format!("read {}", from.display()))?;
        let fixed = crate::kebab_fix::kebab_fix_wit(&text);
        fs::write(&to, fixed)
            .with_context(|| format!("write kebab-fixed {}", to.display()))?;
    }
    Ok(())
}

fn home_path(rel: &str) -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rel))
}

#[cfg(test)]
mod tests {
    //! #766 regression coverage: `copy_top_level_wit_files` must
    //! apply `kebab_fix_wit` to `.wit` sources so the synthesized
    //! upstream deps tree carries the fixed identifier forms.
    //! Without the mop-up, `wit_parse::parse_dir` populates
    //! `WitFunction.kebab_name` from the RAW upstream text
    //! (`cg-3d-alpha-wrapping`), and dispatch-emit derives Rust
    //! call names via `kebab_to_snake` producing
    //! `cg_3d_alpha_wrapping` — which then fails to resolve
    //! against the kebab-fixed WIT the bridge compiles against
    //! (which advertises `cg_threed_alpha_wrapping`).

    use super::*;
    use crate::wit_parse;

    /// Build a small WIT tree with a mid-position `-3d-` identifier
    /// (real upstream pattern from sfcgal-wasm), synthesize it via
    /// `copy_top_level_wit_files`, parse the synthesized tree, and
    /// confirm `WitFunction.kebab_name` carries the fixed
    /// (`cg-threed-alpha-wrapping`) form.
    #[test]
    fn copy_top_level_wit_files_applies_kebab_fix() {
        let tmp = std::env::temp_dir()
            .join("sqlink-codegen-test-wit-paths-kebab-fix");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let src = tmp.join("src");
        let dst = tmp.join("dst");
        fs::create_dir_all(&src).unwrap();
        // Minimal WIT package with one interface + one function whose
        // kebab has `-3d-` mid-position. The parser needs a package
        // decl + an interface block to yield a WitFunction.
        let wit = "\
package example:pkg@0.1.0;

interface example-iface {
    cg-3d-alpha-wrapping: func(wkb: list<u8>) -> list<u8>;
}
";
        fs::write(src.join("iface.wit"), wit).unwrap();
        copy_top_level_wit_files(&src, &dst).expect("copy_top_level_wit_files");

        // Source text arrived kebab-fixed at the destination.
        let fixed = fs::read_to_string(dst.join("iface.wit")).unwrap();
        assert!(
            fixed.contains("cg-threed-alpha-wrapping"),
            "expected fixed identifier in synthesized text; got:\n{fixed}",
        );
        assert!(
            !fixed.contains("cg-3d-alpha-wrapping"),
            "raw upstream identifier should have been rewritten; got:\n{fixed}",
        );

        // The parser sees the fixed kebab. This is what makes the
        // downstream `kebab_to_snake` emit `cg_threed_alpha_wrapping`
        // in dispatch code, matching the wit-bindgen binding.
        let fns = wit_parse::parse_dir(&dst).expect("parse_dir");
        assert!(
            fns.iter().any(|f| f.kebab_name == "cg-threed-alpha-wrapping"),
            "expected `cg-threed-alpha-wrapping` in parsed fns, got: {:?}",
            fns.iter().map(|f| &f.kebab_name).collect::<Vec<_>>(),
        );
        assert!(
            !fns.iter().any(|f| f.kebab_name == "cg-3d-alpha-wrapping"),
            "raw upstream kebab should not appear after fix",
        );

        // And the snake form the dispatch-emit sites derive at
        // 1763 / 1864 / 2112 / 3706 in interface_db.rs — the actual
        // #766 mop-up target.
        let snake: Vec<String> = fns
            .iter()
            .map(|f| wit_parse::kebab_to_snake(&f.kebab_name))
            .collect();
        assert!(
            snake.iter().any(|s| s == "cg_threed_alpha_wrapping"),
            "expected fixed snake form; got: {snake:?}",
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    /// A `.wit` file with no `-Nd` / bare-digit segments passes through
    /// byte-for-byte (kebab_fix is a no-op on unchanged input).
    #[test]
    fn copy_top_level_wit_files_no_op_when_nothing_matches() {
        let tmp = std::env::temp_dir()
            .join("sqlink-codegen-test-wit-paths-noop");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let src = tmp.join("src");
        let dst = tmp.join("dst");
        fs::create_dir_all(&src).unwrap();
        let wit = "\
package example:pkg@0.1.0;

interface example-iface {
    plain-func: func(wkb: list<u8>) -> list<u8>;
}
";
        fs::write(src.join("iface.wit"), wit).unwrap();
        copy_top_level_wit_files(&src, &dst).unwrap();
        let out = fs::read_to_string(dst.join("iface.wit")).unwrap();
        assert_eq!(out, wit, "no-op kebab_fix must preserve source byte-for-byte");
        let _ = fs::remove_dir_all(&tmp);
    }
}
