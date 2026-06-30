//! Task #225 — REAL end-to-end verification of the async deep-reentrant +
//! streaming `compose:dynlink` host ported into `datalink-dynlink`.
//!
//! These mirror the integration-branch reference matrix
//! (`webassembly-component-orchestration`,
//! `hosts/wasmtime/tests/{sqlite_ext_endpoint_declarative,reentrant_spi_leaf,
//! deep_reentrant_spike}.rs`) but drive the PROVEN wasm fixtures through the
//! ASYNC host in this crate (`datalink_dynlink::reentrant::run_cli_dlopen`)
//! on a tokio runtime — exactly the executor sqlink runs on.
//!
//! Each test: register the flavor-B guest harness as root + the provider(s)
//! by id in an in-memory blob+trust store, run the harness as a
//! `wasi:cli/run` guest, and assert on the streamed/returned output. Tests
//! skip gracefully if a fixture is missing.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use datalink_dynlink::reentrant::{
    run_cli_dlopen, BlobSource, DeterminismMode, MemBlobs, TrustGate, CAP_INVOKE, CAP_RESOLVE,
};
use wasmtime::Engine;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn read_fixture(name: &str) -> Option<Vec<u8>> {
    match std::fs::read(fixture(name)) {
        Ok(b) => Some(b),
        Err(_) => {
            eprintln!("skipping: fixture not present: {}", fixture(name).display());
            None
        }
    }
}

/// A component-model engine with async support — the sqlink-shaped config.
fn async_engine() -> Engine {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    // wasmtime 46 drives the component model async by default; the reentrant
    // host instantiates/calls via the *_async bindings.
    Engine::new(&config).expect("engine")
}

fn full_grant() -> BTreeSet<String> {
    [CAP_RESOLVE.to_string(), CAP_INVOKE.to_string()]
        .into_iter()
        .collect()
}

/// Drive `harness` (flavor-B guest) with the named providers registered by
/// id, on the async executor. Returns (exit, stdout, stderr).
async fn run_dlopen(
    harness: &[u8],
    providers: &[(&str, &[u8])],
    env: &[(&str, &str)],
) -> (u32, String, String) {
    let engine = async_engine();
    let blobs = MemBlobs::new();
    let mut registry: Vec<(String, Vec<u8>)> = Vec::new();
    for (id, bytes) in providers {
        let digest = blobs.put(bytes);
        registry.push((id.to_string(), digest));
    }
    let blobs_arc: Arc<dyn BlobSource> = Arc::new(blobs.clone());
    let trust_arc: Arc<dyn TrustGate> = Arc::new(blobs);
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let out = run_cli_dlopen(
        &engine,
        harness,
        &registry,
        blobs_arc,
        trust_arc,
        DeterminismMode::Relaxed,
        full_grant(),
        &[],
        &env,
    )
    .await
    .expect("run_cli_dlopen");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    println!("==== exit {} ====", out.exit_code);
    println!("STDOUT:\n{stdout}");
    if !stderr.trim().is_empty() {
        println!("STDERR:\n{stderr}");
    }
    (out.exit_code, stdout, stderr)
}

// ── Tier 1: declarative scalar (aba) via the generic declarative harness ─

#[tokio::test]
async fn tier_scalar_aba_declarative() {
    let (Some(harness), Some(provider)) = (
        read_fixture("sqlite-ext-endpoint-harness.wasm"),
        read_fixture("aba-provider.wasm"),
    ) else {
        return;
    };
    let (code, out, err) =
        run_dlopen(&harness, &[("ext", &provider)], &[("SCENARIO", "scalar")]).await;
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("loaded extension: aba"), "{out}");
    assert!(out.contains("aba_validate('021000021') => 1"), "{out}");
    assert!(out.contains("aba_validate('021000022') => 0"), "{out}");
}

// ── Tier 2: streaming dot-command (greet) — host-mediated cli-stdout ──────

#[tokio::test]
async fn tier_dotcmd_stream_greet() {
    let (Some(harness), Some(provider)) = (
        read_fixture("sqlite-ext-endpoint-harness.wasm"),
        read_fixture("greet-provider.wasm"),
    ) else {
        return;
    };
    let (code, out, err) = run_dlopen(
        &harness,
        &[("ext", &provider)],
        &[("SCENARIO", "dotcmd_stream")],
    )
    .await;
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("loaded extension: greet"), "{out}");
    assert!(out.contains("dot-command id=1 name=.greet"), "{out}");
    // Output streamed through the HOST capture, not the invoke envelope —
    // this exercises ProviderKind::Cli + CliCapture + cli.drain-stdout.
    assert!(
        out.contains(r#"envelope_stdout="""#),
        "envelope should be empty (streamed via host): {out}"
    );
    assert!(
        out.contains(r#"host-captured cli-stdout => "hello, alice!\n""#),
        "{out}"
    );
}

// ── Tier 3: flat reentrant SPI through the REAL leaf sqlite-wasm engine ───

#[tokio::test]
async fn flat_reentrant_spi_leaf_engine() {
    let (Some(harness), Some(engine_prov)) = (
        read_fixture("reentrant-spi-harness.wasm"),
        read_fixture("sqlite_spi_leaf.component.wasm"),
    ) else {
        return;
    };
    let (code, out, err) = run_dlopen(&harness, &[("engine", &engine_prov)], &[]).await;
    assert_eq!(code, 0, "stderr: {err}");
    // The extension re-entered the linker to run SQL on the resolved engine.
    assert!(
        out.contains("spi.execute-scalar('SELECT 1 + 1') => 2"),
        "reentrant SELECT 1+1 should return 2: {out}"
    );
    assert!(
        out.contains("spi.execute-scalar('SELECT count(*) FROM t') => 3"),
        "inserted rows persist across reentrant calls: {out}"
    );
    assert!(out.contains("(2, alan)"), "row data should round-trip: {out}");
}

// ── Tier 4: DEEP nested reentrancy (engine-as-provider) ──────────────────

#[tokio::test]
async fn deep_nested_reentrancy_engine_and_ext() {
    let (Some(harness), Some(engine_prov), Some(ext_prov)) = (
        read_fixture("deep-harness.wasm"),
        read_fixture("deep_engine_provider.wasm"),
        read_fixture("deep_ext_provider.wasm"),
    ) else {
        return;
    };
    // Both providers registered by id; the engine provider re-enters to
    // resolve "ext" and the ext provider re-enters to resolve "engine" —
    // bidirectional deep reentrancy across distinct provider stores.
    let (code, out, err) =
        run_dlopen(&harness, &[("engine", &engine_prov), ("ext", &ext_prov)], &[]).await;
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("id=1 my_ext_scalar(id)=10"), "1 -> 10: {out}");
    assert!(out.contains("id=2 my_ext_scalar(id)=20"), "2 -> 20: {out}");
    assert!(out.contains("id=3 my_ext_scalar(id)=30"), "3 -> 30: {out}");
    assert!(
        out.contains("deep nested reentrancy over dynlink: OK"),
        "{out}"
    );
}

// ── Negative-path: gates still hold on the async host ─────────────────────

#[tokio::test]
async fn untrusted_digest_is_rejected() {
    let Some(harness) = read_fixture("sqlite-ext-endpoint-harness.wasm") else {
        return;
    };
    let engine = async_engine();
    let blobs = MemBlobs::new();
    // Register an id pointing at a digest that is NOT in the blob/trust store.
    let bogus = vec![7u8; 32];
    let registry = vec![("ext".to_string(), bogus)];
    let blobs_arc: Arc<dyn BlobSource> = Arc::new(blobs.clone());
    let trust_arc: Arc<dyn TrustGate> = Arc::new(blobs);
    let result = run_cli_dlopen(
        &engine,
        &harness,
        &registry,
        blobs_arc,
        trust_arc,
        DeterminismMode::Relaxed,
        full_grant(),
        &[],
        &[("SCENARIO".to_string(), "scalar".to_string())],
    )
    .await;
    // The host's trust gate rejects the untrusted digest cleanly: the guest's
    // `resolve(ext)` returns the host error, the harness `die()`s on it (a
    // guest-side abort, NOT a host trap), and that surfaces as a run error
    // whose drained guest-stderr carries the host's "untrusted" message. The
    // important property: the host refused to instantiate untrusted code.
    let err = result.expect_err("untrusted resolve must not yield a clean run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("untrusted digest") || msg.contains("resolve(ext) failed"),
        "expected the host's untrusted-digest rejection to surface; got: {msg}"
    );
}
