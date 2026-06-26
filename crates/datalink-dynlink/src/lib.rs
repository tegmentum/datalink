//! datalink-dynlink — shared, store-generic host machinery for the
//! `compose:dynlink/linker` WIT package ("dlopen for components").
//!
//! A guest imports `compose:dynlink/linker`, resolves a provider by id (or
//! digest), and `invoke(method, payload)`s it through an opaque, host-owned
//! handle. The host forwards the bytes verbatim to the provider's
//! `compose:dynlink/endpoint.handle` export — no typed WIT values cross the
//! boundary, so the nominal-resource type-identity problem never arises.
//!
//! ## What is shared here vs. what each host implements
//!
//! Three host engines (the orchestration framework, ducklink, and sqlink)
//! independently mirrored this machinery. The DB-/host-agnostic part is the
//! **linker-host machinery**:
//!   - the generated `compose:dynlink` bindings ([`bindings`], [`provider`]),
//!   - the resource table that owns the opaque `instance` handles,
//!   - the `resolve`/`invoke`/`drop` routing,
//!   - the `add_to_linker` / `imports_linker` plumbing.
//!
//! That all lives in this crate, behind [`DynLinkBridge`] and the
//! [`impl_datalink_dynlink_host`] macro, generic over a store type.
//!
//! The part that genuinely differs between hosts is the **provider
//! lifecycle** — when a provider is instantiated, whether its store is reused
//! or thrown away, and how `invoke` reaches it. That is captured by the
//! [`ProviderBackend`] trait. This crate ships one backend,
//! [`ResidentBackend`] (instantiate-ONCE-and-reuse, with preopened dirs —
//! ducklink's "one heavy shared provider serving many function components"
//! model, and the enabler for resident S3/HTTP providers). A
//! fresh-store-per-invoke backend and host-specific built-in shims (e.g.
//! sqlink's `SqliteRuntime`) are additional [`ProviderBackend`] impls on the
//! consumer side; this crate's bridge drives any of them uniformly.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{
    DirPerms, FilePerms, ResourceTable as WasiResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView,
    WasiView,
};

/// Generated bindings for the guest-facing `compose:dynlink/linker` import.
/// We need the host (import) side: the `linker` interface + the `instance`
/// resource (mapped to a host backing type by the consumer's store).
pub mod bindings {
    wasmtime::component::bindgen!({
        path: "wit/compose-dynlink",
        world: "dynlink-guest",
    });
}

/// Generated bindings for instantiating a *provider* component — one that
/// exports `compose:dynlink/endpoint`. Kept in its own module so its
/// generated `compose::dynlink` / `sys::compose` types don't collide with the
/// guest-side `linker` bindings above.
pub mod provider {
    wasmtime::component::bindgen!({
        path: "wit/compose-dynlink",
        world: "dynlink-provider",
    });
}

pub use bindings::compose::dynlink::linker::Instance;
pub use bindings::sys::compose::types::{Error, ErrorCode};

/// Build a host `Error` with the given code and message.
pub fn err(code: ErrorCode, message: impl Into<String>) -> Error {
    Error {
        code,
        message: message.into(),
        context: None,
    }
}

/// Lower a provider-side endpoint error (a distinct generated Rust type with
/// identical shape) into the guest-facing `Error`.
pub fn lower_provider_error(e: provider::sys::compose::types::Error) -> Error {
    Error {
        code: ErrorCode::ExecTrap,
        message: format!("provider endpoint error: {}", e.message),
        context: e.context,
    }
}

/// Retype a guest-facing `Resource<Instance>` to a host backing type
/// `Resource<H>`. The two share the same table rep — the type parameter is
/// only a host-side compile-time tag — so this is a sound reinterpretation,
/// not a cast across distinct table entries.
fn as_backing<H: 'static>(r: &Resource<Instance>) -> Resource<H> {
    Resource::new_own(r.rep())
}

/// The provider lifecycle, abstracted away from the linker-host machinery.
///
/// A `ProviderBackend` decides WHEN a provider is instantiated (lazily once,
/// or fresh per invoke, or never — for a host shim), HOW it is reused, and
/// HOW an `invoke` reaches it. The shared [`DynLinkBridge`] drives any backend
/// uniformly: it owns the opaque-handle resource table and routes
/// resolve/invoke/drop through the backend.
///
/// The bridge is `&mut`-driven from a guest call, which is synchronous in the
/// framework + ducklink hosts; this trait is correspondingly synchronous. An
/// async host (sqlink) keeps its own async linker Host today — see the crate
/// docs and `CONSOLIDATION.md` for the async-seam follow-up.
pub trait ProviderBackend {
    /// What a resolved `instance` handle remembers so `invoke`/`drop` can
    /// reach the provider. Cheap to clone (it is stored in the bridge's
    /// resource table, which requires `Send + 'static`). For the resident
    /// backend this is just the provider id plus a shared handle to the
    /// registry.
    type Handle: Clone + Send + 'static;

    /// Resolve a provider by registry id, performing any instantiation the
    /// backend's lifecycle requires (e.g. materialize the resident instance
    /// once), and return the handle to remember.
    fn resolve_by_id(&self, id: &str) -> Result<Self::Handle, Error>;

    /// Resolve a provider by content digest. Backends that don't support
    /// digest resolution return `ErrorCode::NotImplemented`.
    fn resolve_by_digest(&self, digest: &[u8]) -> Result<Self::Handle, Error>;

    /// Forward an opaque message to the provider behind `handle`.
    fn invoke(&self, handle: &Self::Handle, method: &str, payload: &[u8]) -> Result<Vec<u8>, Error>;

    /// Notification that a resolved `instance` handle was dropped by the
    /// guest. Lets a backend release per-handle bookkeeping (the resident
    /// backend decrements its live-handle count). Default: no-op.
    fn on_drop(&self, _handle: &Self::Handle) {}
}

/// The per-component dynlink bridge: a [`ProviderBackend`] plus the resource
/// table owning the `instance` handles handed to one guest. Embedded in any
/// store state that wants to satisfy a guest's `compose:dynlink/linker`
/// import; the resolve/invoke logic lives here so every store type delegates
/// to ONE implementation (via [`impl_datalink_dynlink_host`]).
pub struct DynLinkBridge<B: ProviderBackend> {
    dyn_table: ResourceTable,
    backend: B,
}

impl<B: ProviderBackend> DynLinkBridge<B> {
    pub fn new(backend: B) -> Self {
        Self {
            dyn_table: ResourceTable::new(),
            backend,
        }
    }

    /// Access the underlying backend (e.g. to inspect resident state).
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Resolve by id: ask the backend for a handle, then mint an opaque
    /// resource pointing at it.
    pub fn resolve_by_id(&mut self, id: String) -> Result<Resource<Instance>, Error> {
        let handle = self.backend.resolve_by_id(&id)?;
        self.push_handle(handle)
    }

    /// Resolve by digest: same, via the backend's digest path.
    pub fn resolve_by_digest(&mut self, d: Vec<u8>) -> Result<Resource<Instance>, Error> {
        let handle = self.backend.resolve_by_digest(&d)?;
        self.push_handle(handle)
    }

    fn push_handle(&mut self, handle: B::Handle) -> Result<Resource<Instance>, Error> {
        let backing = self
            .dyn_table
            .push(handle)
            .map_err(|e| err(ErrorCode::InternalError, format!("table push: {e:?}")))?;
        Ok(Resource::new_own(backing.rep()))
    }

    /// Forward `method`/`payload` verbatim to the backend behind the handle.
    pub fn invoke(
        &mut self,
        self_: Resource<Instance>,
        method: String,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        let handle = self
            .dyn_table
            .get(&as_backing::<B::Handle>(&self_))
            .map_err(|e| err(ErrorCode::InternalError, format!("unknown dynlink handle: {e:?}")))?
            .clone();
        self.backend.invoke(&handle, &method, &payload)
    }

    /// Release an `instance` handle and notify the backend.
    pub fn drop_handle(&mut self, rep: Resource<Instance>) -> wasmtime::Result<()> {
        let handle = self.dyn_table.delete(as_backing::<B::Handle>(&rep))?;
        self.backend.on_drop(&handle);
        Ok(())
    }
}

/// Implement the `compose:dynlink/linker` Host + HostInstance traits for a
/// store type that exposes a `&mut DynLinkBridge<B>`. Every store type that
/// satisfies a guest's linker import delegates through the ONE bridge
/// implementation — no duplicated resolve/invoke logic.
///
/// Usage: `impl_datalink_dynlink_host!(MyStoreState<MyBackend>, my_bridge_accessor);`
/// where the accessor returns `&mut DynLinkBridge<MyBackend>`.
#[macro_export]
macro_rules! impl_datalink_dynlink_host {
    ($ty:ty, $backend:ty, $bridge:ident) => {
        impl $crate::bindings::sys::compose::types::Host for $ty {}

        impl $crate::bindings::compose::dynlink::linker::Host for $ty {
            fn resolve_by_id(
                &mut self,
                id: ::std::string::String,
            ) -> ::core::result::Result<
                ::wasmtime::component::Resource<$crate::Instance>,
                $crate::Error,
            > {
                $crate::DynLinkBridge::<$backend>::resolve_by_id(self.$bridge(), id)
            }

            fn resolve_by_digest(
                &mut self,
                d: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<
                ::wasmtime::component::Resource<$crate::Instance>,
                $crate::Error,
            > {
                $crate::DynLinkBridge::<$backend>::resolve_by_digest(self.$bridge(), d)
            }
        }

        impl $crate::bindings::compose::dynlink::linker::HostInstance for $ty {
            fn invoke(
                &mut self,
                self_: ::wasmtime::component::Resource<$crate::Instance>,
                method: ::std::string::String,
                payload: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::Error> {
                $crate::DynLinkBridge::<$backend>::invoke(self.$bridge(), self_, method, payload)
            }

            fn drop(
                &mut self,
                rep: ::wasmtime::component::Resource<$crate::Instance>,
            ) -> ::wasmtime::Result<()> {
                $crate::DynLinkBridge::<$backend>::drop_handle(self.$bridge(), rep)
            }
        }
    };
}

/// Convenience for the bindgen-generated `add_to_linker` signature
/// (`F: Fn(&mut T) -> &mut Self::Data`).
pub struct HasSelf<T>(std::marker::PhantomData<T>);
impl<T: 'static> wasmtime::component::HasData for HasSelf<T> {
    type Data<'a> = &'a mut T;
}

/// Whether a compiled component imports the `compose:dynlink/linker` interface
/// (flavor B: guest-driven dlopen). Used to conditionally add the host import
/// so components that DON'T import it are unaffected.
pub fn imports_linker(engine: &Engine, component: &Component) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name.starts_with("compose:dynlink/linker"))
}

/// Add the `compose:dynlink/linker` host import to a guest linker over any
/// store type `T` that implements the linker Host traits (via
/// [`impl_datalink_dynlink_host`]). WASI must be added separately by the
/// caller.
pub fn add_to_linker<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: bindings::compose::dynlink::linker::Host
        + bindings::compose::dynlink::linker::HostInstance
        + bindings::sys::compose::types::Host
        + 'static,
{
    bindings::DynlinkGuest::add_to_linker::<_, HasSelf<T>>(linker, |s| s)
        .map_err(|e| wasmtime::Error::msg(format!("add compose:dynlink/linker to linker: {e:?}")))
}

// ===========================================================================
// ResidentBackend — instantiate-ONCE-and-reuse, with preopened dirs.
// ===========================================================================

/// A directory to preopen into a provider's OWN store, mounted at `guest`
/// (e.g. `/lib`) from the host path `host`. A pylon-shaped provider needs its
/// CPython `Lib` (with bundled numpy) and its dispatcher `pylib` dir preopened
/// so the resident interpreter can import them.
#[derive(Clone, Debug)]
pub struct ProviderPreopen {
    /// Host filesystem path to expose.
    pub host: PathBuf,
    /// Guest mount point (e.g. "/lib" or "/app").
    pub guest: String,
}

impl ProviderPreopen {
    pub fn new(host: impl Into<PathBuf>, guest: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            guest: guest.into(),
        }
    }
}

/// Minimal WASI store-state for a resident provider's OWN store. Provider
/// components pull in WASI even for trivial logic (std), so the store carries
/// a minimal WASI context.
struct ProviderState {
    wasi: WasiCtx,
    table: WasiResourceTable,
}

impl WasiView for ProviderState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// A resident, instantiated provider. Instantiated ONCE (lazily) and shared
/// across every resolve/invoke. Holds its own store so calls into the provider
/// never touch the calling guest's store.
struct ResidentProvider {
    store: Store<ProviderState>,
    instance: provider::DynlinkProvider,
}

/// Registration record for a provider id. The component is compiled at
/// registration time; the resident instance is materialized lazily on the
/// first resolve (and then reused).
struct ProviderEntry {
    component: Component,
    /// `Some(..)` once instantiated; reused across all subsequent resolves and
    /// invokes (the shared model).
    resident: Option<ResidentProvider>,
    path: PathBuf,
    /// Directories to preopen into the provider's OWN store when it is
    /// materialized. Empty for a plain provider; a pylon provider carries
    /// `/lib` (CPython Lib + numpy) and `/app` (dispatcher).
    preopens: Vec<ProviderPreopen>,
    /// Number of live `instance` handles outstanding for this id. Bumped on
    /// resolve, decremented on handle drop — used to assert/log the
    /// shared-copy property (N handles, 1 resident).
    handle_count: u64,
}

struct RegistryInner {
    engine: Engine,
    providers: HashMap<String, ProviderEntry>,
    /// Optional digest -> id mapping for `resolve-by-digest`.
    digest_to_id: HashMap<Vec<u8>, String>,
}

/// The provider registry: the wasm engine plus an `id -> ProviderEntry` map.
/// Wrapped in an `Arc<Mutex<..>>` so it can be cloned into a per-load store
/// state. This is both the registration surface AND the [`ResidentBackend`]'s
/// shared state.
#[derive(Clone)]
pub struct ProviderRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

impl ProviderRegistry {
    pub fn new(engine: Engine) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                engine,
                providers: HashMap::new(),
                digest_to_id: HashMap::new(),
            })),
        }
    }

    /// Register a `dynlink-provider`-world wasm component under `id`, compiling
    /// it now (instantiation is deferred to first resolve).
    pub fn register_provider(
        &self,
        id: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Result<(), String> {
        self.register_provider_with_preopens(id, path, Vec::new())
    }

    /// Register a `dynlink-provider`-world wasm component under `id`, with
    /// directories preopened into its OWN store on materialization. A pylon
    /// provider passes its `/lib` and `/app` here so the resident interpreter
    /// can import them; a plain provider passes an empty list.
    pub fn register_provider_with_preopens(
        &self,
        id: impl Into<String>,
        path: impl Into<PathBuf>,
        preopens: Vec<ProviderPreopen>,
    ) -> Result<(), String> {
        let id = id.into();
        let path = path.into();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let component = Component::from_binary(&inner.engine, &bytes)
            .map_err(|e| format!("compile provider {}: {e}", path.display()))?;
        inner.providers.insert(
            id,
            ProviderEntry {
                component,
                resident: None,
                path,
                preopens,
                handle_count: 0,
            },
        );
        Ok(())
    }

    /// Map a content digest to a previously-registered id (so
    /// `resolve-by-digest` can reuse the same resident provider).
    pub fn register_digest(&self, digest: Vec<u8>, id: impl Into<String>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.digest_to_id.insert(digest, id.into());
    }

    /// How many resident (instantiated-once) providers exist for `id` (0 or 1).
    /// Used by the integration test to assert ONE instance backs N resolves.
    pub fn resident_count(&self, id: &str) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .providers
            .get(id)
            .map(|e| usize::from(e.resident.is_some()))
            .unwrap_or(0)
    }

    /// How many live `instance` handles point at `id`'s resident provider.
    pub fn handle_count(&self, id: &str) -> u64 {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.providers.get(id).map(|e| e.handle_count).unwrap_or(0)
    }
}

/// The opaque handle a [`ResidentBackend`] remembers for each resolved
/// `instance`. It does NOT own the provider — the resident provider lives in
/// the shared registry. The handle just remembers WHICH id it resolved.
#[derive(Clone)]
pub struct ResidentHandle {
    id: String,
    registry: ProviderRegistry,
}

/// The resident provider lifecycle: instantiate a registered provider ONCE
/// (lazily, on first resolve) into a single resident store, and reuse it
/// across every resolve/invoke — "one heavy provider serving many function
/// components". Wraps a [`ProviderRegistry`].
#[derive(Clone)]
pub struct ResidentBackend {
    registry: ProviderRegistry,
}

impl ResidentBackend {
    pub fn new(registry: ProviderRegistry) -> Self {
        Self { registry }
    }
}

impl ProviderBackend for ResidentBackend {
    type Handle = ResidentHandle;

    fn resolve_by_id(&self, id: &str) -> Result<Self::Handle, Error> {
        materialize_resident(&self.registry, id)?;
        Ok(ResidentHandle {
            id: id.to_string(),
            registry: self.registry.clone(),
        })
    }

    fn resolve_by_digest(&self, d: &[u8]) -> Result<Self::Handle, Error> {
        let id = {
            let inner = self.registry.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.digest_to_id.get(d).cloned()
        };
        match id {
            Some(id) => self.resolve_by_id(&id),
            None => Err(err(
                ErrorCode::NotImplemented,
                "resolve-by-digest: no digest->id mapping registered (use register_digest)",
            )),
        }
    }

    fn invoke(&self, handle: &Self::Handle, method: &str, payload: &[u8]) -> Result<Vec<u8>, Error> {
        invoke_resident(&handle.registry, &handle.id, method, payload)
    }

    fn on_drop(&self, handle: &Self::Handle) {
        let mut inner = handle.registry.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = inner.providers.get_mut(&handle.id) {
            entry.handle_count = entry.handle_count.saturating_sub(1);
        }
    }
}

/// Materialize (instantiate ONCE, then reuse) the resident provider for `id`.
fn materialize_resident(registry: &ProviderRegistry, id: &str) -> Result<(), Error> {
    let mut inner = registry.inner.lock().unwrap_or_else(|e| e.into_inner());
    let RegistryInner {
        engine, providers, ..
    } = &mut *inner;
    let entry = providers
        .get_mut(id)
        .ok_or_else(|| err(ErrorCode::InvalidInput, format!("unknown provider id: {id}")))?;
    if entry.resident.is_none() {
        let mut linker: Linker<ProviderState> = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| err(ErrorCode::EmitLinkError, format!("provider wasi linker: {e}")))?;
        // Build the provider's OWN WASI ctx, preopening any registered dirs (a
        // pylon needs /lib = CPython Lib+numpy and /app = dispatcher) so the
        // resident interpreter can import them. inherit_stdio surfaces the
        // provider's init markers to the host stderr.
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stdio();
        for po in &entry.preopens {
            builder
                .preopened_dir(&po.host, &po.guest, DirPerms::all(), FilePerms::all())
                .map_err(|e| {
                    err(
                        ErrorCode::InvalidInput,
                        format!(
                            "provider '{id}': preopen {} -> {}: {e}",
                            po.host.display(),
                            po.guest
                        ),
                    )
                })?;
        }
        let state = ProviderState {
            wasi: builder.build(),
            table: WasiResourceTable::new(),
        };
        let mut store = Store::new(engine, state);
        let instance =
            provider::DynlinkProvider::instantiate(&mut store, &entry.component, &linker)
                .map_err(|e| err(ErrorCode::ExecTrap, format!("instantiate provider '{id}': {e:?}")))?;
        entry.resident = Some(ResidentProvider { store, instance });
        eprintln!(
            "[compose-dynlink] resident provider '{id}' instantiated ONCE from {} (shared across resolves)",
            entry.path.display()
        );
    } else {
        eprintln!("[compose-dynlink] resolve '{id}' reuses the existing resident provider (1 instance)");
    }
    entry.handle_count += 1;
    Ok(())
}

/// Drive the SHARED resident provider's `endpoint.handle`.
fn invoke_resident(
    registry: &ProviderRegistry,
    id: &str,
    method: &str,
    payload: &[u8],
) -> Result<Vec<u8>, Error> {
    let mut inner = registry.inner.lock().unwrap_or_else(|e| e.into_inner());
    let entry = inner
        .providers
        .get_mut(id)
        .ok_or_else(|| err(ErrorCode::InvalidInput, format!("provider '{id}' gone")))?;
    let resident = entry
        .resident
        .as_mut()
        .ok_or_else(|| err(ErrorCode::InternalError, format!("provider '{id}' not resident")))?;
    let endpoint = resident.instance.compose_dynlink_endpoint();
    let result = endpoint
        .call_handle(&mut resident.store, method, payload)
        .map_err(|e| err(ErrorCode::ExecTrap, format!("provider '{id}' handle trapped: {e:?}")))?;
    result.map_err(lower_provider_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A trivial in-process backend that needs no wasm: it uppercases the
    /// payload for method `"upper"` and counts live handles. Proves the
    /// store-generic bridge machinery (resource table push/get/delete,
    /// resolve/invoke/drop routing) independent of any wasm provider.
    #[derive(Clone, Default)]
    struct EchoBackend {
        live: Arc<AtomicU64>,
    }

    #[derive(Clone)]
    struct EchoHandle {
        id: String,
        live: Arc<AtomicU64>,
    }

    impl ProviderBackend for EchoBackend {
        type Handle = EchoHandle;

        fn resolve_by_id(&self, id: &str) -> Result<Self::Handle, Error> {
            self.live.fetch_add(1, Ordering::SeqCst);
            Ok(EchoHandle {
                id: id.to_string(),
                live: self.live.clone(),
            })
        }

        fn resolve_by_digest(&self, _d: &[u8]) -> Result<Self::Handle, Error> {
            Err(err(ErrorCode::NotImplemented, "no digest map"))
        }

        fn invoke(
            &self,
            handle: &Self::Handle,
            method: &str,
            payload: &[u8],
        ) -> Result<Vec<u8>, Error> {
            match method {
                "upper" => Ok(String::from_utf8_lossy(payload).to_uppercase().into_bytes()),
                "id" => Ok(handle.id.clone().into_bytes()),
                other => Err(err(ErrorCode::InvalidInput, format!("unknown method {other}"))),
            }
        }

        fn on_drop(&self, handle: &Self::Handle) {
            handle.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn bridge_routes_resolve_invoke_drop() {
        let backend = EchoBackend::default();
        let live = backend.live.clone();
        let mut bridge = DynLinkBridge::new(backend);

        let handle = bridge.resolve_by_id("prov".to_string()).expect("resolve");
        assert_eq!(live.load(Ordering::SeqCst), 1, "one live handle after resolve");

        let upper = bridge
            .invoke(
                Resource::new_own(handle.rep()),
                "upper".to_string(),
                b"hello from dlopen".to_vec(),
            )
            .expect("invoke upper");
        assert_eq!(upper, b"HELLO FROM DLOPEN");

        let id = bridge
            .invoke(
                Resource::new_own(handle.rep()),
                "id".to_string(),
                Vec::new(),
            )
            .expect("invoke id");
        assert_eq!(id, b"prov");

        bridge.drop_handle(handle).expect("drop");
        assert_eq!(live.load(Ordering::SeqCst), 0, "handle released on drop");
    }

    #[test]
    fn resident_backend_reports_digest_unmapped() {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("engine");
        let registry = ProviderRegistry::new(engine);
        let backend = ResidentBackend::new(registry);
        match backend.resolve_by_digest(&[0u8; 32]) {
            Err(e) => assert!(matches!(e.code, ErrorCode::NotImplemented)),
            Ok(_) => panic!("unmapped digest must not resolve"),
        }
    }
}
