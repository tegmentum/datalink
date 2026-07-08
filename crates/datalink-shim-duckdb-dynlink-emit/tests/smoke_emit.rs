//! Phase A smoke: parse the real spatial-catalog and confirm
//! `emit` produces the expected file layout under a temp dir.
//!
//! Skips gracefully when the postgis-shim-interface repo isn't
//! present (fresh checkout without sibling repos populated).

use std::path::PathBuf;

use datalink_shim_duckdb_dynlink_emit::{emit, DynlinkOptions};

fn catalog_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("git/postgis-shim-interface/spatial-catalog.toml")
}

#[test]
fn emit_produces_expected_layout() {
    let catalog = catalog_path();
    if !catalog.exists() {
        eprintln!("skipping smoke_emit: {} not found", catalog.display());
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path().join("bridge");
    std::fs::create_dir_all(&out).unwrap();

    let opts = DynlinkOptions {
        provider_id: "postgis-composed".to_string(),
        sub_ext: "postgis_core".to_string(),
        extension_root: "postgis".to_string(),
        target: String::new(),
    };
    if let Err(e) = emit(&catalog, "postgis_core", &out, opts) {
        let msg = format!("{e:#}");
        if msg.contains("WIT source missing") {
            eprintln!("skipping smoke_emit: {msg}");
            return;
        }
        panic!("emit failed: {e:#}");
    }

    for f in [
        "Cargo.toml",
        "README.md",
        "src/lib.rs",
        "wit/world.wit",
    ] {
        let p = out.join(f);
        assert!(p.exists(), "expected file missing: {}", p.display());
    }

    let world = std::fs::read_to_string(out.join("wit/world.wit")).unwrap();
    assert!(
        world.contains("import compose:dynlink/linker@0.1.0"),
        "world.wit missing compose:dynlink/linker import:\n{world}"
    );
    assert!(
        world.contains("export duckdb:extension/guest@4.0.0"),
        "world.wit missing duckdb:extension/guest export:\n{world}"
    );
    assert!(
        world.contains("export duckdb:extension/callback-dispatch@4.0.0"),
        "world.wit missing callback-dispatch export:\n{world}"
    );
}
