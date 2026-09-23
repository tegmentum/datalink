//! Pins the sync-dispatch contract introduced by commit `0032272`
//! (`feat(datalink-dynlink-wasmos): align with wasmos sync_dispatch
//! contract`).
//!
//! ## What this test file protects
//!
//! Commit `0032272` migrated [`DynLinkBridge`] from `HostCall` (async)
//! to [`SyncHostCall`] (sync). [`install_host_imports`] now registers
//! the bridge via [`HostImports::register_sync`] so wasmos, under
//! `RuntimeConfig::sync_dispatch(true)`, sees a
//! `SyncHostCallAdapter`-wrapped handler that is guaranteed
//! `Poll::Ready` on the first poll (`now_or_never` polling
//! discipline — a `Pending` handler under that regime panics).
//!
//! The three cases below pin the contract points a future refactor
//! could easily regress:
//!
//! 1. `install_host_imports` returns a [`HostImports`] whose
//!    interface set contains `LINKER_INTERFACE`.
//! 2. `resolve-by-id` on a registered provider returns synchronously.
//!    The sync entry point ([`SyncHostCall::call`]) does not need an
//!    ambient tokio runtime, and the adapter-wrapped async future the
//!    linker sees is `Ready` on first poll (`now_or_never` yields
//!    `Some(_)`).
//! 3. `resolve-by-id` on an unregistered provider returns cleanly —
//!    the [`Error`] surfaces inside a `Value::Result(Err(_))` envelope
//!    rather than panicking or bubbling a [`RuntimeError`] out to the
//!    adapter.
//!
//! The tests do NOT touch wasmtime — the bridge is driven with a
//! stub [`ProviderBackend`] and a stub [`HostCallCtxImpl`] so the
//! sync-contract check stays local to this crate's dispatch code.

use std::collections::HashSet;
use std::future::Future;
use std::pin::pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use bytes::Bytes;

use datalink_dynlink_wasmos::{
    install_host_imports, DynLinkBridge, Error, ErrorCode, ProviderBackend, INVOKE_METHOD,
    LINKER_INTERFACE,
};
use wasmos_runtime_api::{
    HostCallContext, HostCallCtxImpl, HostImports, RuntimeError, RuntimeResult, SyncHostCall,
    Value,
};

// ─── Stub ProviderBackend ────────────────────────────────────────────
//
// Resolves entirely in-memory; every method returns Ready
// synchronously (the `#[async_trait]` future compiles to an already-
// completed state). No wasmtime, no tokio needed.

#[derive(Default)]
struct StubBackend {
    registered: Mutex<HashSet<String>>,
}

impl StubBackend {
    fn with_registered(ids: &[&str]) -> Self {
        let this = Self::default();
        {
            let mut guard = this.registered.lock().unwrap();
            for id in ids {
                guard.insert((*id).to_string());
            }
        }
        this
    }
}

#[async_trait]
impl ProviderBackend for StubBackend {
    type Handle = String;

    async fn resolve_by_id(&self, id: &str) -> Result<Self::Handle, Error> {
        if self.registered.lock().unwrap().contains(id) {
            Ok(id.to_string())
        } else {
            Err(Error::new(
                ErrorCode::BlobNotFound,
                format!("stub backend: no provider registered under {id:?}"),
            ))
        }
    }

    async fn resolve_by_digest(&self, digest: &[u8]) -> Result<Self::Handle, Error> {
        Err(Error::new(
            ErrorCode::BlobNotFound,
            format!("stub backend: no digest mapping for {digest:02x?}"),
        ))
    }

    async fn invoke(
        &self,
        handle: &Self::Handle,
        method: &str,
        _payload: Bytes,
    ) -> Result<Bytes, Error> {
        Err(Error::new(
            ErrorCode::NotImplemented,
            format!("stub backend: invoke({handle:?}, {method:?}) not implemented"),
        ))
    }
}

// ─── Stub HostCallCtxImpl ────────────────────────────────────────────
//
// The dispatch bodies call `ctx.new_host_resource(...)` on
// resolve-by-id success and `ctx.resource_rep(...)` on invoke; both
// stub out to opaque, in-memory bookkeeping so the sync path exercises
// the same code that fires under a real wasmtime adapter without
// pulling one in.

struct StubCtx {
    next_handle_id: u64,
    last_rep: Option<u32>,
}

impl StubCtx {
    fn new() -> Self {
        Self { next_handle_id: 1, last_rep: None }
    }
}

impl HostCallCtxImpl for StubCtx {
    fn new_host_resource(
        &mut self,
        _interface: &str,
        _resource_name: &str,
        rep: u32,
    ) -> RuntimeResult<Value> {
        self.last_rep = Some(rep);
        let handle_id = self.next_handle_id;
        self.next_handle_id += 1;
        Ok(Value::Resource { store_id: 42, handle_id })
    }

    fn resource_rep(&mut self, value: &Value) -> RuntimeResult<u32> {
        // Stub: recover from `last_rep` so tests that need to round-
        // trip a handle back through invoke can do so without a real
        // per-store table.
        match value {
            Value::Resource { .. } => self.last_rep.ok_or_else(|| {
                RuntimeError::msg("stub ctx: resource_rep called before new_host_resource")
            }),
            other => Err(RuntimeError::msg(format!(
                "stub ctx: resource_rep expected Value::Resource, got {other:?}"
            ))),
        }
    }
}

// ─── `now_or_never` on std alone ─────────────────────────────────────
//
// Kept inline so no dev-dep bump is needed. Semantics match
// `futures::FutureExt::now_or_never`: poll once with a no-op waker,
// return `Some(_)` iff Ready, `None` on Pending. The test point is
// exactly what wasmos's sync_dispatch does — see
// `runtime/api/src/host_imports.rs` for the contract.

fn now_or_never<F: Future>(fut: F) -> Option<F::Output> {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut fut = pin!(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => Some(v),
        Poll::Pending => None,
    }
}

// ─── Contract point #1 ──────────────────────────────────────────────

#[test]
fn install_host_imports_registers_linker_interface() {
    let backend = std::sync::Arc::new(StubBackend::default());
    let imports: HostImports = install_host_imports(HostImports::new(), backend);
    let iface_names: Vec<&str> = imports.iter().map(|(iface, _)| iface).collect();
    assert!(
        iface_names.contains(&LINKER_INTERFACE),
        "install_host_imports must register {LINKER_INTERFACE:?}; registered ifaces: {iface_names:?}"
    );
    // Also assert lookup by name works — the adapter uses this path
    // during wire-up.
    assert!(
        imports.get(LINKER_INTERFACE).is_some(),
        "HostImports::get({LINKER_INTERFACE:?}) must resolve after install_host_imports"
    );
}

// ─── Contract point #2 ──────────────────────────────────────────────

#[test]
fn resolve_by_id_returns_ready_via_sync_entry_point() {
    // Direct sync entry — no tokio runtime, no now_or_never needed.
    // If SyncHostCall::call ever regresses to needing a runtime, this
    // test panics with the tell-tale "there is no reactor running"
    // shape.
    let backend = std::sync::Arc::new(StubBackend::with_registered(&["hello"]));
    let bridge = DynLinkBridge::new(backend);
    let mut stub = StubCtx::new();
    let mut ctx = HostCallContext::new(&mut stub);

    let out = bridge
        .call(&mut ctx, "resolve-by-id", vec![Value::String("hello".into())])
        .expect("resolve-by-id must succeed on a registered provider");
    match out.as_slice() {
        [Value::Result(Ok(Some(inner)))] => match inner.as_ref() {
            Value::Resource { .. } => {}
            other => panic!("resolve-by-id must return a Resource inside Ok, got {other:?}"),
        },
        other => panic!("resolve-by-id must return `Result::Ok(Some(Resource))`, got {other:?}"),
    }
}

#[test]
fn resolve_by_id_future_is_ready_on_first_poll_via_adapter() {
    // Adapter path — exactly what wasmos's sync_dispatch sees. The
    // handler is `install_host_imports`-registered via
    // register_sync, so it is a SyncHostCallAdapter-wrapped
    // Arc<dyn HostCall>. Under sync_dispatch wasmos polls this future
    // via `now_or_never`; a Pending on the first poll panics loudly.
    // We assert Ready-on-first-poll directly.
    let backend = std::sync::Arc::new(StubBackend::with_registered(&["hello"]));
    let imports = install_host_imports(HostImports::new(), backend);
    let handler = imports
        .get(LINKER_INTERFACE)
        .expect("install_host_imports registered LINKER_INTERFACE");

    let mut stub = StubCtx::new();
    let mut ctx = HostCallContext::new(&mut stub);
    let fut = handler.call(&mut ctx, "resolve-by-id", vec![Value::String("hello".into())]);

    let outcome = now_or_never(fut).expect(
        "SyncHostCallAdapter-wrapped resolve-by-id future must be Ready on first poll — \
         a `None` here means the bridge would panic under wasmos sync_dispatch",
    );
    let values = outcome.expect("resolve-by-id dispatch must not surface RuntimeError");
    assert!(
        matches!(values.as_slice(), [Value::Result(Ok(Some(_)))]),
        "resolve-by-id must return Result::Ok(Some(_)); got {values:?}"
    );
}

// ─── Contract point #3 ──────────────────────────────────────────────

#[test]
fn resolve_by_id_on_unregistered_provider_returns_error_envelope() {
    // The path an unresolved id takes on the DISPATCH side — the
    // task's "invoke against unresolved id" case is hard to reach
    // without minting a real Value::Resource, so we pin the closest
    // reachable variant: resolve-by-id against an unregistered
    // provider surfaces the backend Error as a `Value::Result(Err)`
    // envelope, NOT a RuntimeError panic and NOT a Rust-level Err.
    let backend = std::sync::Arc::new(StubBackend::default());
    let bridge = DynLinkBridge::new(backend);
    let mut stub = StubCtx::new();
    let mut ctx = HostCallContext::new(&mut stub);

    let out = bridge
        .call(&mut ctx, "resolve-by-id", vec![Value::String("nope".into())])
        .expect("resolve-by-id dispatch must not surface a RuntimeError for an unregistered id");
    match out.as_slice() {
        [Value::Result(Err(Some(payload)))] => match payload.as_ref() {
            Value::Record(fields) => {
                // The `code` field holds the WIT variant; verify it's
                // the "blob-not-found" our stub raises.
                let code = fields.iter().find(|(k, _)| k == "code").expect("code field");
                match &code.1 {
                    Value::Variant { discriminant, .. } => {
                        assert_eq!(discriminant, "blob-not-found");
                    }
                    other => panic!("expected code variant, got {other:?}"),
                }
            }
            other => panic!("expected error record inside Result::Err, got {other:?}"),
        },
        other => panic!(
            "resolve-by-id on unregistered provider must return Result::Err(Some(_)), \
             got {other:?}"
        ),
    }
}

#[test]
fn invoke_on_unknown_rep_returns_runtime_error_not_panic() {
    // Exact analogue of the task's contract point #3: an invoke
    // called with a Value::Resource whose rep the bridge never
    // recorded surfaces cleanly as a Rust-level `Err(RuntimeError)`
    // — the adapter turns that into a guest-visible trap — instead
    // of panicking on the `.expect` inside the handle-table lookup.
    let backend = std::sync::Arc::new(StubBackend::default());
    let bridge = DynLinkBridge::new(backend);
    let mut stub = StubCtx::new();
    // Pre-seed `last_rep` so the stub ctx's `resource_rep` returns a
    // rep value — but nothing was ever inserted into the bridge's
    // handle table, so the lookup fails cleanly.
    stub.last_rep = Some(999);
    let mut ctx = HostCallContext::new(&mut stub);

    let err = bridge
        .call(
            &mut ctx,
            INVOKE_METHOD,
            vec![
                Value::Resource { store_id: 42, handle_id: 1 },
                Value::String("m".into()),
                Value::Bytes(Bytes::from_static(b"")),
            ],
        )
        .expect_err("invoke on an unknown rep must surface as RuntimeError, not a panic");
    let msg = err.to_string();
    assert!(
        msg.contains("no backend handle"),
        "error message should name the missing-handle path; got {msg:?}"
    );
}
