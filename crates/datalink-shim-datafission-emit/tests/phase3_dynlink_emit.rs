//! Phase 3 Commit 6: emit + compile a dynlink-mode datafission bridge.
//!
//! Loads the postgis interface DB, runs `emit_dynlink` against it
//! targeting `postgis-sfcgal`, then invokes `cargo build --release
//! --target wasm32-wasip2` on the generated crate. Asserts:
//!
//! 1. The generated crate compiles cleanly.
//! 2. The output wasm is small (< 5 MB — dynlink mode's whole point).
//!
//! Skips gracefully when the postgis interface DB isn't on disk
//! (fresh checkout without the sibling repos populated).

use std::path::PathBuf;
use std::process::Command;

use datalink_shim_datafission_emit::{emit_dynlink, DynlinkOptions};
use shim_bridge_codegen_core::load_plan;

fn postgis_interface_db() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("git/postgis-shim-interface/postgis-interface.sqlite")
}

#[test]
fn dynlink_emit_produces_compilable_small_bridge() {
    let db = postgis_interface_db();
    if !db.exists() {
        eprintln!("skipping dynlink_emit: {} not found", db.display());
        return;
    }

    let plan = match load_plan(&db) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipping dynlink_emit: load_plan failed: {e}");
            return;
        }
    };

    // Emit to a stable location so a maintainer can iterate on
    // compilation without regenerating each time. Overridable via
    // env var for tempfile-based CI.
    let out_dir = match std::env::var("DYNLINK_BRIDGE_OUT") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".cache/dynlink-bridge-sfcgal")
        }
    };
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();

    let opts = DynlinkOptions {
        provider_id: "postgis-sfcgal-composed".to_string(),
        sub_ext: "postgis-sfcgal".to_string(),
    };

    if let Err(e) = emit_dynlink(&plan, &out_dir, &opts) {
        panic!("emit_dynlink failed: {e:?}");
    }

    // Sanity checks on emitted files.
    for f in [
        "Cargo.toml",
        "README.md",
        "src/lib.rs",
        "wit/world.wit",
        "wit/deps/compose-dynlink/linker.wit",
        "wit/deps/sys-compose/types.wit",
        "wit/deps/extension/world.wit",
    ] {
        assert!(
            out_dir.join(f).exists(),
            "missing emitted file: {}",
            out_dir.join(f).display()
        );
    }

    let lib_rs = std::fs::read_to_string(out_dir.join("src/lib.rs")).unwrap();
    assert!(lib_rs.contains("PROVIDER_ID"), "lib.rs must define PROVIDER_ID");
    assert!(
        lib_rs.contains("postgis-sfcgal-composed"),
        "lib.rs must reference the provider id"
    );
    assert!(
        lib_rs.contains("st-volume") || lib_rs.contains("st_volume"),
        "lib.rs must contain at least one SFCGAL arm (st_volume)"
    );

    // Structural: SFCGAL-family SQL names use snake_case.
    let sfcgal_arms = [
        "st_volume",
        "st_area_threed",
        "st_length_threed",
        "st_distance_threed",
        "st_intersects_threed",
        "st_convex_hull_threed",
    ];
    let hits: usize = sfcgal_arms
        .iter()
        .filter(|arm| lib_rs.contains(&format!("\"{}\"", arm)))
        .count();
    assert!(hits >= 5, "expected 5+ SFCGAL arms among {:?}, found {}", sfcgal_arms, hits);
    let arm_count = lib_rs.matches("=> {\n").count();
    eprintln!(
        "dynlink emit: {} scalar arms wired, {} sfcgal arms found",
        arm_count, hits
    );

    // Compile the emitted crate. wasm32-wasip2 is set up by the
    // dev environment; if it isn't, skip gracefully.
    let target = "wasm32-wasip2";
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", target])
        .current_dir(&out_dir)
        .status();
    let Ok(s) = status else {
        eprintln!("skipping compile check: cargo not runnable");
        return;
    };
    if !s.success() {
        panic!(
            "dynlink bridge did not compile — inspect {}",
            out_dir.display()
        );
    }

    let wasm = out_dir.join(format!(
        "target/{target}/release/postgis_sfcgal_datafission_bridge_dynlink.wasm"
    ));
    assert!(wasm.exists(), "no compiled wasm at {}", wasm.display());
    let sz = std::fs::metadata(&wasm).unwrap().len();
    eprintln!(
        "dynlink bridge wasm: {} bytes ({:.1} KB)",
        sz,
        sz as f64 / 1024.0
    );
    // Phase 3 goal per plan doc §6: ~200 KB. Empirical ceiling
    // ~1 MB accounts for wit-bindgen boilerplate + serde CBOR
    // codec. Any regression above 2 MB flags a plumbing leak.
    assert!(sz < 2_000_000, "dynlink bridge should be < 2 MB, got {} bytes", sz);
}
