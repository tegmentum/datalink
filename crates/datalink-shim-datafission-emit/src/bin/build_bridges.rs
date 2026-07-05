//! Stable build recipe for Phase 3 (#823) dynlink bridges.
//!
//! Emits + compiles a `<sub_ext>-datafission-bridge-dynlink` crate for
//! each postgis sub-extension the loader ships a plan for, then
//! consolidates the produced .wasm components under a caller-supplied
//! output directory as `<sub_ext>-bridge.wasm`. The result is the
//! `sub_ext_bridge_paths` map's value set for
//! `PluginLoaderConfig` — plug it in and `CREATE EXTENSION
//! postgis_<sub_ext>` registers real SQL functions.
//!
//! ## Usage
//!
//! ```sh
//! cargo run --release --bin build-bridges -- \
//!     --interface-db ~/git/postgis-shim-interface/postgis-interface.sqlite \
//!     --out          ~/git/datafission/extensions/postgis-dynlink-bridges
//! ```
//!
//! Missing `--interface-db` defaults to
//! `$HOME/git/postgis-shim-interface/postgis-interface.sqlite`.
//! Missing `--out` defaults to
//! `$HOME/git/datafission/extensions/postgis-dynlink-bridges`.
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
//! ```
//!
//! The `build/` subtree is intermediate — feel free to `rm -rf` it after
//! a successful run. The four `.wasm` files at the top level are what
//! callers stash into `sub_ext_bridge_paths`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};

use datalink_shim_datafission_emit::{emit_dynlink, DynlinkOptions};

/// Sub-extensions the datafission postgis wire ships a plan for. The
/// name is the loader-side key (matches `PluginLoaderConfig::
/// sub_ext_plan_paths` and `sub_ext_bridge_paths` keys). The bridge's
/// `provider_id` is `<name>-composed` — mirrors the loader's
/// `sub_ext_provider_id`.
const SUB_EXTS: &[&str] = &[
    "postgis_core",
    "postgis_sfcgal",
    "postgis_raster",
    "postgis_format_encoders",
];

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut interface_db: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut only: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--interface-db" => {
                interface_db = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow!("--interface-db requires a value"))?,
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
    let interface_db = interface_db.unwrap_or_else(|| {
        PathBuf::from(&home).join("git/postgis-shim-interface/postgis-interface.sqlite")
    });
    let out_dir = out_dir.unwrap_or_else(|| {
        PathBuf::from(&home).join("git/datafission/extensions/postgis-dynlink-bridges")
    });

    if !interface_db.is_file() {
        return Err(anyhow!(
            "interface DB not found at {} — clone tegmentum/postgis-shim-interface next to this repo \
             or pass --interface-db",
            interface_db.display()
        ));
    }

    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("mkdir -p {}", out_dir.display()))?;
    let build_root = out_dir.join("build");
    std::fs::create_dir_all(&build_root)
        .with_context(|| format!("mkdir -p {}", build_root.display()))?;

    let plan = shim_bridge_codegen_core::load_plan(&interface_db)
        .with_context(|| format!("load postgis interface DB at {}", interface_db.display()))?;

    let sub_exts: Vec<&str> = if only.is_empty() {
        SUB_EXTS.to_vec()
    } else {
        for name in &only {
            if !SUB_EXTS.iter().any(|s| *s == name.as_str()) {
                return Err(anyhow!(
                    "--only {name} not in known set {:?}",
                    SUB_EXTS
                ));
            }
        }
        only.iter().map(String::as_str).collect()
    };

    let mut produced: Vec<(String, PathBuf, u64)> = Vec::new();
    for sub_ext in sub_exts {
        eprintln!("== building bridge: {sub_ext} ==");
        let bridge_wasm = build_one_bridge(&plan, &build_root, &out_dir, sub_ext)
            .with_context(|| format!("bridge for {sub_ext}"))?;
        let sz = std::fs::metadata(&bridge_wasm)?.len();
        eprintln!(
            "   done: {} ({:.1} KB)",
            bridge_wasm.display(),
            sz as f64 / 1024.0
        );
        produced.push((sub_ext.to_string(), bridge_wasm, sz));
    }

    eprintln!();
    eprintln!("== summary ==");
    for (name, path, sz) in &produced {
        eprintln!("  {name:32}  {sz:>9} bytes  {}", path.display());
    }
    Ok(())
}

fn print_usage() {
    eprintln!(
        "Usage: build-bridges [options]\n\
         \n\
         Options:\n\
         \x20 --interface-db <path>   Path to postgis-interface.sqlite\n\
         \x20                         (default: $HOME/git/postgis-shim-interface/postgis-interface.sqlite)\n\
         \x20 --out <dir>             Output directory for the bridge .wasm files\n\
         \x20                         (default: $HOME/git/datafission/extensions/postgis-dynlink-bridges)\n\
         \x20 --only <sub_ext>        Build only the named sub-ext (may be repeated).\n\
         \x20 --help                  Print this help.\n\
         \n\
         Known sub-extensions: {:?}",
        SUB_EXTS
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
