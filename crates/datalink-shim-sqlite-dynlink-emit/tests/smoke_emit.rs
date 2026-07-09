//! Phase A smoke: parse the real extension catalog and confirm
//! `emit` produces the expected file layout under a temp dir.
//!
//! Skips gracefully when the postgis-shim-interface repo isn't
//! present (fresh checkout without sibling repos populated).

use std::path::PathBuf;

use datalink_shim_sqlite_dynlink_emit::{emit, DynlinkOptions};

fn catalog_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("git/postgis-shim-interface/postgis-catalog.toml")
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
        // The dep-populate step relies on ~/git/sqlink/wit and
        // ~/git/datalink/crates/datalink-dynlink/wit being present.
        // Skip if either source tree is missing — the target-side
        // check is out of scope for this smoke test.
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

    // World file must import compose:dynlink/linker and export
    // the declarative `metadata` + `scalar-function` pair (per
    // the @1.0.0 contract). The pre-1.0 imperative `extension` +
    // `extension-callbacks` shape is retired.
    let world = std::fs::read_to_string(out.join("wit/world.wit")).unwrap();
    assert!(
        world.contains("import compose:dynlink/linker@0.1.0"),
        "world.wit missing compose:dynlink/linker import:\n{world}"
    );
    assert!(
        world.contains("export sqlite:extension/metadata@1.0.0"),
        "world.wit missing sqlite:extension/metadata export:\n{world}"
    );
    assert!(
        world.contains("export sqlite:extension/scalar-function@1.0.0"),
        "world.wit missing sqlite:extension/scalar-function export:\n{world}"
    );
    assert!(
        !world.contains("sqlite:extension/extension-callbacks"),
        "world.wit must not reference the stale extension-callbacks interface"
    );

    // Fix #3 + #6 verification: the emitted lib.rs must implement
    // the declarative `MetadataGuest::describe` + `ScalarFunctionGuest::call`
    // pair — not the pre-1.0 imperative `register_scalar_function`
    // model. `SqlValue` must be pattern-matched as a WIT variant
    // (SqlValue::Integer(...), SqlValue::Blob(...)) rather than
    // the stale `SqlValue { value_type: ValueType::* }` record.
    let lib = std::fs::read_to_string(out.join("src/lib.rs")).unwrap();
    assert!(
        lib.contains("impl MetadataGuest for Component"),
        "lib.rs must impl MetadataGuest (metadata.describe)"
    );
    assert!(
        lib.contains("impl ScalarFunctionGuest for Component"),
        "lib.rs must impl ScalarFunctionGuest (scalar-function.call)"
    );
    assert!(
        lib.contains("ScalarFunctionSpec {"),
        "describe() must populate ScalarFunctionSpec entries"
    );
    for arm in [
        "SqlValue::Null",
        "SqlValue::Integer(",
        "SqlValue::Real(",
        "SqlValue::Text(",
        "SqlValue::Blob(",
    ] {
        assert!(
            lib.contains(arm),
            "emitted lib.rs missing SqlValue arm `{arm}` — WIT variant name mismatch"
        );
    }
    for stale in [
        "register_scalar_function",
        "ExtensionError",
        "on_scalar_function",
        "value_type: ValueType::",
    ] {
        assert!(
            !lib.contains(stale),
            "emitted lib.rs still references stale pre-1.0 API `{stale}`"
        );
    }
}
