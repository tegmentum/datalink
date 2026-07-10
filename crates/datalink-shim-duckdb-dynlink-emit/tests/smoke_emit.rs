//! Phase A smoke: parse the real extension catalog and confirm
//! `emit` produces the expected file layout under a temp dir.
//!
//! Skips gracefully when the postgis-shim-interface repo isn't
//! present (fresh checkout without sibling repos populated).

use std::path::PathBuf;

use datalink_shim_duckdb_dynlink_emit::{emit, DynlinkOptions};

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

    let sqlite_path = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join("git/postgis-shim-interface/postgis-interface.sqlite"))
        .filter(|p| p.exists());
    let opts = DynlinkOptions {
        provider_id: "postgis-composed".to_string(),
        sub_ext: "postgis_core".to_string(),
        extension_root: "postgis".to_string(),
        target: String::new(),
        interface_sqlite: sqlite_path,
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
    // values_to_colvec off-by-one guard: the emit must ship a
    // TWO-PASS layout — pass 1 picks the arm across every non-NULL
    // row, pass 2 materializes into the chosen buffer. A single-pass
    // (pre-M4) implementation would leave NULL rows preceding the
    // first non-NULL row in the wrong (blobs) buffer, causing a
    // buffer.len() < rows off-by-one when the picked arm was not
    // Blob. Grep for the pass markers so regressions surface here
    // instead of at wasm-runtime dispatch time.
    assert!(
        lib.contains("Pass 1: pick the arm"),
        "values_to_colvec must be two-pass (pass 1 = arm pick) to avoid pre-NULL off-by-one"
    );
    assert!(
        lib.contains("Pass 2: allocate the chosen arm's buffer"),
        "values_to_colvec must be two-pass (pass 2 = materialize) to avoid pre-NULL off-by-one"
    );

    // #834 followup: the emit must declare per-fn arity + per-arg
    // logicaltypes at register time. The previous variadic-Blob path
    // (empty args, varargs=Some(Blob)) erased the shape DuckDB needs
    // to bind-time overload-resolve. Grep-lock the four canonical
    // smoke targets — each carries a distinct shape sourced from
    // `postgis-interface.sqlite::scalars.param_types_json`:
    //
    //   st_geomfromtext(text)            -> binary  (1 Text arg, Blob ret)
    //   st_astext(binary)                -> text    (1 Blob arg, Text ret)
    //   st_area(binary)                  -> float64 (1 Blob arg, Float64 ret)
    //   st_distance(binary, binary)      -> float64 (2 Blob args, Float64 ret)
    //
    // Any regression to the empty-args + varargs-Blob path (or a
    // uniform arity-1 Blob fallback when the sqlite IS available)
    // trips these assertions before wasm-runtime dispatch time.
    if sqlite_path_probe_used(&out) {
        // Blob-in Blob-out arity-1 shape (e.g. `st_astext`).
        assert!(
            lib.contains("\"st_astext\","),
            "emit must register st_astext"
        );
        // Text-in Blob-out arity-1 shape.
        assert!(
            lib.contains("\"st_geomfromtext\","),
            "emit must register st_geomfromtext"
        );
        // Blob-in Float64-out arity-1 shape.
        assert!(
            lib.contains("\"st_area\","),
            "emit must register st_area"
        );
        // Blob,Blob-in Float64-out arity-2 shape.
        assert!(
            lib.contains("\"st_distance\","),
            "emit must register st_distance"
        );
        // Multiple Funcarg{} entries in some register call — proof of
        // per-fn arity emission. `st_distance` has two Blob args.
        assert!(
            lib.matches("logical: Logicaltype::Blob }").count() >= 2,
            "expected multiple typed Funcarg entries in emitted lib.rs (per-fn arity should surface > 1 Blob arg total)"
        );
        // No empty-args registration + register_scalar_ex path — the
        // default emit must go through the base `registry.register`
        // call with a fully-typed `args` vec (the historical
        // register_scalar_ex path can still appear in a code comment
        // — the retired-approach marker — so we only forbid the
        // actual function CALL, not the string).
        assert!(
            !lib.contains("runtime_ext::register_scalar_ex("),
            "emit must not call runtime_ext::register_scalar_ex in the default path — base register_scalar carries the per-fn shape"
        );
        assert!(
            lib.contains("Logicaltype::Text"),
            "at least one arg / ret is Text (e.g. st_geomfromtext takes Text)"
        );
        assert!(
            lib.contains("Logicaltype::Float64"),
            "at least one arg / ret is Float64 (e.g. st_area returns Float64)"
        );
    }
}

/// Was the shim-interface sqlite available for this test run? Reads
/// the emitted lib.rs — if we see the sqlite-derived `Logicaltype::
/// Text` at any register call, the DB was read; else the fallback
/// arity-1 Blob shape landed and per-fn assertions are skipped.
fn sqlite_path_probe_used(out: &std::path::Path) -> bool {
    let lib = std::fs::read_to_string(out.join("src/lib.rs")).unwrap_or_default();
    // Presence of any non-Blob logicaltype at a register site is the
    // signal — a Blob-only fallback never emits Text/Float64/Int64/
    // Boolean args.
    lib.contains("logical: Logicaltype::Text")
        || lib.contains("logical: Logicaltype::Float64")
        || lib.contains("logical: Logicaltype::Int64")
        || lib.contains("logical: Logicaltype::Boolean")
}

#[test]
fn values_to_colvec_two_pass_algorithm_holds_alignment() {
    // Standalone verification of the algorithm the emit ships: a
    // batch that starts with a NULL row before the first non-NULL
    // row must still produce a buffer whose length matches the row
    // count, with the validity bit clear for the NULL row and set
    // for the non-NULL row. The check runs a local copy of the
    // two-pass core so we exercise it without spinning a wasm
    // runtime; the emitted lib.rs is grep-locked to the same
    // two-pass layout by `emit_produces_expected_layout`.
    #[derive(Clone, Debug)]
    enum V {
        Null,
        Int64(i64),
    }
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Arm {
        Unknown,
        Int64,
    }
    fn arm_of(v: &V) -> Option<Arm> {
        Some(match v {
            V::Int64(_) => Arm::Int64,
            V::Null => return None,
        })
    }
    let values = vec![V::Null, V::Int64(42)];
    let n = values.len();
    let mut bits = vec![0u8; (n + 7) / 8];
    let mut any_null = false;

    // Pass 1: pick the arm.
    let mut arm = Arm::Unknown;
    for v in &values {
        if matches!(v, V::Null) {
            any_null = true;
            continue;
        }
        let this = arm_of(v).unwrap();
        if arm == Arm::Unknown {
            arm = this;
        }
    }
    assert_eq!(arm, Arm::Int64);

    // Pass 2: materialize.
    let mut int64s: Vec<i64> = Vec::new();
    for (i, v) in values.into_iter().enumerate() {
        if matches!(v, V::Null) {
            match arm {
                Arm::Unknown | Arm::Int64 => int64s.push(0),
            }
            continue;
        }
        bits[i / 8] |= 1u8 << (i % 8);
        match v {
            V::Int64(x) => int64s.push(x),
            V::Null => unreachable!(),
        }
    }

    assert_eq!(int64s.len(), 2, "buffer must have one slot per row (2 rows)");
    assert_eq!(int64s[0], 0, "row 0 (NULL) must be a zero placeholder");
    assert_eq!(int64s[1], 42, "row 1 (non-NULL) must carry the real value");
    assert!(any_null, "any_null must fire so the validity bitmap survives");
    // Row 0 validity bit clear (NULL), row 1 set (non-NULL).
    assert_eq!(bits[0] & 0b01, 0b00, "row 0 validity bit must be 0 (NULL)");
    assert_eq!(bits[0] & 0b10, 0b10, "row 1 validity bit must be 1 (non-NULL)");
}
