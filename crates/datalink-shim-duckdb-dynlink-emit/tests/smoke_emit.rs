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

    // Fix #1-#2 verification: the emitted lib.rs must reference the
    // ACTUAL @4.0.0 Duckvalue variant arm names (Int8/Int16/Int32/
    // Int64/Uint8/…/Float32/Float64/Text) and the ACTUAL Loadresult
    // field names (version: Option, requires: Vec<Capabilitykind>).
    // Any regression to the pre-1.0 arm names (Tinyint / Smallint /
    // Integer / Bigint / Float / Double / Varchar) or to the stale
    // Loadresult `extras` field would fail cargo check on the
    // emitted crate; catching it here means we don't have to build
    // the wasm target to notice.
    let lib = std::fs::read_to_string(out.join("src/lib.rs")).unwrap();
    for arm in [
        "Duckvalue::Int8(",
        "Duckvalue::Int16(",
        "Duckvalue::Int32(",
        "Duckvalue::Int64(",
        "Duckvalue::Uint8(",
        "Duckvalue::Uint16(",
        "Duckvalue::Uint32(",
        "Duckvalue::Uint64(",
        "Duckvalue::Float32(",
        "Duckvalue::Float64(",
        "Duckvalue::Text(",
        "Duckvalue::Blob(",
        "Duckvalue::Boolean(",
        "Duckvalue::Null",
    ] {
        assert!(
            lib.contains(arm),
            "emitted lib.rs missing Duckvalue arm `{arm}` — WIT variant name mismatch"
        );
    }
    for stale in [
        "Duckvalue::Tinyint",
        "Duckvalue::Smallint",
        "Duckvalue::Integer",
        "Duckvalue::Bigint",
        "Duckvalue::Utinyint",
        "Duckvalue::Usmallint",
        "Duckvalue::Uinteger",
        "Duckvalue::Ubigint",
        "Duckvalue::Float(",
        "Duckvalue::Double(",
        "Duckvalue::Varchar",
    ] {
        assert!(
            !lib.contains(stale),
            "emitted lib.rs still references stale Duckvalue arm `{stale}`"
        );
    }
    assert!(
        lib.contains("version: Some(CATALOG_VERSION.to_string())"),
        "Loadresult.version must be Option<String>, not String"
    );
    assert!(
        lib.contains("requires: vec![Capabilitykind::"),
        "Loadresult.requires must be Vec<Capabilitykind>"
    );
    assert!(
        !lib.contains("extras: Vec::new()"),
        "Loadresult must not carry a stale `extras` field"
    );
    // Fix #5 verification: load() must actually register scalars.
    assert!(
        lib.contains("runtime::get_capability(Capabilitykind::Scalar)"),
        "load() must call runtime::get_capability(Scalar) to register scalars"
    );
    assert!(
        lib.contains("registry.register("),
        "load() must invoke scalar-registry.register — comment-only stubs are a regression"
    );
    assert!(
        lib.contains("runtime::ScalarCallback::new(handle)"),
        "each registered scalar must build a ScalarCallback::new(handle)"
    );
}
