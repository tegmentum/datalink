//! Stable build recipe for Phase 3 (#823) dynlink bridges.
//!
//! Emits + compiles a `<sub_ext>-datafission-bridge-dynlink` crate for
//! each postgis sub-extension the loader ships a plan for, plus the
//! mobilitydb single-provider bridge (per Agent #887's recon:
//! mobilitydb-composed is 8.8 MB, no heavyweight native deps, so it
//! stays a single monolithic `sub_ext = "mobilitydb"` target with no
//! `_core / _sfcgal / _raster / _format_encoders` split). Consolidates
//! the produced .wasm components under a caller-supplied output
//! directory as `<sub_ext>-bridge.wasm`. The result is the
//! `sub_ext_bridge_paths` map's value set for `PluginLoaderConfig` —
//! plug it in and `CREATE EXTENSION postgis_<sub_ext>` /
//! `CREATE EXTENSION mobilitydb` registers real SQL functions.
//!
//! ## Usage
//!
//! ```sh
//! cargo run --release --bin build-bridges -- \
//!     --postgis-interface-db   ~/git/postgis-shim-interface/postgis-interface.sqlite \
//!     --mobilitydb-interface-db ~/git/mobilitydb-shim-interface/mobilitydb-interface.sqlite \
//!     --out                    ~/git/datafission/extensions/dynlink-bridges
//! ```
//!
//! `--interface-db <path>` is accepted as a back-compat alias for
//! `--postgis-interface-db`. Missing paths default to the sibling repo
//! layout (`$HOME/git/<repo>/…-interface.sqlite`). Missing `--out`
//! defaults to `$HOME/git/datafission/extensions/dynlink-bridges`.
//!
//! When the mobilitydb interface DB is present but empty (an extractor
//! run against a stale composed .wasm produces an empty extensions
//! table — see `~/git/mobilitydb-shim-interface/README.md`), the
//! mobilitydb target still emits a bridge crate with 0 dispatch arms;
//! that's a valid substrate proof even when the classifier can't populate
//! it with real arms. Add `--only mobilitydb` to skip postgis and just
//! validate the mobilitydb code path.
//!
//! ## Emitted layout
//!
//! ```text
//! <out>/
//!   build/<sub_ext>/         # generated bridge crate + wasm target dir
//!   postgis_core-bridge.wasm
//!   postgis_sfcgal-bridge.wasm
//!   postgis_raster-bridge.wasm
//!   postgis_format_encoders-bridge.wasm
//!   mobilitydb-bridge.wasm
//! ```
//!
//! The `build/` subtree is intermediate — feel free to `rm -rf` it
//! after a successful run. The five `.wasm` files at the top level are
//! what callers stash into `sub_ext_bridge_paths`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};

use datalink_shim_datafission_emit::{emit_dynlink, DynlinkOptions};

/// Which interface DB a bridge target reads its classifier surface from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterfaceDbKey {
    Postgis,
    Mobilitydb,
}

/// A single bridge to emit. The `provider_id` handed to `emit_dynlink`
/// is `<sub_ext>-composed` — mirrors the loader's
/// `sub_ext_provider_id`. `interface_db` picks which shim-interface
/// SQLite the classifier reads dispatch entries from.
#[derive(Debug, Clone, Copy)]
struct BridgeTarget {
    sub_ext: &'static str,
    interface_db: InterfaceDbKey,
}

/// Bridge targets the recipe supports. Postgis contributes 4 sub-exts
/// via one interface DB; mobilitydb contributes 1 monolithic target
/// via its own interface DB.
const TARGETS: &[BridgeTarget] = &[
    BridgeTarget {
        sub_ext: "postgis_core",
        interface_db: InterfaceDbKey::Postgis,
    },
    BridgeTarget {
        sub_ext: "postgis_sfcgal",
        interface_db: InterfaceDbKey::Postgis,
    },
    BridgeTarget {
        sub_ext: "postgis_raster",
        interface_db: InterfaceDbKey::Postgis,
    },
    BridgeTarget {
        sub_ext: "postgis_format_encoders",
        interface_db: InterfaceDbKey::Postgis,
    },
    BridgeTarget {
        sub_ext: "mobilitydb",
        interface_db: InterfaceDbKey::Mobilitydb,
    },
];

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut postgis_db: Option<PathBuf> = None;
    let mut mobilitydb_db: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut only: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            // Postgis interface DB. `--interface-db` kept as a
            // back-compat alias for the pre-mobilitydb single-DB shape.
            "--postgis-interface-db" | "--interface-db" => {
                postgis_db = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow!("{arg} requires a value"))?,
                ));
            }
            "--mobilitydb-interface-db" => {
                mobilitydb_db = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow!("--mobilitydb-interface-db requires a value"))?,
                ));
            }
            "--out" => {
                out_dir = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow!("--out requires a value"))?,
                ));
            }
            "--only" => {
                only.push(
                    args.next()
                        .ok_or_else(|| anyhow!("--only requires a value"))?,
                );
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            other => return Err(anyhow!("unknown arg: {other}. Try --help.")),
        }
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let postgis_db = postgis_db.unwrap_or_else(|| {
        PathBuf::from(&home).join("git/postgis-shim-interface/postgis-interface.sqlite")
    });
    let mobilitydb_db = mobilitydb_db.unwrap_or_else(|| {
        PathBuf::from(&home).join("git/mobilitydb-shim-interface/mobilitydb-interface.sqlite")
    });
    let out_dir = out_dir.unwrap_or_else(|| {
        PathBuf::from(&home).join("git/datafission/extensions/dynlink-bridges")
    });

    let selected: Vec<&BridgeTarget> = if only.is_empty() {
        TARGETS.iter().collect()
    } else {
        for name in &only {
            if !TARGETS.iter().any(|t| t.sub_ext == name.as_str()) {
                return Err(anyhow!(
                    "--only {name} not in known set {:?}",
                    TARGETS.iter().map(|t| t.sub_ext).collect::<Vec<_>>()
                ));
            }
        }
        only.iter()
            .map(|n| TARGETS.iter().find(|t| t.sub_ext == n.as_str()).unwrap())
            .collect()
    };

    // Only demand + load an interface DB for the DB flavors this run
    // actually touches. `--only mobilitydb` on a machine without the
    // postgis-shim-interface repo shouldn't fail on a missing postgis
    // DB it never opens.
    let needs_postgis = selected
        .iter()
        .any(|t| t.interface_db == InterfaceDbKey::Postgis);
    let needs_mobilitydb = selected
        .iter()
        .any(|t| t.interface_db == InterfaceDbKey::Mobilitydb);

    let postgis_plan = if needs_postgis {
        if !postgis_db.is_file() {
            return Err(anyhow!(
                "postgis interface DB not found at {} — clone \
                 tegmentum/postgis-shim-interface next to this repo \
                 or pass --postgis-interface-db",
                postgis_db.display()
            ));
        }
        Some(
            shim_bridge_codegen_core::load_plan(&postgis_db)
                .with_context(|| format!("load postgis interface DB at {}", postgis_db.display()))?,
        )
    } else {
        None
    };

    let mobilitydb_plan = if needs_mobilitydb {
        if !mobilitydb_db.is_file() {
            return Err(anyhow!(
                "mobilitydb interface DB not found at {} — clone \
                 tegmentum/mobilitydb-shim-interface next to this repo \
                 or pass --mobilitydb-interface-db",
                mobilitydb_db.display()
            ));
        }
        let plan = shim_bridge_codegen_core::load_plan(&mobilitydb_db).with_context(|| {
            format!("load mobilitydb interface DB at {}", mobilitydb_db.display())
        })?;
        if plan.extensions.is_empty() {
            eprintln!(
                "warning: mobilitydb interface DB at {} is empty (0 extensions). \
                 The bridge will be emitted with 0 dispatch arms — the substrate \
                 compiles clean but the SQL surface will be trivial. Populate the \
                 DB via `extract-mobilitydb-interface --wasm <mobilitydb-composed.wasm>`.",
                mobilitydb_db.display()
            );
        }
        Some(plan)
    } else {
        None
    };

    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("mkdir -p {}", out_dir.display()))?;
    let build_root = out_dir.join("build");
    std::fs::create_dir_all(&build_root)
        .with_context(|| format!("mkdir -p {}", build_root.display()))?;

    let mut produced: Vec<(String, PathBuf, u64)> = Vec::new();
    for target in &selected {
        eprintln!("== building bridge: {} ==", target.sub_ext);
        let plan = match target.interface_db {
            InterfaceDbKey::Postgis => postgis_plan.as_ref().expect("postgis plan required"),
            InterfaceDbKey::Mobilitydb => {
                mobilitydb_plan.as_ref().expect("mobilitydb plan required")
            }
        };
        let bridge_wasm = build_one_bridge(plan, &build_root, &out_dir, target.sub_ext)
            .with_context(|| format!("bridge for {}", target.sub_ext))?;
        let sz = std::fs::metadata(&bridge_wasm)?.len();
        eprintln!(
            "   done: {} ({:.1} KB)",
            bridge_wasm.display(),
            sz as f64 / 1024.0
        );
        produced.push((target.sub_ext.to_string(), bridge_wasm, sz));
    }

    eprintln!();
    eprintln!("== summary ==");
    for (name, path, sz) in &produced {
        eprintln!("  {name:32}  {sz:>9} bytes  {}", path.display());
    }
    Ok(())
}

fn print_usage() {
    let names: Vec<&str> = TARGETS.iter().map(|t| t.sub_ext).collect();
    eprintln!(
        "Usage: build-bridges [options]\n\
         \n\
         Options:\n\
         \x20 --postgis-interface-db <path>     Path to postgis-interface.sqlite\n\
         \x20                                   (default: $HOME/git/postgis-shim-interface/postgis-interface.sqlite)\n\
         \x20 --interface-db <path>             Back-compat alias for --postgis-interface-db\n\
         \x20 --mobilitydb-interface-db <path>  Path to mobilitydb-interface.sqlite\n\
         \x20                                   (default: $HOME/git/mobilitydb-shim-interface/mobilitydb-interface.sqlite)\n\
         \x20 --out <dir>                       Output directory for the bridge .wasm files\n\
         \x20                                   (default: $HOME/git/datafission/extensions/dynlink-bridges)\n\
         \x20 --only <sub_ext>                  Build only the named sub-ext (may be repeated).\n\
         \x20 --help                            Print this help.\n\
         \n\
         Known sub-extensions: {names:?}"
    );
}

/// Emit + compile one bridge. Places the final .wasm at
/// `<out>/<sub_ext>-bridge.wasm` and returns its path.
fn build_one_bridge(
    plan: &shim_bridge_codegen_core::BridgePlan,
    build_root: &Path,
    out_dir: &Path,
    sub_ext: &str,
) -> Result<PathBuf> {
    // Emit the bridge crate. `provider_id` mirrors the loader's
    // `sub_ext_provider_id` — `<sub_ext>-composed`, underscores
    // preserved so `resolve_by_id` from the bridge matches whatever
    // the loader registered under.
    let crate_dir = build_root.join(sub_ext);
    let _ = std::fs::remove_dir_all(&crate_dir);
    std::fs::create_dir_all(&crate_dir)?;

    let opts = DynlinkOptions {
        provider_id: format!("{sub_ext}-composed"),
        sub_ext: sub_ext.to_string(),
    };
    emit_dynlink(plan, &crate_dir, &opts)
        .with_context(|| format!("emit_dynlink({sub_ext}, {})", crate_dir.display()))?;

    // Compile. Delegates to the generated crate's own Cargo.toml —
    // it declares its own `[workspace]` root so the top-level workspace
    // (if any) doesn't try to compose it.
    let target = "wasm32-wasip2";
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", target])
        .current_dir(&crate_dir)
        .status()
        .with_context(|| format!("spawn cargo for {sub_ext}"))?;
    if !status.success() {
        return Err(anyhow!(
            "cargo build failed for {sub_ext} — inspect {}",
            crate_dir.display()
        ));
    }

    // The crate name emitted by `crate_name_for` is
    // `<sub_ext-sanitized>-datafission-bridge-dynlink`; cargo lowers
    // `-` in a crate name to `_` for the wasm artifact filename.
    // Since `sub_ext` is already snake_case (underscores) the sanitizer
    // leaves it alone; only the `-datafission-bridge-dynlink` suffix
    // gets underscored.
    let wasm_name = format!("{}_datafission_bridge_dynlink.wasm", sub_ext);
    let wasm_src = crate_dir
        .join("target")
        .join(target)
        .join("release")
        .join(&wasm_name);
    if !wasm_src.is_file() {
        return Err(anyhow!(
            "compiled wasm missing at expected path {} — build likely produced a differently-named artifact",
            wasm_src.display()
        ));
    }

    let wasm_dst = out_dir.join(format!("{sub_ext}-bridge.wasm"));
    std::fs::copy(&wasm_src, &wasm_dst)
        .with_context(|| format!("copy {} -> {}", wasm_src.display(), wasm_dst.display()))?;
    Ok(wasm_dst)
}

// The `format!("{sub_ext}-composed")` above duplicates the logic
// `datafission_df_plugin_loader::sub_ext_provider_id` uses so this
// binary doesn't need to pull the datafission crate into datalink's
// dep tree.
