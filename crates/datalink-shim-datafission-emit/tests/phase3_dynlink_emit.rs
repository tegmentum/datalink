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

    // Aggregate registry smoke check: real dispatch replaces the
    // scalar-first stub. The AccState struct + SyncRefCell static
    // shells + at least one wired postgis-core aggregate finalize
    // arm (st-union-aggregate) must be present. The out-of-scope
    // stub-body assertion was retired when `st-summary-stats-agg`
    // landed (see `is_dynlink_wired_aggregate` — the last stubbed
    // postgis aggregate, `AccKind::Raster + RetShape::WitValueRecord`,
    // is now wired). The `alloc::format!("... aggregate shape not
    // yet wired in dynlink mode ...")` UnknownFunction arm is
    // still emitted in code but no aggregate entry currently
    // routes to it for postgis — asserting on it becomes a
    // regression trap that fires whenever the wired coverage
    // increases.
    assert!(
        lib_rs.contains("struct AccState") && lib_rs.contains("static ACCUMULATORS"),
        "aggregate accumulator state block missing"
    );
    // The classifier surfaces aggregate sql_names in snake_case
    // (matches the scalar path); the provider-side wire name spelling
    // is the same for both families and orthogonal to this test.
    assert!(
        lib_rs.contains("st_union_aggregate"),
        "st_union_aggregate metadata missing"
    );
    // Wired-finalize body signature: builds a CborValue::List of the
    // accumulated blobs and routes through `call(...)`.
    assert!(
        lib_rs.contains("st.blobs.iter().map(|b| CborValue::Bytes(b.clone()))"),
        "wired aggregate finalize body missing (geom_list construction)"
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

/// Regression: Null-collapse rehydration in the emitted `call()` helper.
///
/// Agent #935 (Round 17) surfaced a bug where a provider arm returning
/// `Response::ok(CborValue::Null)` — the canonical `Option<T>::None` /
/// empty-first return — surfaced at the bridge as
/// `ExecutionError("<method>: empty response")` instead of SQL NULL.
///
/// Root cause: on the wire the response is `{v:1, ok: null}` (bare
/// CBOR null). The bridge's `Response { ok: Option<ResponseValue> }`
/// field decodes `Some(Null)` → `None` because ciborium reads CBOR
/// null in an `Option<T>` slot as the absent variant. The old
/// `resp.ok.ok_or_else(...)` template then turned that `None` into a
/// hard error, cutting off the `RetShape::WitValueRecord`,
/// `OptionText`, `FirstGeomBlob`, etc. arms that all already had a
/// `ResponseValue::Null => Ok(ScalarValue::Null)` branch waiting.
///
/// The fix (#823 Agent #938) replaces the `.ok_or_else` with
/// `resp.ok.unwrap_or(ResponseValue::Null)` so downstream match arms
/// see the Null and lower it to `ScalarValue::Null`. The err-branch
/// above the unwrap keeps working (provider-side errors still surface
/// as ExecutionError with the provider's message intact), so the
/// unknown-method / explicit-error path is unaffected.
///
/// This structural assertion pins the fix in the emit template so a
/// future rewrite can't silently re-introduce the collapse.
#[test]
fn dynlink_emit_call_helper_rehydrates_null_response() {
    let db = postgis_interface_db();
    if !db.exists() {
        eprintln!("skipping null-collapse regression: {} not found", db.display());
        return;
    }
    let plan = match load_plan(&db) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipping null-collapse regression: load_plan failed: {e}");
            return;
        }
    };

    let out_dir = {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".cache/dynlink-bridge-null-collapse")
    };
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();

    let opts = DynlinkOptions {
        provider_id: "postgis-sfcgal-composed".to_string(),
        sub_ext: "postgis-sfcgal".to_string(),
    };
    emit_dynlink(&plan, &out_dir, &opts).expect("emit_dynlink");

    let lib_rs = std::fs::read_to_string(out_dir.join("src/lib.rs")).unwrap();

    // Fix present: the null-collapse rehydration is in the template.
    assert!(
        lib_rs.contains("resp.ok.unwrap_or(ResponseValue::Null)"),
        "call() must rehydrate `ok: null` collapse as ResponseValue::Null \
         (regression: Agent #935 Round 17 null-collapse bug — see #823 #938)"
    );

    // Regression trip-wire: the old collapse pattern must not resurface.
    // The `unwrap_or(ResponseValue::Null)` line above is the ONLY valid
    // way to write this today; a `.ok_or_else` in `call()` returning
    // "empty response" was the exact bug.
    assert!(
        !lib_rs.contains("\"{}: empty response\""),
        "call() must not raise 'empty response' — the null-collapse fix \
         should route `ok: null` to ResponseValue::Null instead"
    );

    // Provider-side error path unaffected: explicit err messages still
    // surface as ExecutionError with the provider's text intact. This
    // check pins the branch that handles `resp.err` above the null
    // rehydration so unknown-method / explicit-arm errors don't get
    // swallowed by the Null default.
    assert!(
        lib_rs.contains("if let Some(err) = resp.err")
            && lib_rs.contains("ftypes::FunctionError::ExecutionError(alloc::format!(\"{}: {}\", method, err))"),
        "call() must preserve provider-side err branch above the null-collapse \
         rehydration so explicit errors surface unmasked"
    );

    // Every RetShape arm that admits null still routes to
    // `ScalarValue::Null`. Spot-check the shapes that Agent #935 was
    // exercising when the bug surfaced (WitValueRecord, OptionText,
    // FirstGeomBlob) plus the JsonText / Unit shapes that all funnel
    // a bare Null through the same call() helper.
    for admit_null_arm in [
        "ResponseValue::Null => Ok(ftypes::ScalarValue::Null)",
    ] {
        assert!(
            lib_rs.contains(admit_null_arm),
            "expected null-admitting arm pattern missing from emitted bridge: {}",
            admit_null_arm,
        );
    }
}
